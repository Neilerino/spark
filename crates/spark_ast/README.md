# spark_ast

Source-shaped syntax nodes carry spans for diagnostics. The type table uses [node IDs](src/expr.rs) so nodes sharing a span can still have distinct types. See [the AST root](src/ast.rs) for declarations and [the visitor](src/visitor.rs) for traversal.

The [compiler guide](../../docs/compiler-architecture.md#compilation-stages) shows where the parser produces the AST and where later phases consume it.
