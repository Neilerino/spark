# Spark Compiler - Technical Handoff

## Project Overview

Spark is a statically-typed programming language ("lower-level Python") with:

- Immutability by default
- Union types for error handling (no exceptions)
- Opt-in borrow checking for performance-critical code
- Clean, Python-like syntax

## Completed Work

### All Crates

| Crate               | Purpose                                                        | Tests | Status   |
| ------------------- | -------------------------------------------------------------- | ----- | -------- |
| `spark_span`        | Source locations (`FileId`, `Span`, `SourceMap`)               | 16    | Complete |
| `spark_intern`      | String interning (`Symbol`, `Interner`, pre-interned keywords) | 20    | Complete |
| `spark_diagnostics` | Error reporting with ariadne rendering                         | 14    | Complete |
| `spark_lexer`       | Tokenization (keywords, literals, operators, interpolation)    | 45    | Complete |
| `spark_ast`         | AST node definitions for all language constructs               | 53    | Complete |
| `spark_parser`      | Recursive descent parser with Pratt expression parsing         | 131   | Complete |
| `spark_modules`     | Module resolution, dependency graph, cycle detection           | 38    | Complete |
| `spark_resolve`     | Name resolution: scoping, DefIds, imports                      | 56    | Complete |
| `spark_types`       | Type checking: two-pass signature collection + body checking   | 155   | Complete |
| `spark_hir`         | HIR lowering: desugar typed AST into core IR                   | 28    | Complete |
| `spark_mir`         | MIR construction + monomorphization + async transform: CFG-based IR | 48    | Complete |
| `spark_borrow`      | Borrow checking: NLL-style region inference over MIR CFG       | 27    | Complete |
| `spark_codegen`     | Native code generation via Cranelift                           | 35    | Complete |
| `spark_drop`        | Type queries for RC/drop decisions (`needs_rc`, `needs_drop`)  | 14    | Complete |
| `spark_runtime`     | `extern "C"` runtime library (print, strings, collections, RC, net, http) | 98    | Complete |
| `spark_opt`         | MIR optimization passes (5 passes, 2 iterations)               | 75    | Complete |
| `spark_driver`      | CLI driver (`spark build` / `spark run`)                       | —     | Complete |

**Total: 950+ tests passing across workspace.**

All crates pass `cargo test` and `cargo clippy` (pedantic + nursery).

### Runtime & Driver

- **`spark_runtime`**: Static library (`libspark_runtime.a`) with `extern "C"` FFI functions for print, strings, collections, and reference counting. All heap types use custom `repr(C)` structs with `rc: usize` as the first field. `SparkString` manages its own UTF-8 buffer (`{ rc, data, len, cap }`). `__spark_string_from_literal(ptr, len)` creates a heap string; `__spark_print_string/int/bool/float` dispatch at codegen time based on argument type. Collections use type-erased backing: `SparkList` (growable array), `SparkMap` (linear-scan), `SparkSet` (list + dedup). All parameterized by element byte size. RC operations: `__spark_rc_inc(ptr)` is generic (rc at offset 0); type-specific `_rc_dec` functions handle nested element cleanup and freeing. COW copy functions prepared for future mutation support.
- **`spark_driver`**: CLI binary (`spark build <file> [-o out]`, `spark run <file>`). Chains the full pipeline, writes the object file to a temp path, invokes `cc` to link against `libspark_runtime.a`, and optionally runs the result.
- **`install.sh`**: Builds in release mode and copies `spark` + `libspark_runtime.a` to a prefix directory (default `~/.local`).

### Recent: RC Infrastructure & Drop Elaboration

COW value semantics with reference counting (Swift model). All heap-allocated values now properly freed.

**New crate:**

- `spark_drop`: Type query functions — `needs_rc(ty, table)`, `needs_drop(ty, table)`, `element_needs_rc()`, `map_key_val_needs_rc()`. `needs_drop` is a superset of `needs_rc` that also covers `Ty::Function` (closure env RC). Used by MIR lowering and codegen.

