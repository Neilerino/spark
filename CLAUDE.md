# CLAUDE.md - AI Assistant Guide for Spark Compiler

## Essential Reading (Start Here)

1. **[docs/lang-spec.md](docs/lang-spec.md)** - Complete language specification (2000+ lines)
2. **[docs/compiler-architecture.md](docs/compiler-architecture.md)** - Pipeline design, I/O schemas, crate responsibilities
3. **[HANDOFF.md](HANDOFF.md)** - Current status, completed work, immediate next steps

---

## Project Philosophy

**Spark is a "lower-level Python"** - statically typed, compiled, immutable by default, with union types for error handling.

**Core Principles** (influence ALL decisions):

- **Explicitness over implicitness** - All types explicitly declared
- **Readability over writability** - Code for readers, not writers
- **Progressive complexity** - Simple things simple, advanced features opt-in
- **No exceptions** - Errors are values in union types
- **Single responsibility** - Each crate does exactly one thing

---

## Where to Find Information

### Documentation Structure

| File                                                           | Contains                                             |
| -------------------------------------------------------------- | ---------------------------------------------------- |
| [HANDOFF.md](HANDOFF.md)                                       | Status, completed crates, next tasks                 |
| [docs/lang-spec.md](docs/lang-spec.md)                         | Grammar, syntax, all language features with examples |
| [docs/compiler-architecture.md](docs/compiler-architecture.md) | Pipeline phases, schemas, design decisions           |
| `crates/*/README.md`                                           | Per-crate purpose, design rationale, examples        |
| `crates/*/src/lib.rs`                                          | Crate-level docs, type tables, API examples          |

### Key Source Files

| Need                  | Look Here                                                                    |
| --------------------- | ---------------------------------------------------------------------------- |
| Token definitions     | [crates/spark_lexer/src/token.rs](crates/spark_lexer/src/token.rs)           |
| AST node types        | [crates/spark_ast/src/](crates/spark_ast/src/) (expr.rs, stmt.rs, decl.rs)   |
| Pre-interned keywords | [crates/spark_intern/src/lib.rs](crates/spark_intern/src/lib.rs) (kw module) |
| Diagnostic patterns   | [crates/spark_diagnostics/src/lib.rs](crates/spark_diagnostics/src/lib.rs)   |
| Test patterns         | Any `#[cfg(test)]` mod in completed crates                                   |
| Visitor pattern       | [crates/spark_ast/src/visitor.rs](crates/spark_ast/src/visitor.rs)           |

### Architecture Quick Reference

```
Source → Lexer → Parser → Modules → Resolve → Types → HIR → MIR → Borrow → Opt → Codegen
           ✅      ✅        ⏳         ⏳        ⏳      ⏳    ⏳      ⏳       ⏳     ⏳

Dependencies:
spark_span ──┬──► spark_diagnostics ──┬──► spark_lexer ──► spark_ast
spark_intern ┘                         └───────────────────────┘
```

---

## Critical Rules (Non-Negotiable)

1. **NO UNSAFE CODE** - `#![forbid(unsafe_code)]` in every crate
2. **NO PANICS ON INVALID INPUT** - Use error nodes (`ExprKind::Error`), continue parsing
3. **COMPREHENSIVE TESTS** - Every feature, every error path, edge cases
4. **ALL PUBLIC ITEMS DOCUMENTED** - Doc comments required
5. **SINGLE RESPONSIBILITY** - Each crate has exactly one job
6. **LOW COUPLING, HIGH COHESION** - Crates should work together seamlessly. Without depending too heavily on one another. So that we can make a change in one crate independant of the rest of the codebase.

---

## Essential Patterns

### 1. Error Recovery (Most Important)

```rust
// ❌ WRONG - panics stop compilation
panic!("unexpected token")

// ✅ CORRECT - record error, return error node, continue
self.diagnostics.push(Diagnostic::error("unexpected token")
    .with_label(Label::primary(span, "expected ';'")));
return Expr::new(ExprKind::Error, span);
```

**Why**: Report ALL errors in a file, not just the first one.

### 2. Pre-interned Keywords

```rust
// ❌ WRONG - wasteful re-interning
if symbol == intern("fn") { ... }

// ✅ CORRECT - use constants
use spark_intern::kw;
if symbol == kw::FN { ... }
```

**See**: [spark_intern/src/lib.rs](crates/spark_intern/src/lib.rs) - 67 pre-interned symbols

### 3. Span Management

```rust
// Always merge spans for composite nodes
let span = start_span.merge(end_span);
let expr = Expr::new(ExprKind::Binary { lhs, op, rhs }, span);
```

**See**: [spark_span README](crates/spark_span/README.md) for `Span` operations

### 4. Diagnostic Collection

```rust
// Every parser/checker returns (Result, Diagnostics)
pub struct Parser {
    diagnostics: Diagnostics,  // Accumulates errors
    // ...
}

impl Parser {
    pub fn parse(tokens: Vec<Token>) -> (Ast, Diagnostics) {
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_ast();
        (ast, parser.diagnostics)
    }
}
```

**See**: [spark_diagnostics README](crates/spark_diagnostics/README.md) for builder pattern

---

## Common Tasks

### Adding a New Crate

1. Create structure: `crates/new_crate/src/lib.rs`
2. Copy template from any completed crate's `lib.rs`
3. Add to workspace in root `Cargo.toml`
4. Use workspace dependencies: `spark_span.workspace = true`
5. Write README with design rationale
6. Add inline tests

