//! Full compilation pipeline: source code to native object bytes.
//!
//! Chains all 10 compiler phases with early exit on errors and diagnostic
//! rendering via ariadne.

#![allow(clippy::module_name_repetitions)]

use std::collections::HashMap;
use std::path::PathBuf;

use spark_ast::Ast;
use spark_borrow::BorrowInput;
use spark_codegen::{CodegenInput, TargetSpec};
use spark_diagnostics::Diagnostics;
use spark_hir::HirInput;
use spark_lexer::Lexer;
use spark_mir::MirInput;
use spark_modules::ModuleResolverInput;
use spark_parser::parse;
use spark_resolve::ResolverInput;
use spark_span::{FileId, SourceMap};
use spark_types::TypeCheckerInput;

/// Input to the compilation pipeline.
pub struct CompileInput {
    /// All source files: `(path_relative_to_root, source_text)`.
    pub sources: Vec<(PathBuf, String)>,
    /// Project root directory (absolute).
    pub root_path: PathBuf,
    /// Optimization level (`None` to skip all passes).
    pub opt_level: spark_opt::OptLevel,
}

/// Successful output from the compilation pipeline.
pub struct CompileOutput {
    /// The compiled native object file bytes (`.o` / `.obj`).
    pub object_bytes: Vec<u8>,
}

