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
| [Linear project](https://linear.app/neilwaddenpersonal/project/spark-f367ea1ad80c) | Open tasks, priorities, status (source of truth) |
| [tasks/research/](tasks/research/)                             | Research & planning deliverables (SPARK-NNN.md)      |
| [HANDOFF.md](HANDOFF.md)                                       | Status, completed crates, next tasks                 |
| [docs/lang-spec.md](docs/lang-spec.md)                         | Grammar, syntax, all language features with examples |
| [docs/compiler-architecture.md](docs/compiler-architecture.md) | Pipeline phases, schemas, design decisions           |
| [docs/roadmap.md](docs/roadmap.md)                             | Feature roadmap, work streams, priority order        |
| [docs/known-issues.md](docs/known-issues.md)                   | Active bugs with reproducers and workarounds         |
| [docs/KNOWN_ISSUES.md](docs/KNOWN_ISSUES.md)                   | Architectural loose ends (closures, codegen, refs)   |
| [docs/design-notes/](docs/design-notes/)                       | Concurrency, stdlib, lambda/closure design plans     |
| `crates/*/README.md`                                           | Per-crate purpose, design rationale, examples        |
| `crates/*/src/lib.rs`                                          | Crate-level docs, type tables, API examples          |

### Key Source Files

| Need                   | Look Here                                                                    |
| ---------------------- | ---------------------------------------------------------------------------- |
| Token definitions      | [crates/spark_lexer/src/token.rs](crates/spark_lexer/src/token.rs)           |
| AST node types         | [crates/spark_ast/src/](crates/spark_ast/src/) (expr.rs, stmt.rs, decl.rs)   |
| Pre-interned keywords  | [crates/spark_intern/src/lib.rs](crates/spark_intern/src/lib.rs) (kw module) |
| Diagnostic patterns    | [crates/spark_diagnostics/src/lib.rs](crates/spark_diagnostics/src/lib.rs)   |
| Visitor pattern        | [crates/spark_ast/src/visitor.rs](crates/spark_ast/src/visitor.rs)           |
| Pipeline orchestration | [crates/spark_driver/src/pipeline.rs](crates/spark_driver/src/pipeline.rs)   |
| Name resolution        | [crates/spark_resolve/src/resolver.rs](crates/spark_resolve/src/resolver.rs) |
| Type checker           | [crates/spark_types/src/checker.rs](crates/spark_types/src/checker.rs)       |
| Type table             | [crates/spark_types/src/table.rs](crates/spark_types/src/table.rs)           |
| HIR desugaring         | [crates/spark_hir/src/desugar.rs](crates/spark_hir/src/desugar.rs)           |
| MIR data structures    | [crates/spark_mir/src/mir.rs](crates/spark_mir/src/mir.rs)                   |
| Codegen backend trait  | [crates/spark_codegen/src/backend.rs](crates/spark_codegen/src/backend.rs)   |
| Cranelift emission     | [crates/spark_codegen/src/cranelift/](crates/spark_codegen/src/cranelift/)   |
| Drop / RC type queries | [crates/spark_drop/src/lib.rs](crates/spark_drop/src/lib.rs)                 |
| Runtime FFI functions  | [crates/spark_runtime/src/lib.rs](crates/spark_runtime/src/lib.rs)           |
| Optimization passes    | [crates/spark_opt/src/lib.rs](crates/spark_opt/src/lib.rs)                   |
| Borrow checker         | [crates/spark_borrow/src/check.rs](crates/spark_borrow/src/check.rs)         |
| E2E tests              | [crates/spark_driver/tests/e2e.rs](crates/spark_driver/tests/e2e.rs)         |
| LSP server             | [tools/spark-lsp/](tools/spark-lsp/)                                         |
| Test patterns          | Any `#[cfg(test)]` mod across crates                                         |

### Architecture Quick Reference

```
Source → Lexer → Parser → Modules → Resolve → Types → HIR → MIR → Borrow → Opt → Codegen → Link
           ✅      ✅        ✅         ✅        ✅      ✅    ✅      ✅       ✅     ✅        ✅

17 crates, 900+ tests. All phases complete.

Infrastructure:  spark_span, spark_intern, spark_diagnostics
Front-end:       spark_lexer → spark_ast → spark_parser → spark_modules → spark_resolve
Middle-end:      spark_types → spark_hir → spark_mir (+ spark_drop) → spark_borrow → spark_opt
Back-end:        spark_codegen (Cranelift) → spark_runtime (libspark_runtime.a) → spark_driver (CLI)
Tooling:         tools/spark-lsp, spark_stdlib
```

---

## Task Tracking System

All open work is tracked in the **[Linear project](https://linear.app/neilwaddenpersonal/project/spark-f367ea1ad80c)**.
Every issue title includes its `[SPARK-NNN]` ID for cross-referencing with code comments.

### How it works

1. **Linear** — The single source of truth for all open, in-progress, and completed tasks.
   Issues are labelled (Bug, Performance, Feature, Refactor, Documentation, Testing, Research)
   and prioritized (Urgent, High, Medium, Low).
2. **Code comments** — In any source file, a comment like `// TODO [SPARK-001]` links that location
   to the corresponding Linear issue. The **TODO Tree** VS Code extension indexes these and
   shows them in a sidebar tree view, grouped by tag.
3. **[tasks/research/](tasks/research/)** — Research and planning tasks produce deliverables as
   markdown files here, named `SPARK-NNN.md` matching the ticket ID.

### Tags

| Tag     | Meaning                                | When to use                              |
| ------- | -------------------------------------- | ---------------------------------------- |
| `TODO`  | Planned work, not yet started          | Default for new tasks                    |
| `FIXME` | Broken behaviour that needs correcting | Something works but incorrectly          |
| `BUG`   | User-facing correctness issue          | End users would encounter this           |
| `PERF`  | Performance improvement opportunity    | Measurable or architectural perf concern |
| `HACK`  | Temporary workaround, must be replaced | Known tech debt with a plan to fix       |
| `NOTE`  | Context for future readers, no action  | Explain _why_ something is a certain way |

### Adding a task

1. Create a new issue in the [Linear Spark project](https://linear.app/neilwaddenpersonal/project/spark-f367ea1ad80c)
   with a `[SPARK-NNN]` prefix in the title (increment from the highest existing ID).
2. Set the appropriate label, priority, and description.
3. Add `// TAG [SPARK-NNN]` comments in every affected source file.
4. For research/planning tasks, create `tasks/research/SPARK-NNN.md` with deliverables.

### VS Code integration

The TODO Tree extension is configured in `.vscode/settings.json`. It scans `**/*.rs`, `**/*.spark`,
`**/*.md`, and `**/*.toml` for tag patterns. Open the TODO Tree sidebar to browse all tagged
comments, grouped by tag type and filterable by crate.

---

## Critical Rules (Non-Negotiable)

1. **NO UNSAFE CODE** - `#![forbid(unsafe_code)]` in every crate (except `spark_runtime` which uses `extern "C"` FFI)
2. **NO PANICS ON INVALID INPUT** - Use error nodes (`ExprKind::Error`), continue parsing
3. **COMPREHENSIVE TESTS** - Every feature, every error path, edge cases
4. **ALL PUBLIC ITEMS DOCUMENTED** - Doc comments required
5. **SINGLE RESPONSIBILITY** - Each crate has exactly one job
6. **NO TEMPORARY WORKAROUNDS** - If we're refactoring, and need to make a change that would break old unit tests get confirmation how to proceed. We will either create a task to clean up later, or if it makes sense resolve in current task.
7. **LOW COUPLING, HIGH COHESION** - Crates should work together seamlessly. Without depending too heavily on one another. So that we can make a change in one crate independant of the rest of the codebase.

---

## Essential Patterns

### 1. Error Recovery (Most Important)

```rust
// ❌ WRONG - panics stop compilation
panic!("unexpected token")

// ✅ CORRECT - record error, return error node, continue
self.diagnostics.push(Diagnostic::error("unexpected token")
    .with_label(Label::primary(span, "expected '}'")));
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

**See**: [spark_intern/src/lib.rs](crates/spark_intern/src/lib.rs) - 108 pre-interned symbols

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
    pub fn parse(tokens: Vec<Token>, file_id: FileId) -> (Ast, Diagnostics) {
        let mut parser = Parser::new(tokens, file_id);
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

92 total: 8 literals, 37 keywords, 15 punctuation, 28 operators, 4 special.  
**See**: [spark_lexer/src/token.rs](crates/spark_lexer/src/token.rs) for complete enum

### AST Structure

- **Ast** - Top level (file)
- **Item** - 8 variants (Function, Struct, Enum, Interface, TypeAlias, TypeDef, Const, Binding)
- **Expr** - 41 variants (see [spark_ast/src/expr.rs](crates/spark_ast/src/expr.rs))
- **Pattern** - 11 variants (see [spark_ast/src/pattern.rs](crates/spark_ast/src/pattern.rs))

### Naming Conventions

| Item      | Style            | Example                     |
| --------- | ---------------- | --------------------------- |
| Crates    | `snake_case`     | `spark_lexer`               |
| Types     | `PascalCase`     | `Token`, `ExprKind`         |
| Functions | `snake_case`     | `parse_expr`                |
| IDs       | Suffix with `Id` | `FileId`, `DefId`, `TypeId` |

---

## Running Spark Programs

```bash
# From the source tree — compile and run a .spark file
cargo run -p spark_driver -- run hello.spark

# Compile to a standalone executable
cargo run -p spark_driver -- build hello.spark -o hello
./hello

# Install system-wide (copies spark + libspark_runtime.a to prefix)
./install.sh              # default: ~/.local/bin + ~/.local/lib
./install.sh /usr/local   # or a custom prefix

# If latest build is installed:
spark run --single-file file.spark
# Or for full workspace discovery - If any files in workspace have errors will not run
spark run file.spark

```

Requires a C compiler accessible as `cc` (Xcode CLI tools on macOS, `gcc`/`clang` on Linux).

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
cargo build                                    # Build all
cargo test                                     # Test all (900+ tests)
cargo test -p spark_codegen                    # Test specific crate
cargo clippy                                   # Lint (pedantic + nursery)
cargo doc --open                               # View docs
cargo run -p spark_driver -- run hello.spark   # Compile + run a .spark file
cargo run -p spark_driver -- build f.spark -o f # Compile to executable
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
❌ Hacky fallbacks

**For examples**, see any completed crate - they demonstrate correct patterns throughout.

---

## Getting Unstuck

1. **Read the spec** - [lang-spec.md](docs/lang-spec.md) has examples of every feature
2. **Check architecture doc** - [compiler-architecture.md](docs/compiler-architecture.md) has schemas and design rationale
3. **Check the task tracker** - [Linear project](https://linear.app/neilwaddenpersonal/project/spark-f367ea1ad80c) has all open work with affected files
4. **Look at tests** - They show expected usage patterns
5. **READMEs explain "why"** - Every crate README has "Design Decisions" section

---

**Remember**: This is a carefully architected codebase. Follow established patterns, read existing code, and maintain the high quality bar.
