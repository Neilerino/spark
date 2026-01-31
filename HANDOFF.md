# Spark Compiler - Technical Handoff

## Project Overview

Spark is a statically-typed programming language ("lower-level Python") with:
- Immutability by default
- Union types for error handling (no exceptions)
- Opt-in borrow checking for performance-critical code
- Clean, Python-like syntax

## Completed Work

### Foundation Crates (5/10 compiler phases)

| Crate | Purpose | Tests | Status |
|-------|---------|-------|--------|
| `spark_span` | Source locations (`FileId`, `Span`, `SourceMap`) | 13 | ✅ Complete |
| `spark_intern` | String interning (`Symbol`, `Interner`, pre-interned keywords) | 14 | ✅ Complete |
| `spark_diagnostics` | Error reporting with ariadne rendering | 12 | ✅ Complete |
| `spark_lexer` | Tokenization (keywords, literals, operators, comments) | 33 | ✅ Complete |
| `spark_ast` | AST node definitions for all language constructs | 53 | ✅ Complete |

**Total: 125 tests passing across workspace.**

All crates pass `cargo test` and `cargo clippy` (nursery warnings only).

### Project Structure

```
spark/
├── Cargo.toml                    # Workspace config
├── HANDOFF.md                    # This file
├── docs/
│   ├── lang-spec.md              # Language specification (READ THIS FIRST)
│   └── compiler-architecture.md  # Pipeline design with I/O schemas
└── crates/
    ├── spark_span/               # FileId, Span, SourceMap, Spanned<T>
    ├── spark_intern/             # Symbol, Interner, kw module (67 keywords)
    ├── spark_diagnostics/        # Diagnostic, Label, Diagnostics
    ├── spark_lexer/              # Lexer, Token, TokenKind (65 variants)
    └── spark_ast/                # AST nodes (NEW)
        ├── src/lib.rs            # Re-exports
        ├── src/path.rs           # Ident, Path
        ├── src/lit.rs            # Literal, LiteralKind, StringLiteral
        ├── src/types.rs          # TypeExpr, GenericParam, constraints
        ├── src/pattern.rs        # Pattern, PatternKind, FieldPattern
        ├── src/stmt.rs           # Block, Stmt, StmtKind
        ├── src/expr.rs           # Expr, ExprKind (~35 variants), BinaryOp, UnaryOp
        ├── src/decl.rs           # FunctionDef, StructDef, EnumDef, InterfaceDef, etc.
        ├── src/ast.rs            # Ast, Item
        └── src/visitor.rs        # Visitor trait, walk_* functions
```

### spark_ast Overview

The AST crate defines all node types for the Spark language:

**Top-level:**
- `Ast` - Complete file representation
- `Item` - Function, Struct, Enum, Interface, TypeAlias, TypeDef, Const, Binding

**Declarations:**
- `FunctionDef` - Functions with generics, params, decorators, async support
- `StructDef` / `StructField` - Struct definitions
- `EnumDef` / `EnumVariant` - Enums with unit, raw value, or associated data variants
- `InterfaceDef` - Interfaces with extends support
- `TypeAliasDef` / `TypeDefDef` - Type aliases and distinct types
- `ConstDef` - Constants
- `BindingDef` / `BindingTarget` - Interface bindings

**Expressions (~35 variants in `ExprKind`):**
- Literals, identifiers, paths
- Collections: List, Map, Set, Tuple, ListRange
- Operators: Binary, Unary
- Access: Field, OptionalField, Index, Call, MethodCall
- Instantiation: StructInit, EnumVariant
- Control flow: Block, If, Match, Loop, Break, Continue, Return
- Iteration: Each, EachVia, To (narrowing/pipeline)
- Optional handling: Coalesce, Propagate
- Type operations: Is, As
- Functions: Lambda, Partial
- Async: Await, BlockAwait, Spawn

**Types (`TypeExprKind`):**
- Named (with generics), Tuple, Function, Optional, Union, Mutable, Ref, Structural

**Patterns (`PatternKind`):**
- Wildcard, Ident, Literal, Variant, Struct, Tuple, List, TypeNarrowing, Or, Guard

**Visitor:**
- `Visitor` trait with default traversal implementations
- `walk_*` functions for each node type

