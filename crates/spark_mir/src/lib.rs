//! # `spark_mir`
//!
//! MIR construction for the Spark compiler (Phase 7).
//!
//! This crate lowers tree-structured HIR into a control-flow-graph-based
//! Mid-Level IR. Each function becomes a collection of basic blocks with
//! explicit terminators, all intermediate values held in locals/temporaries,
//! and control flow expressed as edges between blocks.
//!
//! ## Quick Start
//!
//! ```ignore
//! use spark_mir::{lower, MirInput};
//!
//! let input = MirInput {
//!     hir: hir_output.hir,
//!     table: hir_output.table,
//!     definitions: resolver_output.definitions,
//! };
//!
//! let output = lower(input);
//!
//! if output.diagnostics.has_errors() {
//!     // Handle errors
//! } else {
//!     // Use output.mir for subsequent phases
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::cast_possible_truncation)]

pub mod async_transform;
pub mod builder;
pub mod lower_expr;
pub mod lower_function;
pub mod lower_pattern;
pub mod lower_stmt;
pub mod mir;
pub mod monomorphize;
pub mod pretty;

use spark_diagnostics::Diagnostics;
use spark_hir::Hir;
use spark_resolve::DefinitionTable;
use spark_types::TypeTable;

pub use mir::{
    AggregateKind, BasicBlock, BlockId, Constant, Local, LocalDecl, Mir, MirFunction, Operand,
    Place, Projection, Rvalue, Statement, Terminator,
};
pub use pretty::pretty_function;

/// Input to the MIR lowering pass.
#[derive(Debug)]
pub struct MirInput {
    /// The HIR from the previous phase.
    pub hir: Hir,
    /// The type table (read-only).
    pub table: TypeTable,
    /// Definition table from the resolver.
    pub definitions: DefinitionTable,
}

/// Output from the MIR lowering pass.
#[derive(Debug)]
pub struct MirOutput {
    /// The lowered MIR.
    pub mir: Mir,
    /// The type table (passed through, may have new types from monomorphization).
    pub table: TypeTable,
    /// Definition table (may have new entries from monomorphization).
    pub definitions: DefinitionTable,
    /// Diagnostics accumulated during lowering.
    pub diagnostics: Diagnostics,
}

