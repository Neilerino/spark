# Spark Compiler - Technical Handoff

## Project Overview

Spark is a statically-typed programming language ("lower-level Python") with:

- Immutability by default
- Union types for error handling (no exceptions)
- Opt-in borrow checking for performance-critical code
- Clean, Python-like syntax

## Completed Work

### Foundation Crates (7/10 compiler phases)

| Crate               | Purpose                                                        | Tests | Status      |
| ------------------- | -------------------------------------------------------------- | ----- | ----------- |
| `spark_span`        | Source locations (`FileId`, `Span`, `SourceMap`)               | 13    | ✅ Complete |
| `spark_intern`      | String interning (`Symbol`, `Interner`, pre-interned keywords) | 14    | ✅ Complete |
| `spark_diagnostics` | Error reporting with ariadne rendering                         | 12    | ✅ Complete |
| `spark_lexer`       | Tokenization (keywords, literals, operators, comments)         | 33    | ✅ Complete |
| `spark_ast`         | AST node definitions for all language constructs               | 53    | ✅ Complete |
| `spark_parser`      | Recursive descent parser with Pratt expression parsing         | 107   | ✅ Complete |
| `spark_modules`     | Module resolution, dependency graph, cycle detection           | 38    | ✅ Complete |

**Total: 270 tests passing across workspace.**

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
    ├── spark_ast/                # AST nodes
    │   ├── src/lib.rs            # Re-exports
    │   ├── src/path.rs           # Ident, Path
    │   ├── src/lit.rs            # Literal, LiteralKind, StringLiteral
    │   ├── src/types.rs          # TypeExpr, GenericParam, constraints
    │   ├── src/pattern.rs        # Pattern, PatternKind, FieldPattern
    │   ├── src/stmt.rs           # Block, Stmt, StmtKind
    │   ├── src/expr.rs           # Expr, ExprKind (~35 variants), BinaryOp, UnaryOp
    │   ├── src/decl.rs           # FunctionDef, StructDef, EnumDef, InterfaceDef, etc.
    │   ├── src/ast.rs            # Ast, Item
    │   └── src/visitor.rs        # Visitor trait, walk_* functions
    ├── spark_parser/             # Parser
    │   ├── README.md             # Design documentation
    │   └── src/
    │       ├── lib.rs            # Public API, Parser struct
    │       ├── expr.rs           # Expression parsing (Pratt algorithm)
    │       ├── stmt.rs           # Statement parsing
    │       ├── decl.rs           # Declaration parsing
    │       ├── types.rs          # Type expression parsing
    │       └── pattern.rs        # Pattern parsing
    └── spark_modules/            # Module resolution (NEW)
        ├── README.md             # Design documentation
        └── src/
            ├── lib.rs            # Public API, resolve() function
            ├── path.rs           # ModulePath type
            ├── discovery.rs      # Module discovery from ASTs
            ├── graph.rs          # ModuleGraph, cycle detection
            └── exports.rs        # Export collection utilities