**Template**: See any completed crate (spark_span, spark_lexer, etc.)

### Extending AST

1. Add variant to enum in [spark_ast/src/](crates/spark_ast/src/)
2. Update visitor in [spark_ast/src/visitor.rs](crates/spark_ast/src/visitor.rs)
3. Add parser method in `spark_parser`
4. Add tests

### Creating Diagnostics

```rust
// Simple
Diagnostic::error("message")
    .with_label(Label::primary(span, "note"))

// Rich with context
Diagnostic::error("type mismatch")
    .with_code("E0308")
    .with_label(Label::primary(expr_span, "expected Int, found String"))
    .with_label(Label::secondary(def_span, "defined here"))
    .with_note("Consider using .toInt()")
    .with_help("See docs for type conversion")
```

**See**: [spark_diagnostics README](crates/spark_diagnostics/README.md)

---

## Quick Reference

### Operator Precedence (for parser)

See [docs/compiler-architecture.md](docs/compiler-architecture.md) section "Phase 2: Parsing" for complete table (14 levels).

**Key points**: Assignment is right-associative, postfix operators (`.`, `[]`, `()`) are highest precedence.

### Token Kinds

65 total: 4 literals, 31 keywords, 25 operators, 5 special.  
**See**: [spark_lexer/src/token.rs](crates/spark_lexer/src/token.rs) for complete enum

### AST Structure

- **Ast** - Top level (file)
- **Item** - 8 variants (Function, Struct, Enum, Interface, TypeAlias, TypeDef, Const, Binding)
- **Expr** - 35+ variants (see [spark_ast/src/expr.rs](crates/spark_ast/src/expr.rs))
- **Pattern** - 9 variants (see [spark_ast/src/pattern.rs](crates/spark_ast/src/pattern.rs))

### Naming Conventions

| Item      | Style            | Example             |
| --------- | ---------------- | ------------------- |
| Crates    | `snake_case`     | `spark_lexer`       |
| Types     | `PascalCase`     | `Token`, `ExprKind` |
| Functions | `snake_case`     | `parse_expr`        |
| IDs       | Suffix with `Id` | `FileId`, `Symbol`  |

---

## Next Phase: spark_parser

**Goal**: Parse `Vec<Token>` → `Ast`

### Required Files

```
crates/spark_parser/src/
├── lib.rs       # Public API, Parser struct
├── expr.rs      # Expression parsing (precedence climbing)
├── stmt.rs      # Statement parsing
├── decl.rs      # Declaration parsing
├── types.rs     # Type expression parsing
└── pattern.rs   # Pattern parsing
```

### Implementation Strategy

1. **Start with basic structure** - Parser struct with utilities (current, advance, expect, check)
2. **Expression parsing** - Use precedence climbing algorithm (see architecture doc)
3. **Error recovery** - Synchronize at semicolons, braces, item keywords
4. **Test each feature** - Parse simple cases first, add complexity

### Key References for Parser

- **Grammar**: [docs/lang-spec.md](docs/lang-spec.md) sections 6-9 (operators, control flow, functions)
- **Precedence table**: [docs/compiler-architecture.md](docs/compiler-architecture.md) Phase 2
- **AST nodes**: [crates/spark_ast/src/](crates/spark_ast/src/)
- **Existing patterns**: Look at [spark_lexer/src/lexer.rs](crates/spark_lexer/src/lexer.rs) for cursor/advance patterns

### Precedence Climbing Resources

- [Pratt Parsing Explanation](https://matklad.github.io/2020/04/13/simple-but-powerful-pratt-parsing.html)
- [Crafting Interpreters - Parsing](https://craftinginterpreters.com/parsing-expressions.html)

---

## Testing Strategy

### Requirements

- Test every feature (positive cases)
- Test every error path (negative cases)
- Test edge cases (empty, EOF, invalid)
- Inline tests in `#[cfg(test)]` mod at file end

### Pattern

```rust
#[cfg(test)]
mod tests {
    use super::{...};

    fn helper(input: &str) -> Output {
        // Ergonomic test helper
    }

    #[test]
    fn descriptive_name() {
        let result = helper("input");
        assert_eq!(result, expected);
    }
}
```

**See examples**: Any completed crate's test mods

---

## Development Commands

```bash
cargo build                    # Build all
cargo test                     # Test all
cargo test -p spark_parser     # Test specific crate
cargo clippy                   # Lint (pedantic + nursery)
cargo doc --open               # View docs
```

---

## Anti-Patterns (Don't Do This)

❌ Using unsafe code  
❌ Panicking on invalid input  
❌ Re-interning pre-interned keywords  
❌ Missing spans on AST nodes  
❌ Silent error handling (returning None without diagnostic)  
❌ Non-exhaustive pattern matching  
❌ Missing tests

**For examples**, see any completed crate - they demonstrate correct patterns throughout.

---

## Getting Unstuck

1. **Read the spec** - [lang-spec.md](docs/lang-spec.md) has examples of every feature
2. **Check architecture doc** - [compiler-architecture.md](docs/compiler-architecture.md) has schemas and design rationale
3. **Look at tests** - They show expected usage patterns
4. **READMEs explain "why"** - Every crate README has "Design Decisions" section

---

**Remember**: This is a carefully architected codebase. Follow established patterns, read existing code, and maintain the high quality bar.
