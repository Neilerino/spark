//! # `spark_types`
//!
//! Type checking for the Spark compiler (Phase 5).
//!
//! This crate consumes the resolver output ([`ResolvedAst`], [`DefinitionTable`],
//! [`ModuleGraph`]) and produces a [`TypeTable`] mapping every expression and
//! definition to its type.
//!
//! ## Quick Start
//!
//! ```ignore
//! use spark_types::{check, TypeCheckerInput};
//!
//! let input = TypeCheckerInput {
//!     resolved: resolver_output.resolved,
//!     definitions: resolver_output.definitions,
//!     graph: module_graph,
//! };
//!
//! let output = check(input);
//!
//! if output.diagnostics.has_errors() {
//!     // Handle errors
//! } else {
//!     // Use output.table for subsequent phases
//! }
//! ```
//!
//! ## Two-Pass Architecture
//!
//! Type checking happens in two passes:
//!
//! 1. **Collection Pass**: Walk all declarations and register their type
//!    signatures. This enables forward references between functions.
//!
//! 2. **Checking Pass**: Walk all function bodies, type-check expressions,
//!    verify assignments, and record `Span → TypeId` mappings.
//!
//! ## Relationship to Other Crates
//!
//! ```text
//! spark_resolve (produces ResolvedAst, DefinitionTable)
//!       │
//!       ▼
//! spark_types (type checking) ◄── THIS CRATE
//!       │
//!       ▼
//! spark_hir (typed intermediate representation)
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::cast_possible_truncation)]

pub mod builtins;
pub mod check_decl;
pub mod check_expr;
pub mod check_stmt;
pub mod checker;
pub mod exhaustive;
pub mod intern;
pub mod lower;
pub mod table;
pub mod ty;
pub mod union;

use spark_modules::ModuleGraph;
use spark_resolve::{DefinitionTable, ResolvedAst};

pub use check_expr::type_name;
pub use checker::TypeCheckerOutput;
pub use table::TypeTable;
pub use ty::{FloatWidth, IntWidth, LifetimeId, Ty, TypeId};

/// Input to the type checker.
#[derive(Debug)]
pub struct TypeCheckerInput {
    /// The resolved AST with `DefId` mappings.
    pub resolved: ResolvedAst,
    /// Table of all definitions.
    pub definitions: DefinitionTable,
    /// The module dependency graph.
    pub graph: ModuleGraph,
}