/// Lowers HIR into MIR.
///
/// This is the main entry point for MIR construction. It takes the HIR
/// and type table and produces a CFG-based [`Mir`].
#[must_use]
pub fn lower(input: MirInput) -> MirOutput {
    let MirInput {
        hir,
        mut table,
        mut definitions,
    } = input;

    let mut functions = std::collections::HashMap::new();
    let diagnostics;

    {
        let mut mir_builder = builder::MirBuilder::new(&table);

        for (def_id, hir_fn) in &hir.functions {
            let mir_fn = lower_function::lower_function(
                hir_fn,
                &mut mir_builder,
                &table,
                &definitions,
            );
            functions.insert(*def_id, mir_fn);
        }

        diagnostics = mir_builder.diagnostics;
    }

    let mut mir = Mir { functions };

    // Run monomorphization to instantiate generic functions with concrete types.
    monomorphize::monomorphize(&mut mir, &mut table, &mut definitions);

    MirOutput {
        mir,
        table,
        definitions,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use spark_modules::ModuleGraph;

    /// Builds a module graph for a single file's AST.
    fn make_graph(file_id: spark_span::FileId, ast: &spark_ast::Ast) -> ModuleGraph {
        use spark_ast::Item;
        use spark_modules::{ModuleInfo, ModulePath};

        let module_path = ModulePath::from_single(spark_intern::sym("test"));
        let mut graph = ModuleGraph::default();
        let mut module_info = ModuleInfo::new(module_path.clone());
        module_info.files.push(file_id);
        for item in &ast.items {
            let name = match item {
                Item::Function(f) => Some(f.name.name),
                Item::Struct(s) => Some(s.name.name),
                Item::Enum(e) => Some(e.name.name),
                Item::Interface(i) => Some(i.name.name),
                Item::TypeAlias(t) => Some(t.name.name),
                Item::TypeDef(t) => Some(t.name.name),
                Item::Const(c) => Some(c.name.name),
                Item::Binding(_) => None,
            };
            if let Some(n) = name {
                module_info.exports.insert(n);
            }
        }
        graph.modules.insert(module_path.clone(), module_info);
        graph.compilation_order.push(module_path);
        graph
    }

    /// Full pipeline: lex → parse → resolve → type-check → HIR → MIR.
    fn lower_source(source: &str) -> MirOutput {
        use spark_lexer::Lexer;
        use spark_parser::parse;
        use spark_resolve::{resolve, ResolverInput};
        use spark_span::FileId;
        use spark_types::{check, TypeCheckerInput};

        let file_id = FileId::new(0);

        // Pass 1: resolve + type-check
        let tokens = Lexer::new(file_id, source).tokenize();
        let (ast, _) = parse(tokens, file_id);
        let graph = make_graph(file_id, &ast);
        let mut asts = HashMap::new();
        asts.insert(file_id, ast);

        let resolver_output = resolve(ResolverInput { graph, asts });
        let type_output = check(TypeCheckerInput {
            resolved: resolver_output.resolved,
            definitions: resolver_output.definitions,
            graph: ModuleGraph::default(),
        });

        // Pass 2: re-resolve for HIR (type checker consumed ASTs)
        let tokens2 = Lexer::new(file_id, source).tokenize();
        let (ast2, _) = parse(tokens2, file_id);
        let graph2 = make_graph(file_id, &ast2);
        let mut asts2 = HashMap::new();
        asts2.insert(file_id, ast2);

        let resolver_output2 = resolve(ResolverInput {
            graph: graph2,
            asts: asts2,
        });

        let hir_output = spark_hir::lower(spark_hir::HirInput {
            resolved: resolver_output2.resolved,
            definitions: resolver_output2.definitions,
            table: type_output.table,
        });

        lower(MirInput {
            hir: hir_output.hir,
            table: hir_output.table,
            definitions: hir_output.definitions,
        })
    }

    // ── Basic tests ──────────────────────────────────────────────

    #[test]
    fn empty_input() {
        let input = MirInput {
            hir: Hir {
                functions: HashMap::new(),
                statics: HashMap::new(),
            },
            table: TypeTable::new(),
            definitions: DefinitionTable::new(),
        };
        let output = lower(input);
        assert!(!output.diagnostics.has_errors());
        assert!(output.mir.functions.is_empty());
    }

    #[test]
    fn lower_void_function() {
        let output = lower_source("fn main(): Void { }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
        assert_eq!(output.mir.functions.len(), 1);

        let func = output.mir.functions.values().next().unwrap();
        // Should have at least: return place + entry block
        assert!(!func.locals.is_empty());
        assert!(!func.blocks.is_empty());

        let pp = pretty_function(func);
        assert!(pp.contains("return"), "MIR:\n{pp}");
    }

    #[test]
    fn lower_return_literal() {
        let output = lower_source("fn f(): Int { return 42 }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
        let func = output.mir.functions.values().next().unwrap();
        let pp = pretty_function(func);
        assert!(pp.contains("const 42"), "MIR:\n{pp}");
        assert!(pp.contains("return"), "MIR:\n{pp}");
    }

    #[test]
    fn lower_binary_op() {
        let output = lower_source("fn add(a: Int, b: Int): Int { return a + b }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
        let func = output.mir.functions.values().next().unwrap();
        assert_eq!(func.param_count, 2);
        let pp = pretty_function(func);
        assert!(pp.contains("+"), "MIR:\n{pp}");
    }

    #[test]
    fn lower_if_else() {
        let output = lower_source("fn f(x: Bool): Int { return if x { 1 } else { 2 } }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
        let func = output.mir.functions.values().next().unwrap();
        let pp = pretty_function(func);
        // Should have switchInt for the if condition
        assert!(pp.contains("switchInt"), "MIR:\n{pp}");
        assert!(pp.contains("const 1"), "MIR:\n{pp}");
        assert!(pp.contains("const 2"), "MIR:\n{pp}");
    }

    #[test]
    fn lower_let_binding() {
        let output = lower_source("fn f(): Int { x: Int = 10\nreturn x }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
        let func = output.mir.functions.values().next().unwrap();
        let pp = pretty_function(func);
        assert!(pp.contains("const 10"), "MIR:\n{pp}");
    }

    #[test]
    fn lower_function_call() {
        let output = lower_source("fn id(x: Int): Int { return x }\nfn main(): Int { return id(42) }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
        assert_eq!(output.mir.functions.len(), 2);

        // The main function should have a call terminator
        for func in output.mir.functions.values() {
            let pp = pretty_function(func);
            if pp.contains("call") {
                assert!(pp.contains("const 42"), "MIR:\n{pp}");
            }
        }
    }

    #[test]
    fn lower_struct_init() {
        let output = lower_source(
            "struct Point { x: Int, y: Int }\nfn make(): Point { return Point { x: 1, y: 2 } }",
        );
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn lower_loop_and_break() {
        let output = lower_source("fn f(): Void { loop { break } }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
        let func = output.mir.functions.values().next().unwrap();
        let pp = pretty_function(func);
        // Should have goto terminators for loop structure
        assert!(pp.contains("goto"), "MIR:\n{pp}");
    }

    #[test]
    fn lower_early_return() {
        let output = lower_source("fn f(x: Bool): Int { if x { return 1 }\nreturn 2 }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
        let func = output.mir.functions.values().next().unwrap();
        let pp = pretty_function(func);
        // Should have two return terminators
        let return_count = pp.matches("return").count();
        assert!(
            return_count >= 2,
            "Expected at least 2 returns, got {return_count}. MIR:\n{pp}"
        );
    }

    #[test]
    fn lower_multiple_functions() {
        let output = lower_source("fn a(): Int { return 1 }\nfn b(): Int { return 2 }\nfn c(): Int { return 3 }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
        assert_eq!(output.mir.functions.len(), 3);
    }

    #[test]
    fn lower_unary_op() {
        let output = lower_source("fn neg(x: Int): Int { return -x }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
        let func = output.mir.functions.values().next().unwrap();
        let pp = pretty_function(func);
        assert!(pp.contains("-"), "MIR:\n{pp}");
    }

    #[test]
    fn lower_bool_literal() {
        let output = lower_source("fn f(): Bool { return true }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
        let func = output.mir.functions.values().next().unwrap();
        let pp = pretty_function(func);
        assert!(pp.contains("const true"), "MIR:\n{pp}");
    }

    #[test]
    fn lower_string_literal() {
        let output = lower_source("fn f(): String { return \"hello\" }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn lower_if_without_else() {
        let output = lower_source("fn f(x: Bool): Void { if x { } }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn pretty_display_trait() {
        let output = lower_source("fn f(): Int { return 42 }");
        let func = output.mir.functions.values().next().unwrap();
        // Display should produce the same as pretty_function
        let display = format!("{func}");
        let pretty = pretty_function(func);
        assert_eq!(display, pretty);
    }

    // === Union/Optional operator end-to-end through MIR ===

    #[test]
    fn mir_null_coalesce() {
        let output = lower_source("fn f(x: Int?): Int { return x ?? 0 }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        let func = output.mir.functions.values().next().unwrap();
        let pp = pretty_function(func);
        // Should contain a switch (from match desugaring) and the literal 0
        assert!(pp.contains("switch") || pp.contains("goto"), "Expected branch in MIR:\n{pp}");
    }

    #[test]
    fn mir_optional_field() {
        let output = lower_source(
            "struct User { name: String }\nfn f(u: User?): String? { return u?.name }",
        );
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        assert_eq!(output.mir.functions.len(), 1);
    }

    #[test]
    fn mir_propagate_optional() {
        let output = lower_source("fn f(x: Int?): Int? { return x! }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        let func = output.mir.functions.values().next().unwrap();
        let pp = pretty_function(func);
        // Propagate desugars to match, which creates branches
        assert!(pp.contains("return"), "Expected return in MIR:\n{pp}");
    }

    #[test]
    fn mir_coalesce_string() {
        let output = lower_source("fn f(x: String?): String { return x ?? \"default\" }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        assert_eq!(output.mir.functions.len(), 1);
    }

    // ── `is` type test MIR lowering ─────────────────────────────

    #[test]
    fn is_union_member() {
        let output = lower_source(
            "fn check(x: Int | String): Bool { return x is Int }",
        );
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        let func = output.mir.functions.values().next().unwrap();
        let pp = pretty_function(func);
        assert!(pp.contains("discriminant"), "Expected discriminant check in MIR:\n{pp}");
        assert!(pp.contains("switchInt"), "Expected switchInt in MIR:\n{pp}");
        assert!(pp.contains("const true"), "Expected const true branch in MIR:\n{pp}");
        assert!(pp.contains("const false"), "Expected const false branch in MIR:\n{pp}");
    }

    #[test]
    fn is_optional_null() {
        let output = lower_source(
            "fn check(x: Int?): Bool { return x is Null }",
        );
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        let func = output.mir.functions.values().next().unwrap();
        let pp = pretty_function(func);
        assert!(pp.contains("discriminant"), "Expected discriminant check in MIR:\n{pp}");
        assert!(pp.contains("switchInt"), "Expected switchInt in MIR:\n{pp}");
    }

    #[test]
    fn is_primitive_same_type() {
        let output = lower_source(
            "fn check(x: Int): Bool { return x is Int }",
        );
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        let func = output.mir.functions.values().next().unwrap();
        let pp = pretty_function(func);
        // Same type known at compile time — should emit `const true`
        assert!(pp.contains("const true"), "Expected const true for same-type is check:\n{pp}");
    }

    #[test]
    fn is_enum_variant() {
        let output = lower_source(
            "enum Status { Active, Inactive }\nfn check(s: Status): Bool { return s is Status.Active }",
        );
        // This may produce errors if `is` on enum variants isn't fully resolved;
        // the key assertion is that the lowering doesn't panic
        let func = output.mir.functions.values().next();
        if let Some(func) = func {
            let pp = pretty_function(func);
            // Should contain either discriminant check or const bool
            assert!(
                pp.contains("discriminant") || pp.contains("const true") || pp.contains("const false"),
                "Expected discriminant or const bool in MIR:\n{pp}"
            );
        }
    }

    #[test]
    fn is_error_recovery() {
        // Use an undefined type to trigger an error type — lowering should not panic
        let output = lower_source(
            "fn check(x: Int): Bool { return x is Undefined }",
        );
        // Should have errors but not panic
        let func = output.mir.functions.values().next();
        if let Some(func) = func {
            let pp = pretty_function(func);
            // Error type should result in const false
            assert!(pp.contains("const false"), "Expected const false for error type:\n{pp}");
        }
    }
}
