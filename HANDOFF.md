# Spark Compiler - Technical Handoff

## Project Overview

Spark is a statically-typed programming language ("lower-level Python") with:

- Immutability by default
- Union types for error handling (no exceptions)
- Opt-in borrow checking for performance-critical code
- Clean, Python-like syntax

## Completed Work

### Foundation Crates (10/10 compiler phases through borrow checking)

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
| `spark_mir`         | MIR construction: CFG-based IR for borrow checking             | 32    | Complete |
| `spark_borrow`      | Borrow checking: NLL-style region inference over MIR CFG       | 27    | Complete |

**Total: 505 tests passing across workspace.**

All crates pass `cargo test` and `cargo clippy` (pedantic + nursery).

### spark_borrow Overview (Just Completed)

NLL-style borrow checker consuming MIR + TypeTable and validating reference safety:

- **Input**: `Mir` + `TypeTable` (read-only) + `DefinitionTable`
- **Output**: `Mir` (passed through unchanged) + `TypeTable` (passed through) + `Diagnostics`
- **Selective checking**: Only functions whose locals involve `Ty::Ref` types are analyzed. Functions using default copy semantics skip borrow checking entirely.
- **Fact extraction**: Walk MIR CFG to extract loans (borrows), uses, moves, assignments, and CFG edges at each program point `(BlockId, statement_index)`.
- **Liveness analysis**: Backward dataflow with worklist fixpoint. A local is live at a point if it may be read before reassignment on some path.
- **Region inference**: NLL-style — each loan's region extends from its issue point through all CFG-reachable points where the reference local is live.
- **Conflict detection**: Checks six rules at each program point:
  1. No two active borrows of overlapping places where at least one is mutable
  2. No mixed mutable + immutable borrows of overlapping places
  3. No use of a moved value without intervening reassignment
  4. No borrow of a moved value
  5. No mutation through an immutable reference (deref of `Ref { mutable: false }`)
- **Rich diagnostics**: Each error uses `Diagnostic::error()` with primary + secondary labels pointing to conflicting locations.

**Cross-crate changes made:**
- `spark_types`: Added `LifetimeId` to `Ty::Ref`, lifetime interning in `TypeTable`
- `spark_hir`: Added `HirExprKind::Borrow` and `HirExprKind::Deref` variants
- `spark_mir`: Added MIR lowering for `Borrow` (→ `Rvalue::Ref`) and `Deref` (→ `Projection::Deref`)

**API**: `check(BorrowInput) -> BorrowOutput`

**Files**:
```
crates/spark_borrow/src/
├── lib.rs        # Public API: BorrowInput, BorrowOutput, check()
├── facts.rs      # Fact extraction from MIR (loans, points, CFG edges)
├── liveness.rs   # Backward dataflow liveness analysis
├── regions.rs    # NLL-style region inference
├── check.rs      # Conflict detection and validation rules
└── errors.rs     # Diagnostic messages for borrow errors
```

## What's Next

### Immediate: spark_opt or spark_codegen (NEXT MAIN TASK)

**Option A: spark_opt** — Optimization passes over MIR (constant folding, dead code elimination, SSA construction).

**Option B: spark_codegen** — Code generation from MIR (LLVM via inkwell, or Cranelift).

### Deferred Work

These can be added incrementally as later phases need them:

- Generic instantiation and constraint solving
- Match exhaustiveness checking
- Interface binding validation (`binding` implementations)
- Method resolution (type-directed lookup)
- `to` operator / `!` propagation type narrowing
- `is`/`as` type operations (MIR stubs exist, emit warnings)
- Lambda/closure conversion (MIR stubs exist, emit warnings)
- Async state machines (MIR stubs exist, emit warnings)
- SSA construction / phi nodes (optimization pass)
- Decision-tree match compilation (sequential suffices initially)
- Drop elaboration (separate pass)
- Fresh `DefId` synthesis in desugared match arms
- Reborrowing (error on attempted reborrow for now)
- Two-phase borrows (optimization for method calls)
- Explicit lifetime parameter syntax (`fn foo[A](...)`)
- Lifetime elision rules

### Full Pipeline

1. Lexer (spark_lexer) ✅
2. Parser (spark_parser) ✅
3. Module Resolution (spark_modules) ✅
4. Name Resolution (spark_resolve) ✅
5. Type Checking (spark_types) ✅
6. HIR Lowering (spark_hir) ✅
7. MIR Construction (spark_mir) ✅
8. Borrow Checking (spark_borrow) ✅
9. **Optimization (spark_opt) <- NEXT**
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
spark_mir           (depends on span, intern, diagnostics, ast, resolve, types, hir)
spark_borrow        (depends on span, intern, diagnostics, resolve, types, mir)
```

## Key Implementation Details

1. **Pre-interned keywords** - `spark_intern::kw` module has constants like `kw::FN`, `kw::STRUCT`, plus `kw::is_builtin_type()`
2. **Lexer API** - `Lexer::new(file_id, source).tokenize()` (no free function)
3. **Error recovery** - `ExprKind::Error`, `TypeExprKind::Error`, `PatternKind::Error`, `TypeId::ERROR`, `HirExprKind::Error` nodes
4. **Visitor pattern** - `spark_ast::Visitor` trait uses `&self` (read-only). For mutable walks, use direct recursive methods (as resolver and type checker do).
5. **Borrow conflict pattern** - Use `std::mem::take` to temporarily move `self.asts`/`self.resolutions` when methods need `&mut self` alongside shared AST data.
6. **Parser limitation** - Deeply nested bare blocks with bindings (`{ { a: Int = 1 } }`) can misparse. Use `if`-blocks in tests to avoid this.
7. **Pipeline API** - Each crate: `Input` struct → public `lower()`/`check()` fn → `Output` struct. Output passes `TypeTable` through for subsequent phases.
8. **Double-parse workaround** - Type checker consumes ASTs, so test helpers re-parse + re-resolve for HIR/MIR input.
9. **Lifetime system** - `LifetimeId` in `Ty::Ref`, interned in `TypeTable`. `LifetimeId::ANONYMOUS` (0) is the default inferred lifetime. Parser already extracts lifetimes from `Ref[T, A]` syntax.
10. **Borrow checker architecture** - NLL-style: fact extraction → liveness → region inference → conflict detection. Functions without `Ref` types skip the analysis entirely.

## Notes for Next Agent

1. **Read `docs/lang-spec.md`** - Full grammar and language semantics
2. **Read `docs/compiler-architecture.md`** - Pipeline design with I/O schemas for each phase
3. **Borrow checker output** (`Mir` + `TypeTable`) is the starting point for optimization/codegen
4. **All phases use `Diagnostics`** for error accumulation — never panic on invalid input
5. **`#![forbid(unsafe_code)]`** in every crate, clippy pedantic + nursery lints
6. **MIR structure**: Each `MirFunction` has `locals: Vec<LocalDecl>` and `blocks: Vec<BasicBlock>`. Iterate blocks in order; each block has `statements` + `terminator`. Use `pretty_function()` to dump MIR for debugging.

## Build Commands

```bash
cargo build                      # Build all
cargo test                       # Test all (505 tests)
cargo clippy                     # Lint all
cargo test -p spark_borrow       # Test borrow checker
cargo doc --open                 # View documentation
```
