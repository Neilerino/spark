# `spark_parser`

Recursive descent parser for the Spark programming language.

## Overview

This crate transforms a token stream from `spark_lexer` into an Abstract Syntax Tree (AST) defined in `spark_ast`. It implements:

- **Pratt parsing** for expressions with proper operator precedence (14 levels)
- **Recursive descent** for statements, declarations, types, and patterns
- **Error recovery** to report multiple errors per file
- **Span tracking** for all AST nodes

## Quick Start

```rust
use spark_lexer::Lexer;
use spark_parser::parse;
use spark_span::FileId;

let source = "fn main(): Void { }";
let file_id = FileId::new(0);
let mut lexer = Lexer::new(file_id, source);
let tokens = lexer.tokenize();

let (ast, diagnostics) = parse(tokens, file_id);

assert!(!diagnostics.has_errors());
assert_eq!(ast.items.len(), 1);
```

## Architecture

The parser is organized into modules by syntactic category:

```
src/
├── lib.rs       # Public API, Parser struct, token navigation
├── expr.rs      # Expression parsing (Pratt algorithm)
├── stmt.rs      # Statement parsing
├── decl.rs      # Declaration parsing (fn, struct, enum, etc.)
├── types.rs     # Type expression parsing
└── pattern.rs   # Pattern parsing for match expressions
```

### Expression Parsing

Expressions use Pratt parsing (precedence climbing) with binding powers:

| Level | Operators | Binding Power |
|-------|-----------|---------------|
| 1 | `..`, `..=` (range) | 1-2 |
| 2 | `??` (coalesce) | 3-4 |
| 3 | `or` | 5-6 |
| 4 | `and` | 7-8 |
| 5 | `==`, `!=` | 9-10 |
| 6 | `<`, `>`, `<=`, `>=`, `is`, `as` | 11-12 |
| 7 | `\|` (bitwise or) | 13-14 |
| 8 | `^` (xor) | 15-16 |
| 9 | `&` (bitwise and) | 17-18 |
| 10 | `<<`, `>>` | 19-20 |
| 11 | `+`, `-` | 21-22 |
| 12 | `*`, `/`, `%` | 23-24 |
| 13 | `-`, `not`, `~` (prefix) | 25 |
| 14 | `!`, `?`, `.`, `?.`, `[]`, `()` (postfix) | 27 |

### Error Recovery

When an unexpected token is encountered, the parser:

1. Emits a diagnostic with the error location
2. Returns an error node (`ExprKind::Error`, `TypeExprKind::Error`, etc.)
3. Synchronizes to a known recovery point (semicolon, brace, or keyword)

This allows reporting multiple syntax errors per file.

## Supported Syntax

### Declarations

- **Functions**: `[pub] [async] fn name[T](params): Type { body }`
- **Structs**: `[pub] struct Name[T] { fields }`
- **Enums**: `[pub] enum Name[T] [: RawType] { variants }`
- **Interfaces**: `[pub] interface Name[T] [extends ...] { methods }`
- **Type aliases**: `[pub] alias Name[T] = Type`
- **Type definitions**: `[pub] type Name[T] = Type`
- **Constants**: `[pub] const NAME: Type = value`
- **Bindings**: `Type binds Interface { methods }`
- **Module/Import**: `module name`, `import path { items }`

### Expressions

- Literals: integers, floats, strings, chars, booleans, null
- Identifiers and paths
- Binary and unary operators
- Field access and method calls
- Index access
- Control flow: if, match, loop
- Collections: lists, tuples, maps
- Lambdas: `fn(params): Type { body }`
- Async: await, spawn, block

### Types

- Named types: `Int`, `List[T]`
- Optional: `Type?`
- Mutable: `Type!`
- Union: `A | B | C`
- Function: `(Params): Return`
- Structural: `{ field: Type }`

### Patterns

- Wildcard: `_`
- Identifier: `x`, `x: Type`
- Literal: `42`, `"hello"`
- Variant: `Enum.Variant(fields)`
- Struct: `Type { field, field: pattern, .. }`
- Tuple: `(a, b, c)`
- List: `[first, ...rest]`
- Or: `A | B`
- Guard: `pattern if condition`

## Design Decisions

### Why Pratt Parsing?

Pratt parsing (precedence climbing) handles operator precedence elegantly without requiring separate parsing functions for each precedence level. It's simple to extend with new operators and handles both left and right associativity.

### Why Separate `parse_expr_no_struct`?

Some contexts (like match scrutinees and if conditions) should not consume a `{` as struct initialization. The `parse_expr_no_struct` method stops before brace expressions, allowing the caller to parse the block separately.

### Error Nodes vs Panicking

The parser never panics on invalid input. Instead, it creates error nodes (`ExprKind::Error`, etc.) and continues parsing. This allows:

- Reporting all syntax errors in a file
- Providing better IDE support (partial ASTs)
- Graceful degradation in tooling

## Testing

The parser has comprehensive tests for each parsing function:

```bash
cargo test -p spark_parser
```

Tests cover:
- All expression types
- All declaration types
- All pattern types
- Error recovery scenarios
- Edge cases (empty input, EOF handling)