```

### spark_parser Overview

The parser implements:

- **Pratt parsing** for expressions with 14 precedence levels
- **Recursive descent** for declarations, statements, types, and patterns
- **Error recovery** to report multiple syntax errors per file
- **Span tracking** for all AST nodes

**Key features:**

- `parse(tokens, file_id) -> (Ast, Diagnostics)` - Main entry point
- `parse_expr(tokens, file_id) -> (Expr, Diagnostics)` - For REPL/testing
- Error nodes (`ExprKind::Error`, etc.) allow continued parsing after errors
- `parse_expr_no_struct` for contexts where `{` starts a block, not struct init

**Operator Precedence:**

| Level | Operators                                 | Binding Power |
| ----- | ----------------------------------------- | ------------- |
| 1     | `..`, `..=` (range)                       | 1-2           |
| 2     | `??` (coalesce)                           | 3-4           |
| 3     | `or`                                      | 5-6           |
| 4     | `and`                                     | 7-8           |
| 5     | `==`, `!=`                                | 9-10          |
| 6     | `<`, `>`, `<=`, `>=`, `is`, `as`          | 11-12         |
| 7     | `\|` (bitwise or)                         | 13-14         |
| 8     | `^` (xor)                                 | 15-16         |
| 9     | `&` (bitwise and)                         | 17-18         |
| 10    | `<<`, `>>`                                | 19-20         |
| 11    | `+`, `-`                                  | 21-22         |
| 12    | `*`, `/`, `%`                             | 23-24         |
| 13    | `-`, `not`, `~` (prefix)                  | 25            |
| 14    | `!`, `?`, `.`, `?.`, `[]`, `()` (postfix) | 27            |

## Known Gaps (Non-Blocking)

The parser handles all core language constructs but has minor gaps for advanced async features:

1. **`spawn` blocks** - Parallel async execution: `spawn { await fetch_data() }`
   - Currently unimplemented
   - Used in test fixtures but parsing not yet supported
   - Low priority: can be added when implementing async runtime

2. **`block` keyword** - Blocking I/O marker: `block read_file(path)`
   - Currently unimplemented
   - Used to mark operations that should run on blocking thread pool
   - Low priority: syntax exists in fixtures, can be added with runtime

3. **Error propagation `!` operator** - Needs verification
   - Lexer has `!` token
   - Parser likely handles as postfix operator
   - Should verify it works correctly in all contexts: `await fetch()!`

These gaps do not block progress to the next phase (module resolution). They can be implemented when the async runtime design is finalized.

## What's Next

### Immediate: spark_resolve (NEXT MAIN TASK)

**Goal**: Build name resolution to bind identifiers to their definitions.

**Core Tasks**:

1. **Scope management** - Track nested scopes (modules, functions, blocks)
2. **Name binding** - Resolve identifiers to their declarations
3. **Symbol table** - Build a symbol table for all named items
4. **Import handling** - Resolve imported names using ModuleGraph
5. **Shadowing** - Handle variable shadowing correctly
6. **Error reporting** - Report undefined/duplicate names

**Key Design Decisions**:

- **Symbol representation**: How to represent resolved symbols (DefId?)
- **Scope structure**: How to model nested scopes efficiently
- **Deferred resolution**: Handle forward references in interfaces

**Reference**: See Phase 4 in `docs/compiler-architecture.md` for detailed requirements.

**Success Criteria**:

- Resolves all identifiers to their definitions
- Handles imports from ModuleGraph correctly
- Reports clear errors for undefined names
- Supports shadowing and nested scopes

### spark_modules Overview (COMPLETE)

The module resolver implements:

- **Module discovery** from `module` declarations
- **Using resolution** for files joining existing modules
- **Import resolution** with dependency edge tracking
- **Cycle detection** using Tarjan's SCC algorithm
- **Export collection** for public items
- **Topological sort** for compilation order

**Key features:**

- `resolve(input) -> ModuleResolverOutput` - Main entry point
- `ModuleGraph` with modules, dependencies, compilation order
- `ModulePath` for representing module hierarchies
- Comprehensive error messages for module issues

### Full Pipeline (from compiler-architecture.md)

1. ✅ Lexer (spark_lexer)
2. ✅ Parser (spark_parser)
3. ✅ Module Resolution (spark_modules)
4. ⏳ Name Resolution (spark_resolve) ← **NEXT**
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
cargo test               # Test all (270 tests)
cargo clippy             # Lint all
cargo test -p spark_modules  # Test specific crate
cargo doc --open         # View documentation
```

## Key Implementation Details

1. **Spans are u32 byte offsets** - `SourceMap` converts to line/column when needed
2. **Pre-interned keywords** - `spark_intern::kw` module has constants like `kw::FN`, `kw::STRUCT`
3. **TokenKind has 65 variants** - 4 literals, 31 keywords, 25 operators, 3 special
4. **ExprKind has ~35 variants** - Covers all expression forms in the language
5. **Visitor pattern** - Use `walk_*` functions to traverse AST nodes
6. **Error recovery** - `ExprKind::Error`, `TypeExprKind::Error`, `PatternKind::Error` for parser recovery
7. **Symbol::as_str()** - Use this to get the string representation of interned symbols

## Notes for Next Agent

1. **Read `docs/lang-spec.md`** - It has the full grammar and language semantics
2. **The parser is complete** - You can parse any valid Spark program into an AST
3. **Module resolution is complete** - `spark_modules` builds dependency graph from ASTs
4. **Error recovery is implemented** - All phases continue after errors
5. **The architecture doc** has detailed input/output schemas for each phase
6. **Use ModuleGraph** - Name resolution should consume the ModuleGraph from spark_modules

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
spark_parser        (depends on span, intern, diagnostics, lexer, ast)
      │
spark_modules       (depends on span, intern, diagnostics, ast)
      │
spark_resolve       (will depend on all above)
```
