//! Full compilation pipeline: source code to native object bytes.
//!
//! Chains all 10 compiler phases with early exit on errors and diagnostic
//! rendering via ariadne.

#![allow(clippy::module_name_repetitions)]

use std::collections::HashMap;
use std::path::PathBuf;

use spark_borrow::BorrowInput;
use spark_codegen::{CodegenInput, TargetSpec};
use spark_diagnostics::Diagnostics;
use spark_hir::HirInput;
use spark_lexer::Lexer;
use spark_mir::MirInput;
use spark_modules::{ModuleResolverInput, ModuleResolverOutput};
use spark_parser::parse;
use spark_resolve::{ResolverInput, ResolverOutput};
use spark_span::{FileId, SourceMap};
use spark_types::TypeCheckerInput;

/// Input to the compilation pipeline.
pub struct CompileInput {
    /// The source code text.
    pub source: String,
    /// The file name for diagnostics (e.g., `"hello.spark"`).
    pub file_name: String,
}

/// Successful output from the compilation pipeline.
pub struct CompileOutput {
    /// The compiled native object file bytes (`.o` / `.obj`).
    pub object_bytes: Vec<u8>,
}

/// Runs the full compilation pipeline from source code to object bytes.
///
/// Returns `Ok(CompileOutput)` on success or `Err(())` if compilation errors
/// were encountered (diagnostics will have been emitted to stderr).
///
/// # Errors
///
/// Returns `Err(())` if any phase produces errors. Diagnostics are emitted
/// to stderr before returning.
pub fn compile(input: CompileInput) -> Result<CompileOutput, ()> {
    let CompileInput { source, file_name } = input;

    // -- Source registration --------------------------------------------------
    let mut source_map = SourceMap::new();
    let file_id = source_map.add_file(&file_name, &source);

    // -- Phase 1: Lex ---------------------------------------------------------
    let tokens = Lexer::new(file_id, &source).tokenize();

    // -- Phase 2: Parse -------------------------------------------------------
    let (ast, parse_diags) = parse(tokens, file_id);
    if parse_diags.has_errors() {
        parse_diags.emit_all(&source_map);
        return Err(());
    }
    emit_warnings(&parse_diags, &source_map);

    // -- Phase 3: Module resolution -------------------------------------------
    let module_output = resolve_modules(file_id, ast.clone(), &file_name);
    if module_output.diagnostics.has_errors() {
        module_output.diagnostics.emit_all(&source_map);
        return Err(());
    }
    emit_warnings(&module_output.diagnostics, &source_map);
    let graph = module_output.graph;

    // -- Phase 4: Name resolution (pass 1) ------------------------------------
    let resolve1 = resolve_names(graph.clone(), file_id, ast);
    if resolve1.diagnostics.has_errors() {
        resolve1.diagnostics.emit_all(&source_map);
        return Err(());
    }
    emit_warnings(&resolve1.diagnostics, &source_map);

    // -- Phase 5: Type checking -----------------------------------------------
    let type_output = spark_types::check(TypeCheckerInput {
        resolved: resolve1.resolved,
        definitions: resolve1.definitions,
        graph: graph.clone(),
    });
    if type_output.diagnostics.has_errors() {
        type_output.diagnostics.emit_all(&source_map);
        return Err(());
    }
    emit_warnings(&type_output.diagnostics, &source_map);
    let table = type_output.table;

    // -- Phase 6-7: Re-lex, re-parse, name resolution (pass 2) ---------------
    // The type checker consumed the first AST. We re-parse to get a fresh AST
    // for HIR lowering.
    let tokens2 = Lexer::new(file_id, &source).tokenize();
    let (ast2, _) = parse(tokens2, file_id);

    let resolve2 = resolve_names(graph, file_id, ast2);
    if resolve2.diagnostics.has_errors() {
        resolve2.diagnostics.emit_all(&source_map);
        return Err(());
    }

    // -- Phase 8: HIR lowering ------------------------------------------------
    let hir_output = spark_hir::lower(HirInput {
        resolved: resolve2.resolved,
        definitions: resolve2.definitions.clone(),
        table,
    });
    if hir_output.diagnostics.has_errors() {
        hir_output.diagnostics.emit_all(&source_map);
        return Err(());
    }
    emit_warnings(&hir_output.diagnostics, &source_map);

    // -- Phase 9: MIR construction --------------------------------------------
    let mir_output = spark_mir::lower(MirInput {
        hir: hir_output.hir,
        table: hir_output.table,
        definitions: resolve2.definitions.clone(),
    });
    if mir_output.diagnostics.has_errors() {
        mir_output.diagnostics.emit_all(&source_map);
        return Err(());
    }
    emit_warnings(&mir_output.diagnostics, &source_map);

    // -- Phase 10: Borrow checking --------------------------------------------
    let borrow_output = spark_borrow::check(BorrowInput {
        mir: mir_output.mir,
        table: mir_output.table,
        definitions: resolve2.definitions.clone(),
    });
    if borrow_output.diagnostics.has_errors() {
        borrow_output.diagnostics.emit_all(&source_map);
        return Err(());
    }
    emit_warnings(&borrow_output.diagnostics, &source_map);

    // -- Phase 11: Code generation --------------------------------------------
    let codegen_output = spark_codegen::generate(CodegenInput {
        mir: borrow_output.mir,
        table: borrow_output.table,
        definitions: resolve2.definitions,
        target: TargetSpec::host(),
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

/// Sets up a single-file module graph.
///
/// For single-file programs we create a synthetic root module containing just
/// the one file. Multi-file compilation will be added later.
fn resolve_modules(
    file_id: FileId,
    ast: spark_ast::Ast,
    file_name: &str,
) -> ModuleResolverOutput {
    let mut asts = HashMap::new();
    asts.insert(file_id, ast);

    let mut file_paths = HashMap::new();
    file_paths.insert(file_id, PathBuf::from(file_name));

    spark_modules::resolve(ModuleResolverInput {
        asts,
        root_path: PathBuf::from("."),
        file_paths,
    })
}

/// Runs name resolution on a single-file module.
fn resolve_names(
    graph: spark_modules::ModuleGraph,
    file_id: FileId,
    ast: spark_ast::Ast,
) -> ResolverOutput {
    let mut asts = HashMap::new();
    asts.insert(file_id, ast);

    spark_resolve::resolve(ResolverInput { graph, asts })
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