**Changes made:**

- `spark_runtime/src/lib.rs`: Replaced `Box<String>` with custom `SparkString { rc, data, len, cap }`. Added `rc: usize` as first field to all heap types. All `_new` functions initialize `rc: 1`. Added `__spark_rc_inc(ptr)` (generic), type-specific `_rc_dec` functions with nested element cleanup, and COW copy functions (prepared for future use).
- `spark_mir/src/builder.rs`: Added `ScopeContext` tracking with `locals_to_drop` for LIFO drop elaboration. Methods: `push_scope()`, `pop_scope_drops()`, `register_drop()`, `drops_for_return()`, `drops_to_loop()`.
- `spark_mir/src/lower_stmt.rs`: Registers RC'd locals for drop on `Let`. Drops old value before reassignment on `Assign`. Drops discarded RC'd temps on `Expr`.
- `spark_mir/src/lower_expr.rs`: Blocks push/pop scopes. Return/break/continue emit drops before jumping.
- `spark_mir/src/lower_function.rs`: RC'd parameters registered for drop (caller inc'd, callee dec's). Fallthrough path emits drops before Return.
- `spark_codegen/cranelift/runtime.rs`: Declared 8 new stubs: `rc_inc`, `string_rc_dec`, `list_rc_dec`, `map_rc_dec`, `set_rc_dec`, `string_cow`, `list_cow`, `map_cow`.
- `spark_codegen/cranelift/emit_stmt.rs`: `Statement::Drop` now emits type-dispatched `_rc_dec` calls (was no-op). `Operand::Copy` emits `rc_inc` for RC'd types.
- `spark_codegen/cranelift/emit_rvalue.rs`: `Operand::Copy` emits `__spark_rc_inc` for RC'd types. `Operand::Move` transfers without RC change.

### Earlier: Collection Support (List, Map, Set)

Collection literals and index access work end-to-end. `[10, 20, 30]` creates a real heap-backed list.

### Recent: Union Type Handling (Stream A)

Complete implementation of `??`, `?.`, `!`, and `to` expression desugaring for union/optional types.

**New infrastructure:**

- `spark_resolve/src/def.rs`: `DefinitionTable::next_synthetic_id()` for allocating fresh DefIds during HIR desugaring
- `spark_types/src/union.rs`: **New module** — `strip_null`, `split_union`, `is_nullable`, `union_members` utilities
- `spark_types/src/check_expr.rs`: `check_expr_expected()` entry point threads expected type from binding annotations to propagation splitting
- `spark_hir/src/hir.rs`: `HirPattern::TypeNarrow { def_id, ty }` for union member matching

**Changes made:**

- `spark_hir/src/lower.rs`: `Lowerer` holds `&mut DefinitionTable` (was `&DefinitionTable`)
- `spark_hir/src/desugar.rs`: `desugar_coalesce` and `desugar_optional_field` create proper Binding/Local arms with synthetic DefIds. Replaced `desugar_propagate` with `desugar_propagate_optional` and `desugar_propagate_union`.
- `spark_hir/src/lower_expr.rs`: `ExprKind::To` fully lowered via `lower_to_expr`/`lower_to_pattern` (was Error stub). Propagate dispatches to optional vs union paths.
- `spark_hir/src/lower_pattern.rs`: `PatternKind::TypeNarrowing` → `HirPattern::TypeNarrow`
- `spark_types/src/check_expr.rs`: `unwrap_optional` handles `Ty::Union` with Null. `Propagate` uses `split_union` with expected type. New `check_to_expr` for `to` type checking.
- `spark_mir/src/lower_pattern.rs`: `HirPattern::TypeNarrow` compiled to MIR (bind + assign)
- `spark_driver/src/pipeline.rs`: Uses `hir_output.definitions` (with synthetic entries) for downstream phases
- `spark_hir/src/lib.rs`: `HirOutput` includes `definitions` field