/// Runs the full compilation pipeline from source code to object bytes.
///
/// Supports multi-file projects: all `.spark` source files under the project
/// root are lexed, parsed, and fed into the module resolver together. The
/// downstream phases (type checking, HIR, MIR, borrow, opt, codegen) operate
/// on the resolved/typed IR and do not need per-file changes.
///
/// Returns `Ok(CompileOutput)` on success or `Err(())` if compilation errors
/// were encountered (diagnostics will have been emitted to stderr).
///
/// # Errors
///
/// Returns `Err(())` if any phase produces errors. Diagnostics are emitted
/// to stderr before returning.
#[allow(clippy::too_many_lines)]
pub fn compile(input: CompileInput) -> Result<CompileOutput, ()> {
    let CompileInput {
        sources,
        root_path,
        opt_level,
    } = input;

    // -- Source registration --------------------------------------------------
    let mut source_map = SourceMap::new();
    // Keep (FileId, relative_path, source_text) for later phases.
    let mut file_entries: Vec<(FileId, PathBuf, String)> = Vec::with_capacity(sources.len());

    // Inject standard library sources first so they're available as modules.
    for stdlib_src in spark_stdlib::stdlib_sources() {
        let file_id = source_map.add_file(stdlib_src.path, stdlib_src.source);
        file_entries.push((file_id, PathBuf::from(stdlib_src.path), stdlib_src.source.to_string()));
    }

    for (rel_path, source) in sources {
        let display_name = rel_path.to_string_lossy().to_string();
        let file_id = source_map.add_file(&display_name, &source);
        file_entries.push((file_id, rel_path, source));
    }

    // -- Phase 1-2: Lex + Parse all files ------------------------------------
    let mut asts: HashMap<FileId, Ast> = HashMap::with_capacity(file_entries.len());
    let mut file_paths: HashMap<FileId, PathBuf> = HashMap::with_capacity(file_entries.len());
    let mut had_parse_errors = false;

    for &(file_id, ref rel_path, ref source) in &file_entries {
        let tokens = Lexer::new(file_id, source).tokenize();
        let (ast, parse_diags) = parse(tokens, file_id);

        if parse_diags.has_errors() {
            parse_diags.emit_all(&source_map);
            had_parse_errors = true;
        }
        emit_warnings(&parse_diags, &source_map);

        asts.insert(file_id, ast);
        file_paths.insert(file_id, rel_path.clone());
    }

    if had_parse_errors {
        return Err(());
    }

    // -- Phase 3: Module resolution -------------------------------------------
    let module_output = spark_modules::resolve(ModuleResolverInput {
        asts: asts.clone(),
        root_path,
        file_paths,
    });
    if module_output.diagnostics.has_errors() {
        module_output.diagnostics.emit_all(&source_map);
        return Err(());
    }
    emit_warnings(&module_output.diagnostics, &source_map);
    let graph = module_output.graph;

    // -- Phase 4: Name resolution (pass 1) ------------------------------------
    let resolve1 = spark_resolve::resolve(ResolverInput {
        graph: graph.clone(),
        asts,
    });
    if resolve1.diagnostics.has_errors() {
        resolve1.diagnostics.emit_all(&source_map);
        return Err(());
    }
    emit_warnings(&resolve1.diagnostics, &source_map);

    // -- Phase 5: Type checking -----------------------------------------------
    // Clone the resolved ASTs before type checking so that HIR lowering can
    // reuse the same AST nodes (and their ExprIds) that the type checker
    // recorded types against. This avoids re-parsing which would create new
    // nodes with different ExprIds.
    let resolved_for_hir = resolve1.resolved.clone();
    let definitions_for_hir = resolve1.definitions.clone();

    let type_output = spark_types::check(TypeCheckerInput {
        resolved: resolve1.resolved,
        definitions: resolve1.definitions,
        graph,
    });
    if type_output.diagnostics.has_errors() {
        type_output.diagnostics.emit_all(&source_map);
        return Err(());
    }
    emit_warnings(&type_output.diagnostics, &source_map);
    let table = type_output.table;

    // -- Phase 6: HIR lowering ------------------------------------------------
    let hir_output = spark_hir::lower(HirInput {
        resolved: resolved_for_hir,
        definitions: definitions_for_hir,
        table,
    });
    if hir_output.diagnostics.has_errors() {
        hir_output.diagnostics.emit_all(&source_map);
        return Err(());
    }
    emit_warnings(&hir_output.diagnostics, &source_map);

    let definitions = hir_output.definitions;

    // -- Phase 9: MIR construction + monomorphization ---------------------------
    let mir_output = spark_mir::lower(MirInput {
        hir: hir_output.hir,
        table: hir_output.table,
        definitions,
    });
    if mir_output.diagnostics.has_errors() {
        mir_output.diagnostics.emit_all(&source_map);
        return Err(());
    }
    emit_warnings(&mir_output.diagnostics, &source_map);

    // Use definitions from MIR output (may contain monomorphized entries).
    let definitions = mir_output.definitions;

    // -- Phase 10: Borrow checking --------------------------------------------
    let borrow_output = spark_borrow::check(BorrowInput {
        mir: mir_output.mir,
        table: mir_output.table,
        definitions: definitions.clone(),
    });
    if borrow_output.diagnostics.has_errors() {
        borrow_output.diagnostics.emit_all(&source_map);
        return Err(());
    }
    emit_warnings(&borrow_output.diagnostics, &source_map);

    // -- Phase 10.25: Async state machine transform ----------------------------
    let async_output = spark_mir::async_transform::transform(
        spark_mir::async_transform::AsyncTransformInput {
            mir: borrow_output.mir,
            table: borrow_output.table,
            definitions: definitions.clone(),
        },
    );
    emit_warnings(&async_output.diagnostics, &source_map);
    let definitions = async_output.definitions;

    // Save poll_fn_map before optimizer consumes the async output.
    let poll_fn_map = async_output.poll_fn_map;

    // -- Phase 10.5: Optimization ---------------------------------------------
    let opt_output = spark_opt::optimize(spark_opt::OptInput {
        mir: async_output.mir,
        table: async_output.table,
        definitions: definitions.clone(),
        opt_level,
    });
    emit_warnings(&opt_output.diagnostics, &source_map);

    // -- Phase 11: Code generation --------------------------------------------
    let codegen_output = spark_codegen::generate(CodegenInput {
        mir: opt_output.mir,
        table: opt_output.table,
        definitions,
        target: TargetSpec::host(),
        poll_fn_map,
    });
    if codegen_output.diagnostics.has_errors() {
        codegen_output.diagnostics.emit_all(&source_map);
        return Err(());
    }
    emit_warnings(&codegen_output.diagnostics, &source_map);

    Ok(CompileOutput {
        object_bytes: codegen_output.object_bytes,
    })
}

/// Emits warnings (but not errors) from a diagnostics set.
fn emit_warnings(diags: &Diagnostics, source_map: &SourceMap) {
    if diags.has_warnings() {
        for diag in diags.iter() {
            if diag.is_warning() {
                diag.emit(source_map);
            }
        }
    }
}
