# `spark_ast`

Abstract Syntax Tree definitions for the Spark compiler.

## Responsibility

This crate defines the AST node types produced by the parser. The AST represents the syntactic structure of Spark programs before any semantic analysis (name resolution, type checking, etc.).

## Core Types

| Type | Purpose |
|------|---------|
| `Ast` | Complete AST for a single source file |
| `Item` | Top-level declaration (function, struct, enum, etc.) |
| `Expr` | Expression node with ~35 variants |
| `Stmt` | Statement node (let, assign, expr) |
| `TypeExpr` | Type expression (named, union, optional, etc.) |
| `Pattern` | Pattern for match expressions |
| `Ident` | Identifier with source span |
| `Path` | Multi-segment path (e.g., `module.Type`) |
| `Literal` | Literal values (int, float, string, etc.) |

## Design Principles

- **Data only**: AST nodes are pure data structures with no methods beyond basic constructors
- **Spans everywhere**: Every node tracks its source location via `Span`
- **Interned identifiers**: Names are represented as `Symbol` for efficiency
- **Flat structure**: Nodes own their children directly (no arena allocation at this phase)

## Relationship to Other Crates

```
spark_span          (provides Span)
      │
      ▼
spark_intern        (provides Symbol)
      │
      ▼
spark_ast ◄─────────  YOU ARE HERE
      │
      ▼
spark_parser        (produces AST from tokens)
      │
      ▼
spark_resolve       (resolves names in AST)
```

## Example

```rust
use spark_ast::{Ast, Item, FunctionDef};

// The parser produces an Ast
let ast: Ast = parser.parse();

// Iterate over top-level items
for item in &ast.items {
    match item {
        Item::Function(f) => println!("Found function: {:?}", f.name),
        Item::Struct(s) => println!("Found struct: {:?}", s.name),
        _ => {}
    }
}
```

## Visitor Pattern

The crate provides a `Visitor` trait for traversing the AST:

```rust
use spark_ast::{Visitor, Ast, Expr, walk_ast};

struct MyVisitor {
    expr_count: usize,
}

impl Visitor for MyVisitor {
    fn visit_expr(&mut self, expr: &Expr) {
        self.expr_count += 1;
        spark_ast::walk_expr(self, expr);
    }
}

let mut visitor = MyVisitor { expr_count: 0 };
visitor.visit_ast(&ast);
println!("Found {} expressions", visitor.expr_count);
```

## Features

- `serde` - Enables serialization/deserialization of AST nodes (useful for debugging)