/// Runs type checking on the resolved AST.
///
/// This is the main entry point for the type checker. It takes the resolver's
/// output and produces a [`TypeTable`] mapping definitions and expressions
/// to their types.
///
/// # Example
///
/// ```ignore
/// use spark_types::{check, TypeCheckerInput};
///
/// let input = TypeCheckerInput {
///     resolved: resolver_output.resolved,
///     definitions: resolver_output.definitions,
///     graph: module_graph,
/// };
///
/// let output = check(input);
/// assert!(!output.diagnostics.has_errors());
/// ```
#[must_use]
pub fn check(input: TypeCheckerInput) -> TypeCheckerOutput {
    let checker = checker::TypeChecker::new(
        input.resolved,
        input.definitions,
        input.graph,
    );
    checker.check()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_modules::ModuleGraph;
    use std::collections::HashMap;

    #[test]
    fn empty_input() {
        let input = TypeCheckerInput {
            resolved: ResolvedAst::new(HashMap::new()),
            definitions: DefinitionTable::new(),
            graph: ModuleGraph::default(),
        };

        let output = check(input);
        assert!(!output.diagnostics.has_errors());
    }

    /// Helper to run the full pipeline: lex → parse → resolve → type-check.
    fn check_source(source: &str) -> TypeCheckerOutput {
        use spark_ast::Item;
        use spark_lexer::Lexer;
        use spark_modules::{ModuleInfo, ModulePath};
        use spark_parser::parse;
        use spark_resolve::{resolve, ResolverInput};
        use spark_span::FileId;

        let file_id = FileId::new(0);
        let tokens = Lexer::new(file_id, source).tokenize();
        let (ast, _parse_diags) = parse(tokens, file_id);

        // Set up a proper module graph so the resolver processes this file
        let module_path = ModulePath::from_single(spark_intern::sym("test"));
        let mut graph = ModuleGraph::default();
        let mut module_info = ModuleInfo::new(module_path.clone());
        module_info.files.push(file_id);

        // Mark all items as exported for testing
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

        let mut asts = HashMap::new();
        asts.insert(file_id, ast);

        let resolver_input = ResolverInput { graph, asts };
        let resolver_output = resolve(resolver_input);

        let type_input = TypeCheckerInput {
            resolved: resolver_output.resolved,
            definitions: resolver_output.definitions,
            graph: ModuleGraph::default(),
        };

        check(type_input)
    }

    #[test]
    fn literal_types() {
        let output = check_source("fn main(): Void { x: Int = 42 }");
        // Should type-check without errors
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn function_return_type() {
        let output = check_source("fn get_num(): Int { return 42 }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn function_return_type_mismatch() {
        let output = check_source("fn get_num(): Int { return true }");
        assert!(output.diagnostics.has_errors());
    }

    #[test]
    fn binary_arithmetic() {
        let output = check_source("fn add(a: Int, b: Int): Int { return a + b }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn binary_comparison_returns_bool() {
        let output = check_source("fn cmp(a: Int, b: Int): Bool { return a > b }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn let_binding_type_mismatch() {
        let output = check_source("fn main(): Void { x: String = 42 }");
        assert!(output.diagnostics.has_errors());
    }

    #[test]
    fn if_condition_must_be_bool() {
        let output = check_source("fn main(): Void { if 42 { } }");
        assert!(output.diagnostics.has_errors());
    }

    #[test]
    fn struct_field_access() {
        let output = check_source(
            "struct User { name: String }\nfn get_name(u: User): String { return u.name }",
        );
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn struct_init() {
        let output = check_source(
            "struct Point { x: Int, y: Int }\nfn make(): Point { return Point { x: 1, y: 2 } }",
        );
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn forward_reference() {
        let output = check_source(
            "fn a(): Int { return b() }\nfn b(): Int { return 42 }",
        );
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn wrong_argument_count() {
        let output = check_source(
            "fn add(a: Int, b: Int): Int { return a + b }\nfn main(): Void { add(1) }",
        );
        assert!(output.diagnostics.has_errors());
    }

    #[test]
    fn wrong_argument_type() {
        let output = check_source(
            "fn greet(name: String): Void { }\nfn main(): Void { greet(42) }",
        );
        assert!(output.diagnostics.has_errors());
    }

    #[test]
    fn bool_literal() {
        let output = check_source("fn main(): Bool { return true }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn void_function() {
        let output = check_source("fn main(): Void { }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn null_literal() {
        let output = check_source("fn main(): Void { x: Int? = Null }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn optional_type_compatible_with_value() {
        let output = check_source("fn main(): Void { x: Int? = 42 }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn logical_operators() {
        let output = check_source("fn test(a: Bool, b: Bool): Bool { return a and b }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn unary_negation() {
        let output = check_source("fn neg(x: Int): Int { return -x }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn unary_not() {
        let output = check_source("fn flip(x: Bool): Bool { return not x }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn const_definition() {
        let output = check_source("const MAX: Int = 100");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn const_type_mismatch() {
        let output = check_source("const MAX: Int = true");
        assert!(output.diagnostics.has_errors());
    }

    #[test]
    fn missing_return_is_error() {
        let output = check_source("fn f(): Int { 42 }");
        assert!(output.diagnostics.has_errors());
        let msgs: Vec<_> = output.diagnostics.iter().map(|d| &d.message).collect();
        assert!(
            msgs.iter().any(|m| m.contains("missing `return`")),
            "Expected 'missing return' diagnostic, got: {msgs:?}"
        );
    }

    #[test]
    fn void_function_no_return_ok() {
        let output = check_source("fn f(): Void { }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn explicit_return_on_all_paths() {
        let output = check_source("fn f(x: Bool): Int { if x { return 1 } else { return 2 } }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    // === Coalesce (??) ===

    #[test]
    fn coalesce_optional_int() {
        let output = check_source("fn f(x: Int?): Int { return x ?? 0 }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn coalesce_non_optional_is_error() {
        let output = check_source("fn f(x: Int): Int { return x ?? 0 }");
        assert!(output.diagnostics.has_errors());
    }

    #[test]
    fn coalesce_optional_string() {
        let output = check_source("fn f(x: String?): String { return x ?? \"default\" }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    // === Optional field (?.) ===

    #[test]
    fn optional_field_access() {
        let output = check_source(
            "struct User { name: String }\nfn f(u: User?): String? { return u?.name }",
        );
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    // === Propagate (!) ===

    #[test]
    fn propagate_optional() {
        let output = check_source("fn f(x: Int?): Int? { return x! }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    // === Union types ===

    #[test]
    fn union_type_annotation() {
        let output = check_source("fn f(): Void { x: Int | String = 42 }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn union_type_with_null() {
        let output = check_source("fn f(): Void { x: Int | Null = Null }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn coalesce_union_with_null() {
        // Int | Null should work with ?? just like Int?
        let output = check_source("fn f(x: Int | Null): Int { return x ?? 0 }");
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    // === Propagation with expected type from binding ===

    #[test]
    fn propagate_union_split_by_binding_type() {
        // The binding annotation `Int` tells propagation that Int is the success
        // type, so String is propagated to the function return type.
        let output = check_source(
            "fn get(): Int | String { return 42 }\n\
             fn caller(): String { x: Int = get()!\n return \"done\" }",
        );
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn propagate_union_split_by_binding_union() {
        // Binding expects Int | String, so Bool is the propagated error type.
        let output = check_source(
            "fn get(): Int | String | Bool { return 42 }\n\
             fn caller(): Bool { x: Int | String = get()!\n return true }",
        );
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    // ── Match exhaustiveness (full pipeline) ────────────────────

    #[test]
    fn match_enum_with_wildcard() {
        let output = check_source(
            "enum Color { Red, Green, Blue }\n\
             fn f(c: Color): Int {\n\
                 return match c {\n\
                     Color.Red -> 1,\n\
                     _ -> 0,\n\
                 }\n\
             }",
        );
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn match_enum_all_variants_exhaustive() {
        let output = check_source(
            "enum Color { Red, Green, Blue }\n\
             fn f(c: Color): Int {\n\
                 return match c {\n\
                     Color.Red -> 1,\n\
                     Color.Green -> 2,\n\
                     Color.Blue -> 3,\n\
                 }\n\
             }",
        );
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn match_enum_missing_variant_error() {
        let output = check_source(
            "enum Color { Red, Green, Blue }\n\
             fn f(c: Color): Int {\n\
                 return match c {\n\
                     Color.Red -> 1,\n\
                     Color.Green -> 2,\n\
                 }\n\
             }",
        );
        assert!(output.diagnostics.has_errors());
        let msgs: Vec<_> = output.diagnostics.iter().map(|d| &d.message).collect();
        assert!(
            msgs.iter().any(|m| m.contains("non-exhaustive")),
            "Expected non-exhaustive error, got: {msgs:?}"
        );
    }

    #[test]
    fn match_optional_with_wildcard() {
        let output = check_source(
            "fn f(x: Int?): Int {\n\
                 return match x {\n\
                     _ -> 0,\n\
                 }\n\
             }",
        );
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn match_bool_exhaustive() {
        let output = check_source(
            "fn f(x: Bool): Int {\n\
                 return match x {\n\
                     true -> 1,\n\
                     false -> 0,\n\
                 }\n\
             }",
        );
        assert!(!output.diagnostics.has_errors(), "Errors: {:?}", output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>());
    }

    #[test]
    fn match_bool_non_exhaustive() {
        let output = check_source(
            "fn f(x: Bool): Int {\n\
                 return match x {\n\
                     true -> 1,\n\
                 }\n\
             }",
        );
        assert!(output.diagnostics.has_errors());
        let msgs: Vec<_> = output.diagnostics.iter().map(|d| &d.message).collect();
        assert!(
            msgs.iter().any(|m| m.contains("non-exhaustive")),
            "Expected non-exhaustive error, got: {msgs:?}"
        );
    }

    #[test]
    fn match_int_without_wildcard_error() {
        let output = check_source(
            "fn f(x: Int): Int {\n\
                 return match x {\n\
                     1 -> 10,\n\
                     2 -> 20,\n\
                 }\n\
             }",
        );
        assert!(output.diagnostics.has_errors());
        let msgs: Vec<_> = output.diagnostics.iter().map(|d| &d.message).collect();
        assert!(
            msgs.iter().any(|m| m.contains("non-exhaustive")),
            "Expected non-exhaustive error, got: {msgs:?}"
        );
    }
}
