//! # `spark_hir`
//!
//! HIR lowering for the Spark compiler (Phase 6).
//!
//! This crate lowers the typed AST into a desugared High-Level IR. The HIR
//! is still tree-structured (not a control flow graph) but has all syntactic
//! sugar removed and every expression annotated with its type.
//!
//! ## Desugaring
//!
//! The following constructs are desugared during lowering:
//!
//! | Source | Target |
//! |--------|--------|
//! | `(expr)` | `expr` |
//! | `block expr` | `await expr` |
//! | `x += y` | `x = x + y` |
//! | `a ?? b` | `match a { v -> v, Null -> b }` |
//! | `a?.f` | `match a { v -> Some(v.f), Null -> Null }` |
//! | `e!` | `match e { v -> v, err -> return err }` |
//!
//! ## Quick Start
//!
//! ```ignore
//! use spark_hir::{lower, HirInput};
//!
//! let input = HirInput {
//!     resolved: resolver_output.resolved,
//!     definitions: resolver_output.definitions,
//!     table: type_output.table,
//! };
//!
//! let output = lower(input);
//!
//! if output.diagnostics.has_errors() {
//!     // Handle errors
//! } else {
//!     // Use output.hir for subsequent phases
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::cast_possible_truncation)]

pub mod desugar;
pub mod hir;
pub mod lower;
pub mod lower_expr;
pub mod lower_pattern;
pub mod lower_stmt;

use spark_resolve::{DefinitionTable, ResolvedAst};
use spark_types::{TypeId, TypeTable};

/// Lowers a type expression to a [`TypeId`] by looking it up in the type table.
///
/// During HIR lowering all types have already been resolved by the type checker,
/// so we just look up the type by the expression's span. Falls back to
/// [`TypeId::ERROR`] if no type was recorded.
pub(crate) fn lower_type_expr(ty: &spark_ast::TypeExpr, table: &TypeTable) -> TypeId {
    table.get_expr_type(ty.span).unwrap_or(TypeId::ERROR)
}

pub use hir::{
    Hir, HirExpr, HirExprKind, HirFunction, HirLiteral, HirMatchArm, HirParam, HirPattern,
    HirStatic, HirStmt,
};

/// Input to the HIR lowering pass.
#[derive(Debug)]
pub struct HirInput {
    /// The resolved AST with name resolution mappings.
    pub resolved: ResolvedAst,
    /// Table of all definitions from the resolver.
    pub definitions: DefinitionTable,
    /// The type table from the type checker.
    pub table: TypeTable,
}

/// Output from the HIR lowering pass.
#[derive(Debug)]
pub struct HirOutput {
    /// The lowered HIR.
    pub hir: Hir,
    /// The type table (passed through from input).
    pub table: TypeTable,
    /// Diagnostics accumulated during lowering.
    pub diagnostics: spark_diagnostics::Diagnostics,
}

/// Lowers the typed AST into HIR.
///
/// This is the main entry point for HIR lowering. It takes the resolver
/// and type checker outputs and produces a desugared [`Hir`].
#[must_use]
pub fn lower(input: HirInput) -> HirOutput {
    let lowerer = lower::Lowerer::new(
        &input.table,
        &input.definitions,
        &input.resolved,
    );
    let (hir, diagnostics) = lowerer.lower_all();

    HirOutput {
        hir,
        table: input.table,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use spark_modules::ModuleGraph;

    #[test]
    fn empty_input() {
        let input = HirInput {
            resolved: ResolvedAst::new(HashMap::new()),
            definitions: DefinitionTable::new(),
            table: TypeTable::new(),
        };

        let output = lower(input);
        assert!(!output.diagnostics.has_errors());
        assert!(output.hir.functions.is_empty());
        assert!(output.hir.statics.is_empty());
    }

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

    /// Helper to run the full pipeline: lex -> parse -> resolve -> type-check -> lower.
    fn lower_source(source: &str) -> HirOutput {
        use spark_lexer::Lexer;
        use spark_parser::parse;
        use spark_resolve::{resolve, ResolverInput};
        use spark_span::FileId;
        use spark_types::{check, TypeCheckerInput};

        let file_id = FileId::new(0);

        // First pass: resolve + type-check
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

        // Second pass: re-resolve for HIR (type checker consumed ASTs)
        let tokens2 = Lexer::new(file_id, source).tokenize();
        let (ast2, _) = parse(tokens2, file_id);
        let graph2 = make_graph(file_id, &ast2);
        let mut asts2 = HashMap::new();
        asts2.insert(file_id, ast2);

        let resolver_output2 = resolve(ResolverInput { graph: graph2, asts: asts2 });

        lower(HirInput {
            resolved: resolver_output2.resolved,
            definitions: resolver_output2.definitions,
            table: type_output.table,
        })
    }

    #[test]
    fn lower_simple_function() {
        let output = lower_source("fn main(): Void { }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        assert_eq!(output.hir.functions.len(), 1);
    }

    #[test]
    fn lower_function_with_body() {
        let output = lower_source("fn add(a: Int, b: Int): Int { a + b }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        assert_eq!(output.hir.functions.len(), 1);
    }

    #[test]
    fn lower_const() {
        let output = lower_source("const MAX: Int = 100");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        assert_eq!(output.hir.statics.len(), 1);
    }

    #[test]
    fn lower_if_else() {
        let output = lower_source("fn f(x: Bool): Int { if x { 1 } else { 2 } }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn lower_struct_init() {
        let output = lower_source(
            "struct Point { x: Int, y: Int }\nfn make(): Point { Point { x: 1, y: 2 } }",
        );
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn lower_bool_literal() {
        let output = lower_source("fn f(): Bool { true }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn lower_forward_reference() {
        let output = lower_source("fn a(): Int { b() }\nfn b(): Int { 42 }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        assert_eq!(output.hir.functions.len(), 2);
    }

    #[test]
    fn lower_null_coalesce() {
        let output = lower_source("fn f(x: Int?): Int { x ?? 0 }");
        assert!(
            !output.diagnostics.has_errors(),
            "Errors: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        // The body should contain a Match (desugared from ??)
    }
}
