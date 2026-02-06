# Spark Compiler - Technical Handoff

## Project Overview

Spark is a statically-typed programming language ("lower-level Python") with:

- Immutability by default
- Union types for error handling (no exceptions)
- Opt-in borrow checking for performance-critical code
- Clean, Python-like syntax

## Completed Work

### Foundation Crates (9/10 compiler phases)

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

**Total: 478 tests passing across workspace.**

All crates pass `cargo test` and `cargo clippy` (pedantic + nursery).

### spark_mir Overview (Just Completed)

CFG-based MIR builder consuming HIR + TypeTable and producing one `MirFunction` per function:

- **Input**: `Hir` + `TypeTable` (read-only) + `DefinitionTable`
- **Output**: `Mir` (`HashMap<DefId, MirFunction>`) + `TypeTable` (passed through) + `Diagnostics`
- **Pre-SSA**: One `Local` per variable — no phi nodes. `Local(0)` = return place, `1..=N` = params, rest = locals/temps.
- **Calls are terminators**: Every `Call` splits the current block and creates a continuation block. This is correct for borrow checking (reason about before/after call).
- **Expression lowering**: Every `HirExprKind` variant dispatched in `lower_expr()` → returns `Local` holding result. Leaf expressions produce constants; compound expressions allocate temporaries; control flow creates new blocks.
- **Control flow**: `If` → `SwitchInt` + then/else/join blocks. `Loop` → header block + break block with `LoopContext` stack. `Return` → assign to `Local(0)` + `Return` terminator + dead block.
- **Pattern matching**: Sequential test-and-branch chains per match arm. Wildcard/Binding always match; Literal uses `SwitchInt`; Variant reads discriminant; Struct/Tuple project fields recursively.
- **Deferred features**: `is`, `lambda`, `await`, `spawn` emit warning diagnostics and produce void temps.
- **Pretty printer**: `pretty_function()` produces human-readable MIR dumps for debugging and test assertions.

**API**: `lower(MirInput) -> MirOutput`

**Files**:
```
crates/spark_mir/src/
├── lib.rs            # Public API + integration tests
├── mir.rs            # MIR node definitions (BlockId, Local, Place, Rvalue, Terminator, etc.)
├── builder.rs        # MirBuilder + FunctionBuilder + LoopContext
├── lower_function.rs # Function-level lowering orchestration
├── lower_expr.rs     # HirExprKind -> Local (all 28 variants)
├── lower_stmt.rs     # HirStmt lowering (Let, Assign, Expr)
├── lower_pattern.rs  # Pattern matching compilation
└── pretty.rs         # Debug printer for MIR
```

## What's Next

### Immediate: spark_borrow (NEXT MAIN TASK)

**Goal**: Borrow checking over the MIR CFG — enforce ownership, move semantics, and reference validity.

**Reference**: See Phase 8 in `docs/compiler-architecture.md`.

**Key considerations**:
- The MIR is pre-SSA (one Local per variable), so liveness analysis operates at the statement level
- Calls are terminators, so reasoning about borrows across calls is straightforward (before-call vs after-call blocks)
- Pattern match bindings create new locals via `compile_pattern`, so each arm's bindings are distinct
- `Local(0)` (return place) is always written before `Return` terminator

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

### Full Pipeline

1. Lexer (spark_lexer) ✅
2. Parser (spark_parser) ✅
3. Module Resolution (spark_modules) ✅
4. Name Resolution (spark_resolve) ✅
5. Type Checking (spark_types) ✅
6. HIR Lowering (spark_hir) ✅
7. MIR Construction (spark_mir) ✅
8. **Borrow Checking (spark_borrow) <- NEXT**
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
spark_mir           (depends on span, intern, diagnostics, ast, resolve, types, hir)
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

## Notes for Next Agent

1. **Read `docs/lang-spec.md`** - Full grammar and language semantics
2. **Read `docs/compiler-architecture.md`** - Pipeline design with I/O schemas for each phase
3. **MIR output** (`Mir` + `TypeTable`) is the starting point for borrow checking
4. **All phases use `Diagnostics`** for error accumulation — never panic on invalid input
5. **`#![forbid(unsafe_code)]`** in every crate, clippy pedantic + nursery lints
6. **MIR structure**: Each `MirFunction` has `locals: Vec<LocalDecl>` and `blocks: Vec<BasicBlock>`. Iterate blocks in order; each block has `statements` + `terminator`. Use `pretty_function()` to dump MIR for debugging.

## Build Commands

```bash
cargo build                      # Build all
cargo test                       # Test all (478 tests)
cargo clippy                     # Lint all
cargo test -p spark_mir          # Test specific crate
cargo doc --open                 # View documentation
```
