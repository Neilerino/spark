//! # `spark_resolve`
//!
//! Name resolution for the Spark compiler (Phase 4).
//!
//! This crate resolves identifiers to their definitions, building a mapping
//! from AST nodes to `DefId`s. It transforms unresolved ASTs into resolved
//! ASTs where every identifier reference is linked to its definition.
//!
//! ## Responsibilities
//!
//! - **Build scope hierarchy**: Construct nested scopes for modules, functions, blocks
//! - **Define top-level items**: Register functions, structs, enums, interfaces, consts
//! - **Resolve identifier references**: Map every identifier use to its `DefId`
//! - **Resolve type references**: Resolve all type names to their definitions
//! - **Detect undefined names**: Report errors for references to undefined identifiers
//! - **Detect duplicate definitions**: Report redeclarations within the same scope
//! - **Handle shadowing**: Allow locals to shadow outer-scope names
//! - **Manage imports**: Track imported names and their visibility
//!
//! ## Quick Start
//!
//! ```ignore
//! use spark_resolve::{resolve, ResolverInput};
//! use spark_modules::ModuleGraph;
//! use std::collections::HashMap;
//!
//! let input = ResolverInput {
//!     graph: module_graph,
//!     asts: parsed_files,
//! };
//!
//! let output = resolve(input);
//!
//! if output.diagnostics.has_errors() {
//!     // Handle errors
//! } else {
//!     // Use output.definitions and output.resolved for type checking
//! }
//! ```
//!
//! ## Two-Pass Resolution
//!
//! Resolution happens in two passes:
//!
//! 1. **Collection Pass**: Collect all top-level definitions (functions, structs, etc.)
//!    This enables forward references between items.
//!
//! 2. **Resolution Pass**: Resolve all identifier references within function bodies,
//!    type expressions, and patterns.
//!
//! ## Relationship to Other Crates
//!
//! ```text
//! spark_modules (produces ModuleGraph)
//!       │
//!       ▼
//! spark_resolve (resolves names) ◄── THIS CRATE
//!       │
//!       ▼
//! spark_types (type checking)
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::cast_possible_truncation)]

mod def;
mod imports;
mod resolved_ast;
mod resolver;
mod rib;
mod scope;

pub use def::{DefId, DefinitionInfo, DefinitionKind, DefinitionTable, Visibility};
pub use imports::{ImportBinding, ImportScope};
pub use resolved_ast::{ResolutionMap, ResolvedAst};
pub use rib::{Rib, RibKind, RibStack};
pub use scope::{Scope, ScopeArena, ScopeId, ScopeKind};

use std::collections::HashMap;

use spark_ast::Ast;
use spark_diagnostics::Diagnostics;
use spark_modules::ModuleGraph;
use spark_span::FileId;

/// Input to the name resolver.
#[derive(Debug)]
pub struct ResolverInput {
    /// The module dependency graph from `spark_modules`.
    pub graph: ModuleGraph,
    /// All parsed ASTs indexed by file ID.
    pub asts: HashMap<FileId, Ast>,
}

/// Output from the name resolver.
#[derive(Debug)]
pub struct ResolverOutput {
    /// The resolved AST with `DefId` mappings.
    pub resolved: ResolvedAst,
    /// Table of all definitions.
    pub definitions: DefinitionTable,
    /// Diagnostics accumulated during resolution.
    pub diagnostics: Diagnostics,
}

/// Resolves names in all ASTs.
///
/// This is the main entry point for name resolution. It takes the module
/// graph and parsed ASTs, and produces a resolved AST with all identifiers
/// mapped to their definitions.
///
/// # Example
///
/// ```ignore
/// use spark_resolve::{resolve, ResolverInput};
///
/// let input = ResolverInput {
///     graph: module_graph,
///     asts: parsed_files,
/// };
///
/// let output = resolve(input);
/// assert!(!output.diagnostics.has_errors());
/// ```
#[must_use]
pub fn resolve(input: ResolverInput) -> ResolverOutput {
    let mut resolver = resolver::Resolver::new(input);
    resolver.resolve();
    resolver.into_output()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_modules::ModuleGraph;

    #[test]
    fn empty_input() {
        let input = ResolverInput {
            graph: ModuleGraph::default(),
            asts: HashMap::new(),
        };

        let output = resolve(input);

        assert!(!output.diagnostics.has_errors());
        // Built-in functions (e.g., print) are always registered
        assert!(output.definitions.len() >= 1);
    }
}
