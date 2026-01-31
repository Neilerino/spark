//! # `spark_ast`
//!
//! Abstract Syntax Tree definitions for the Spark compiler.
//!
//! This crate defines the AST node types produced by the parser. The AST
//! represents the syntactic structure of Spark programs before any semantic
//! analysis (name resolution, type checking, etc.).
//!
//! ## Design Principles
//!
//! - **Data only**: AST nodes are pure data structures with no methods beyond
//!   basic constructors and predicates
//! - **Spans everywhere**: Every node tracks its source location via `Span`
//! - **Interned identifiers**: Names are represented as `Symbol` for efficiency
//! - **Flat structure**: Nodes own their children directly (no arena allocation
//!   at this phase)
//!
//! ## Core Types
//!
//! | Type | Purpose |
//! |------|---------|
//! | [`Ast`] | Complete AST for a single source file |
//! | [`Item`] | Top-level declaration (function, struct, enum, etc.) |
//! | [`Expr`] | Expression node with ~35 variants |
//! | [`Stmt`] | Statement node (let, assign, expr) |
//! | [`TypeExpr`] | Type expression (named, union, optional, etc.) |
//! | [`Pattern`] | Pattern for match expressions |
//! | [`Ident`] | Identifier with source span |
//! | [`Path`] | Multi-segment path (e.g., `module.Type`) |
//! | [`Literal`] | Literal values (int, float, string, etc.) |
//!
//! ## Example
//!
//! ```ignore
//! use spark_ast::{Ast, Item, Visitor, walk_ast};
//!
//! // The parser produces an Ast
//! let ast: Ast = parser.parse();
//!
//! // Iterate over top-level items
//! for item in &ast.items {
//!     match item {
//!         Item::Function(f) => println!("Found function: {:?}", f.name),
//!         Item::Struct(s) => println!("Found struct: {:?}", s.name),
//!         _ => {}
//!     }
//! }
//!
//! // Or use the visitor pattern
//! struct MyVisitor;
//! impl Visitor for MyVisitor {
//!     fn visit_expr(&mut self, expr: &Expr) {
//!         println!("Found expression at {:?}", expr.span);
//!         spark_ast::walk_expr(self, expr);
//!     }
//! }
//! ```
//!
//! ## Relationship to Other Crates
//!
//! ```text
//! spark_span          (provides Span)
//!       │
//!       ▼
//! spark_intern        (provides Symbol)
//!       │
//!       ▼
//! spark_ast ◄─────────  YOU ARE HERE
//!       │
//!       ▼
//! spark_parser        (produces AST from tokens)
//!       │
//!       ▼
//! spark_resolve       (resolves names in AST)
//! ```
//!
//! ## Features
//!
//! - `serde` - Enables serialization/deserialization of AST nodes

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod ast;
mod decl;
mod expr;
mod lit;
mod path;
mod pattern;
mod stmt;
mod types;
mod visitor;

// Re-export all public types
pub use ast::*;
pub use decl::*;
pub use expr::*;
pub use lit::*;
pub use path::*;
pub use pattern::*;
pub use stmt::*;
pub use types::*;
pub use visitor::*;