### Earlier: Real String Support

String literals work end-to-end. `print("hello world")` produces output.

## What's Next

### Feature Status

| Feature | Status | Notes |
|---------|--------|-------|
| Lambdas & closures | **Working** | Fat-pointer ABI, capture-by-value, RC'd environments |
| Generics | **Working** | Monomorphization, type inference, struct/function generics |
| Structs | **Working** | Init, field access, passing as args, returning from functions |
| Enums + match | **Working** | Variant data, exhaustiveness checking |
| String interpolation | **Working** | `"hello {name}"` desugars to concat calls |
| Union types (`??`, `?.`, `!`, `to`) | **Working** | Full desugaring in HIR |
| Interfaces + bindings | **Working** | Method resolution, type checking |
| Type aliases | **Working** | Fully integrated |
| Collections (List, Map, Set) | **Working** | Literals, index, methods, RC |
| `loop` + `break` | **Working** | E2E tested |
| Tuples | **Partial** | Type support complete, no E2E test |
| Modules / imports | **Partial** | Resolver infrastructure done, no multi-file E2E test |
| References (`Ref`) | **Partial** | Backend wired (borrow checker, MIR, codegen) but no user-facing creation syntax. See KNOWN_ISSUES.md |
| `each` / `each via` | **Working** | Index-loop desugaring, callback via fat pointer |
| `async` / `await` / `spawn` | **Working** | Phase 3 — cooperative async infrastructure (state machine transform, poll functions, `spawn_coop` runtime). v1: poll calls original synchronously; v2 will add segmented suspension at await points |
| Generic constraints (`T: IFoo`) | **Not implemented** | Type params work but constraints not validated |
| Weak references | **Not implemented** | Mentioned in spec, no type system support |

### Suggested Next Steps (in rough priority order)

1. **References end-to-end** — The backend is done (borrow checker, MIR, codegen). Needs: parser support for `Ref[T]` (1 arg, inferred lifetime), implicit borrow coercion in type checker (`Ref[T] = value`), and implicit deref for field access. Small surface area, high payoff — unlocks the entire borrow-checking story.
2. **Nested closure RC cleanup** — Closure environments are now RC'd, but captured RC values (e.g., Strings inside an env) are not decremented when the env is freed. Phase 2 follow-up.
3. **Tuples / modules E2E tests** — These features appear to work but have no E2E coverage. Quick wins for confidence.
4. **Generic constraint validation** (`T: Comparable`) — Low priority but needed for correct generic programming.
5. **Async Phase 3 v2** — Complete the cooperative async state machine: segmented poll functions that suspend/resume at each await point (currently v1 calls the original function synchronously). Then channels for inter-task communication, `select` for multiplexing futures.

### Recently Completed