## What's Next

### Immediate: spark_parser

Build a recursive descent parser that:
1. Consumes `Vec<Token>` from `spark_lexer`
2. Produces `Ast` from `spark_ast`
3. Collects `Diagnostics` for syntax errors
4. Implements error recovery to report multiple errors per file

**Key files to create:**
```
crates/spark_parser/
├── Cargo.toml
├── README.md
└── src/
    ├── lib.rs           # Public API
    ├── parser.rs        # Main parser struct and entry points
    ├── expr.rs          # Expression parsing (precedence climbing)
    ├── stmt.rs          # Statement parsing
    ├── decl.rs          # Declaration parsing (fn, struct, enum, etc.)
    ├── types.rs         # Type expression parsing
    └── pattern.rs       # Pattern parsing for match
```

**Operator Precedence (from architecture doc):**

| Level | Operators | Associativity |
|-------|-----------|---------------|
| 1 | `=`, `+=`, `-=`, `*=`, `/=` | Right |
| 2 | `??` | Left |
| 3 | `or` | Left |
| 4 | `and` | Left |
| 5 | `==`, `!=` | Left |
| 6 | `<`, `>`, `<=`, `>=`, `is`, `as` | Left |
| 7 | `\|` | Left |
| 8 | `^` | Left |
| 9 | `&` | Left |
| 10 | `<<`, `>>` | Left |
| 11 | `+`, `-` | Left |
| 12 | `*`, `/`, `%` | Left |
| 13 | `-` (unary), `not`, `~` | Right (prefix) |
| 14 | `!`, `?`, `.`, `?.`, `[]`, `()` | Left (postfix) |

### Full Pipeline (from compiler-architecture.md)

1. ✅ Lexer (spark_lexer)
2. ⏳ Parser (spark_parser) ← **NEXT**
3. ⏳ Module Resolution (spark_modules)
4. ⏳ Name Resolution (spark_resolve)
5. ⏳ Type Checking (spark_types)
6. ⏳ HIR Lowering (spark_hir)
7. ⏳ MIR Lowering (spark_mir)
8. ⏳ Borrow Checking (spark_borrow)
9. ⏳ Optimization (spark_opt)
10. ⏳ Code Generation (spark_codegen)

## Design Principles

- Each crate has a README.md explaining its responsibility
- Tests are inline (idiomatic Rust)
- All code passes `cargo clippy` with pedantic/nursery lints
- No unsafe code (`#![forbid(unsafe_code)]`)
- Hand-written lexer/parser for better error messages
- Data-only AST nodes (logic lives in visitors and later phases)

## Build Commands

```bash
cargo build              # Build all
cargo test               # Test all (125 tests)
cargo clippy             # Lint all
cargo test -p spark_ast  # Test specific crate
cargo doc --open         # View documentation
```

## Key Implementation Details

1. **Spans are u32 byte offsets** - `SourceMap` converts to line/column when needed
2. **Pre-interned keywords** - `spark_intern::kw` module has constants like `kw::FN`, `kw::STRUCT`
3. **TokenKind has 65 variants** - 4 literals, 31 keywords, 25 operators, 3 special
4. **ExprKind has ~35 variants** - Covers all expression forms in the language
5. **Visitor pattern** - Use `walk_*` functions to traverse AST nodes
6. **Error recovery** - `ExprKind::Error`, `TypeExprKind::Error`, `PatternKind::Error` for parser recovery

## Notes for Next Agent

1. **Read `docs/lang-spec.md`** before implementing the parser - it has the full grammar
2. **Use `spark_ast` types directly** - they're designed for easy construction
3. **Pre-interned keywords** in `kw` module are ready to compare with token symbols
4. **Consider error recovery** - the parser should continue after syntax errors
5. **Test with real Spark code** - create sample `.spark` files to parse
6. **The architecture doc** has detailed input/output schemas for each phase

## Crate Dependencies

```
spark_span          (no dependencies)
      │
spark_intern        (no dependencies)
      │
spark_diagnostics   (depends on spark_span)
      │
spark_lexer         (depends on span, intern, diagnostics)
      │
spark_ast           (depends on span, intern)
      │
spark_parser        (will depend on span, intern, diagnostics, lexer, ast)
```
