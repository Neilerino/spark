# Spark Compiler - Technical Handoff

## Project Overview

Spark is a statically-typed programming language ("lower-level Python") with:

- Immutability by default
- Union types for error handling (no exceptions)
- Opt-in borrow checking for performance-critical code
- Clean, Python-like syntax

## Completed Work

### Foundation Crates (8/10 compiler phases)

| Crate               | Purpose                                                        | Tests | Status   |
| ------------------- | -------------------------------------------------------------- | ----- | -------- |
| `spark_span`        | Source locations (`FileId`, `Span`, `SourceMap`)               | 13    | Complete |
| `spark_intern`      | String interning (`Symbol`, `Interner`, pre-interned keywords) | 14    | Complete |
| `spark_diagnostics` | Error reporting with ariadne rendering                         | 12    | Complete |
| `spark_lexer`       | Tokenization (keywords, literals, operators, comments)         | 33    | Complete |
| `spark_ast`         | AST node definitions for all language constructs               | 53    | Complete |
| `spark_parser`      | Recursive descent parser with Pratt expression parsing         | 125   | Complete |
| `spark_modules`     | Module resolution, dependency graph, cycle detection           | 38    | Complete |
| `spark_resolve`     | Name resolution: scoping, DefIds, imports                      | 51    | Complete |
| `spark_types`       | Type checking: two-pass signature collection + body checking   | 72    | Complete |
| `spark_hir`         | HIR lowering: desugar typed AST into core IR                   | 21    | Complete |

**Total: 414 tests passing across workspace.**

All crates pass `cargo test` and `cargo clippy` (pedantic + nursery).

### spark_hir Overview (Just Completed)

Single-pass lowerer consuming resolver + type checker output and producing a tree-structured HIR:

- **Input**: `ResolvedAst` + `DefinitionTable` + `TypeTable` (read-only)
- **Output**: `Hir` (`HashMap<DefId, HirFunction>` + `HashMap<DefId, HirStatic>`) + `TypeTable` (passed through)
- **Identifier resolution**: All identifiers/paths lowered to `DefId` via `ResolutionMap`
- **Type annotation**: Every `HirExpr` carries a `TypeId` looked up from `TypeTable`
- **Desugaring**: `Paren` (unwrap), `BlockAwait` (to `Await`), `CompoundAssign` (`x += y` to `x = x + y`), `Coalesce` (`??` to match), `OptionalField` (`?.` to match), `Propagate` (`!` to match + early return)
- **Deferred constructs**: `Each`/`EachVia`, `To`, `Partial`, `Range`/`ListRange` produce `HirExprKind::Error`
- **Pattern lowering**: All pattern kinds including guard extraction into `HirMatchArm.guard`
- **Double-parse workaround**: Type checker consumes ASTs, so `lower_source` test helper re-parses + re-resolves for HIR input

**API**: `lower(HirInput) -> HirOutput`

## What's Next

### Immediate: spark_mir (NEXT MAIN TASK)

**Goal**: Lower HIR into a control-flow-graph-based MIR for borrow checking and optimization.

**Reference**: See Phase 7 in `docs/compiler-architecture.md`.

### Deferred Work

These can be added incrementally as later phases need them:

- Generic instantiation and constraint solving
- Match exhaustiveness checking
- Interface binding validation (`binding` implementations)
- Method resolution (type-directed lookup)
- `to` operator / `!` propagation type narrowing
- `is`/`as` type operations
- Fresh `DefId` synthesis in desugared match arms (coalesce/optional field currently use placeholders)

### Full Pipeline

1. Lexer (spark_lexer) ✅
2. Parser (spark_parser) ✅
3. Module Resolution (spark_modules) ✅
4. Name Resolution (spark_resolve) ✅
5. Type Checking (spark_types) ✅
6. HIR Lowering (spark_hir) ✅
7. **MIR Lowering (spark_mir) <- NEXT**
8. Borrow Checking (spark_borrow)
9. Optimization (spark_opt)
10. Code Generation (spark_codegen)

## Crate Dependencies

```
spark_span          (no dependencies)
spark_intern        (no dependencies)
spark_diagnostics   (depends on spark_span)
spark_lexer         (depends on span, intern, diagnostics)
spark_ast           (depends on span, intern)
spark_parser        (depends on span, intern, diagnostics, lexer, ast)
spark_modules       (depends on span, intern, diagnostics, ast)
spark_resolve       (depends on span, intern, diagnostics, ast, modules)
spark_types         (depends on span, intern, diagnostics, ast, modules, resolve)
spark_hir           (depends on span, intern, diagnostics, ast, modules, resolve, types)
```

## Key Implementation Details

1. **Pre-interned keywords** - `spark_intern::kw` module has constants like `kw::FN`, `kw::STRUCT`, plus `kw::is_builtin_type()`
2. **Lexer API** - `Lexer::new(file_id, source).tokenize()` (no free function)
3. **Error recovery** - `ExprKind::Error`, `TypeExprKind::Error`, `PatternKind::Error`, `TypeId::ERROR`, `HirExprKind::Error` nodes
4. **Visitor pattern** - `spark_ast::Visitor` trait uses `&self` (read-only). For mutable walks, use direct recursive methods (as resolver and type checker do).
5. **Borrow conflict pattern** - Use `std::mem::take` to temporarily move `self.asts`/`self.resolutions` when methods need `&mut self` alongside shared AST data.
6. **Parser limitation** - Deeply nested bare blocks with bindings (`{ { a: Int = 1 } }`) can misparse. Use `if`-blocks in tests to avoid this.

## Notes for Next Agent

1. **Read `docs/lang-spec.md`** - Full grammar and language semantics
2. **Read `docs/compiler-architecture.md`** - Pipeline design with I/O schemas for each phase
3. **HIR output** (`Hir` + `TypeTable`) is the starting point for MIR lowering
4. **All phases use `Diagnostics`** for error accumulation — never panic on invalid input
5. **`#![forbid(unsafe_code)]`** in every crate, clippy pedantic + nursery lints

## Build Commands

```bash
cargo build                      # Build all
cargo test                       # Test all (414 tests)
cargo clippy                     # Lint all
cargo test -p spark_hir          # Test specific crate
cargo doc --open                 # View documentation
```