- **Async Phase 3 — Cooperative async infrastructure (v1)** — MIR-level async state machine transform (`spark_mir/src/async_transform.rs`). For each async function with await points: computes live-across-await sets via backward dataflow liveness, builds `Ty::AsyncState` struct type, generates a poll function (`fn(state_ptr) -> i8`, returns 0=Pending/1=Ready). v1 simplification: poll function calls the original function synchronously on first poll. Full infrastructure in place: `RuntimeStubs` for `__spark_alloc_state`, `__spark_spawn_coop`, `__spark_try_poll`; `SparkTask` Rust Future adapter with thread-local waker bridge; pipeline integration between borrow checking and optimization. Original function preserved for sync callers. 3 new E2E tests (`coop_basic`, `coop_chain`, `coop_parallel`). v2 will add segmented suspension at await points for true cooperative scheduling.
- **Async/Await/Spawn Phase 2** — Real concurrency via Tokio `spawn_blocking`. `Future[T]` is now an opaque heap-allocated RC'd pointer (`SparkFuture*`) instead of a transparent type. `spawn { body }` lifts the body into a zero-arg closure, spawns it on Tokio's blocking thread pool, and returns a `SparkFuture*`. `await future` joins the handle and returns the inner `T`. Direct `await async_fn()` (no spawn) stays synchronous for backward compatibility. Thread safety: spawn blocks cannot capture `Ref[T]` or mutable bindings. Runtime stubs: `__spark_spawn`, `__spark_await`, `__spark_future_rc_dec`. 7 E2E tests covering basic spawn, parallel execution, closure capture, and error cases.
- **Closure environment RC** — Closure environments now have a 16-byte RC header (`rc`, `alloc_size`). `__spark_env_alloc` prepends the header; `__spark_env_rc_dec` frees at rc=0. `needs_drop()` (superset of `needs_rc`) registers function-typed locals for drop. `Operand::Copy` RC-incs the env_ptr. Nested RC cleanup (captured RC values inside envs) deferred to Phase 2.
- **Lambdas & capturing closures** — Full lambda/closure pipeline: lambda lifting, fat-pointer ABI (`(fn_ptr, env_ptr)`), thunk generation for function references, capture-by-value semantics with heap-allocated environments (`__spark_env_alloc`). Sret calling convention for aggregate returns.
- **Generic monomorphization** — MIR-level monomorphization pass clones generic function bodies with concrete type substitutions, rewrites call sites, and removes generic originals. Codegen computes layouts for `Ty::Applied` via substituted field types. 3 new monomorphization tests.
- **Generic type checking** — `Ty::Applied { base, args }` for instantiated generic types. Type substitution via `substitute.rs`. Generic struct field access, struct init, and function calls with type arg inference all work.
- **String interpolation** — Lexer tokenizes `"hello {expr}"` syntax, parser produces `StringPart::Interpolation`, HIR desugars to runtime string concatenation calls. Works end-to-end.
- **Method resolution & interface bindings** — `DefinitionKind::BindingMethod(DefId)` for user-defined methods. `binding Foo for IBar { fn method() { ... } }` works end-to-end. Interface type checking validates bindings implement all required methods.

### Earlier Completed (Stream A: Union Type Handling)

- **`??` coalesce** — Desugared in HIR to match expressions with synthetic `DefId` bindings.
- **`?.` optional chain** — Desugared to match with Null propagation.
- **`!` propagation** — Full union type analysis with `split_union` for success/error classification.
- **`to` type narrowing** — Desugared to match on union error variants.

### Earlier Completed (Stream D: Safety & Analysis)

- **`is` type test** — MIR lowering with discriminant checks for enums, unions, optionals. Compile-time constants for primitive types.
- **Match exhaustiveness checking** — `spark_types/src/exhaustive.rs`. Coverage-based analysis for enum variants, union members, optionals, booleans. Reports missing patterns and unreachable arms.
- **Lifetime elision rules** — `spark_types/src/lower.rs:apply_lifetime_elision()`. Rust-like rules: single input ref → shared output lifetime, self parameter → output gets self's lifetime, multiple input refs → error.
- **Resolver enum variant paths** — `resolve_value_path` and `resolve_type_path` now resolve `Enum.Variant` to the variant's individual `DefId`, enabling proper enum exhaustiveness checking.

### Deferred Design Decisions

- Explicit lifetime parameter syntax (`fn foo[A](...)`)

### Full Pipeline

1. Lexer (spark_lexer) ✅
2. Parser (spark_parser) ✅
3. Module Resolution (spark_modules) ✅
4. Name Resolution (spark_resolve) ✅
5. Type Checking (spark_types) ✅
6. HIR Lowering (spark_hir) ✅
7. MIR Construction (spark_mir) ✅
8. Borrow Checking (spark_borrow) ✅
8.5. Async State Machine Transform (spark_mir::async_transform) ✅
9. Optimization (spark_opt) ✅ (nop elim, const fold, CFG simplify, DCE, copy prop; `--no-opt` to skip)
10. Code Generation (spark_codegen) ✅
11. Runtime (spark_runtime) ✅ (strings, print, collections, RC implemented)
12. Driver (spark_driver) ✅ (`spark build` / `spark run`)

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
spark_drop          (depends on types)
spark_mir           (depends on span, intern, diagnostics, ast, resolve, types, hir, drop, modules)
spark_borrow        (depends on span, intern, diagnostics, resolve, types, mir)
spark_opt           (depends on span, intern, diagnostics, ast, resolve, types, mir)
spark_codegen       (depends on span, intern, diagnostics, ast, resolve, types, mir, drop + cranelift)
spark_runtime       (depends on tokio — standalone staticlib)
spark_driver        (depends on all pipeline crates + opt + codegen + clap)
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
11. **Reference counting** - COW value semantics (Swift model). `spark_drop::needs_rc()` determines which types need RC pointer ops; `needs_drop()` is the superset used for drop registration (includes `Ty::Function` for closure env RC). `Operand::Copy` emits `rc_inc`; `Operand::Move` does not. `Statement::Drop` emits type-specific `_rc_dec` (or `__spark_env_rc_dec` for functions). Scope tracking in MIR builder handles drop elaboration at scope exits, returns, break/continue. Parameters are RC'd (caller inc's, callee dec's at scope exit). Closure environments have a 16-byte header (`rc`, `alloc_size`) prepended by `__spark_env_alloc`.

## Notes for Next Agent

1. **Read `docs/lang-spec.md`** - Full grammar and language semantics
2. **Read `docs/compiler-architecture.md`** - Pipeline design with I/O schemas for each phase
3. **Read `docs/roadmap.md`** - Prioritized list of remaining work with status
4. **All phases use `Diagnostics`** for error accumulation — never panic on invalid input
5. **`#![forbid(unsafe_code)]`** in every crate (except `spark_runtime`), clippy pedantic + nursery lints
6. **MIR structure**: Each `MirFunction` has `locals: Vec<LocalDecl>` and `blocks: Vec<BasicBlock>`. Iterate blocks in order; each block has `statements` + `terminator`. Use `pretty_function()` to dump MIR for debugging.
7. **Built-in function dispatch**: `print` is handled specially in codegen — `resolve_callee_def_id()` traces the callee through MIR locals, then `emit_builtin_print()` dispatches by argument type. New built-in functions should follow this pattern.

## Installing & Running Spark Programs

### Quick start (from source tree)

```bash
cargo build -p spark_driver -p spark_runtime

# Compile and run a .spark file (runtime lib found automatically next to binary)
cargo run -p spark_driver -- run hello.spark

# Or compile to an executable
cargo run -p spark_driver -- build hello.spark -o hello
./hello
```

### Install system-wide

```bash
./install.sh              # installs to ~/.local/bin + ~/.local/lib
./install.sh /usr/local   # installs to /usr/local/bin + /usr/local/lib
```

After installing, set `SPARK_RUNTIME_DIR` so the compiler can find `libspark_runtime.a`:

```bash
export SPARK_RUNTIME_DIR="$HOME/.local/lib"   # add to your shell profile
```

Then:

```bash
spark run hello.spark          # compile + run
spark build hello.spark -o hello   # compile to executable
```

### What `print` supports

`print` accepts `Int`, `Float`, `Bool`, or `String`:

```
fn main(): Int {
    print("hello world")
    print(42)
    print(3.14)
    print(true)
    0
}
```

### Requirements

- Rust toolchain (for building from source)
- A C compiler accessible as `cc` (Xcode CLI tools on macOS, `gcc`/`clang` on Linux) — used by the linker step

## Build Commands

```bash
cargo build                      # Build all
cargo test                       # Test all (900+ tests)
cargo clippy                     # Lint all
cargo test -p spark_codegen      # Test code generator
cargo doc --open                 # View documentation
```
