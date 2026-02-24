# Spark Compiler — Feature Roadmap

This document tracks all features remaining to be implemented, organized by priority and dependency.

## Current State

The compiler has a complete pipeline from source to native executables:

```
Source → Lexer → Parser → Modules → Resolve → Types → HIR → MIR → Borrow → Opt → Codegen → Link
  ✅       ✅      ✅       ✅        ✅        ✅     ✅    ✅      ✅      ✅      ✅       ✅
```

**What works**: 802 tests pass (including 11 end-to-end tests). Programs using scalars (Int, Float, Bool, Char), strings, function calls, if/else, loops, match, structs, enums, and collection literals (List, Map, Set) compile to native executables and run correctly. `print` supports Int, Float, Bool, and String arguments. String interpolation (`"hello {name}"`) works end-to-end. List/Map index access (`list[i]`, `map[key]`) works via runtime calls. Collection methods (`.len()`, `.append()`, `.contains()`, `.remove()`) work for List/Map/Set/String with COW enforcement on mutating operations. User-defined methods via `binding` declarations work with dot-call syntax. Generic structs and functions with type inference work end-to-end via monomorphization (`struct Box[T] { value: T }`, `fn identity[T](x: T): T`). Reference counting and drop elaboration prevent memory leaks for all heap-allocated types. Union type handling complete: `??` coalesce, `?.` optional chain, `!` propagation, and `to` type narrowing all desugar to match expressions with proper bindings. `is` type tests work at runtime for enums, unions, and optionals. Match exhaustiveness checking catches missing patterns at compile time. Lifetime elision infers output lifetimes from input references. Interface type checking validates that bindings implement required methods. E2E test framework in `spark_driver/tests/e2e.rs`. LSP with semantic tokens in `tools/spark-lsp`. Install script (`install.sh`) provided.

```spark
fn main(): Int {
    nums: List[Int]! = [10, 20, 30]
    print(nums.len())
    print(nums.contains(20))
    nums.append(40)
    print("hello world")
    return 0
}
```

**What's missing**: Generic constraint validation (`T: Comparable`). `parallel each` (Rayon). Async Phase 3 v2 (segmented suspension). Standard library expansion. See design notes: `docs/design-notes/concurrency-model.md`, `docs/design-notes/standard-library.md`.

---

## ~~Priority 1: Make Programs Runnable~~ (DONE)

### ~~1.1 Compiler Driver (`spark` CLI)~~ — Complete

`spark_driver` implements `spark build <file> [-o out]` and `spark run <file>`. Chains all pipeline phases, invokes `cc` to link against `libspark_runtime.a`.

### ~~1.2 Runtime Library (`spark_runtime`)~~ — Complete

All heap types use custom `repr(C)` structs with `rc: usize` as the first field:
- **`SparkString`** — custom UTF-8 buffer: `{ rc, data, len, cap }`. Created by `__spark_string_new()` / `__spark_string_from_literal(ptr, len)`. Freed by `__spark_string_rc_dec()`.
- **`SparkList`** — growable type-erased array: `{ rc, data, len, cap, elem_size }`. Bounds-checked (aborts on OOB). Doubling growth strategy, minimum capacity 4.
- **`SparkMap`** — linear-scan map: `{ rc, keys, values, len, cap, key_size, value_size }`. Key comparison via memcmp. Returns null on missing key. O(n) scan — hash-based can replace it later without FFI changes.
- **`SparkSet`** — backed by `SparkList` with uniqueness and deduplication via memcmp.

**Reference counting**: `__spark_rc_inc(ptr)` works for all heap types (rc is always at offset 0). Type-specific `_rc_dec` functions (`__spark_string_rc_dec`, `__spark_list_rc_dec`, `__spark_map_rc_dec`, `__spark_set_rc_dec`) decrement RC and free when RC=0. Collection `_rc_dec` functions accept `elem_rc`/`key_rc`/`val_rc` boolean flags indicating whether elements are themselves RC'd (for nested cleanup).

**COW functions**: `__spark_string_cow`, `__spark_list_cow`, `__spark_map_cow` — if RC=1 return as-is; if RC>1 copy buffer, dec old, return new with RC=1. Wired into codegen for mutating collection methods (append, remove, add).

**Not yet implemented**: Proper `Optional` wrapping on index access, set literals (parser ambiguity with blocks).

---

## Priority 1: Resource Management

### ~~1.1 Collection Runtime Functions~~ — Complete

Type-erased collection backing implemented. List/Map/Set construction and index access work end-to-end. See section 1.2 above for details.

### ~~1.2 Drop Elaboration & Reference Counting~~ — Complete

**Implementation**: COW value semantics backed by reference counting (Swift model).

- **`spark_drop` crate**: Type query functions (`needs_rc()`, `element_needs_rc()`, `map_key_val_needs_rc()`) determine which types need RC operations. Used by both MIR lowering and codegen.
- **MIR scope tracking**: `FunctionBuilder` tracks `ScopeContext` stack with `locals_to_drop` vectors. Drops emitted at scope exits (`pop_scope_drops`), returns (`drops_for_return`), break/continue (`drops_to_loop`). Parameters registered for drops (caller RC-inc'd, callee RC-dec's).
- **Statement lowering**: `HirStmt::Let` registers RC'd locals for drop. `HirStmt::Assign` drops old value before reassignment. `HirStmt::Expr` drops discarded RC'd temps.
- **Expression lowering**: Blocks push/pop scopes. Return/break/continue emit drops before jumping.
- **Codegen**: `Statement::Drop` dispatches to type-specific `_rc_dec` functions. `Operand::Copy` emits `__spark_rc_inc` for RC'd types. `Operand::Move` transfers ownership without RC change.

### ~~1.3 COW Mutation Hooks~~ — Complete

COW uniqueness checks wired into codegen for all mutating collection methods. Before append/remove/add, `emit_cow()` calls the appropriate COW function (`__spark_list_cow`, `__spark_map_cow`), writes back the (possibly new) pointer to the receiver local.

### ~~1.4 Collection Methods~~ — Complete

Built-in method dispatch implemented via `DefinitionKind::BuiltinMethod` (same pattern as `print`). Methods type-checked in `spark_types`, resolved in MIR, dispatched by receiver type in codegen.

| Receiver | Methods | Mutating (COW) |
|----------|---------|----------------|
| `List[T]` | `.len()`, `.append(item)`, `.contains(item)`, `.remove(index)` | append, remove |
| `Map[K,V]` | `.len()`, `.contains(key)`, `.remove(key)` | remove |
| `Set[T]` | `.len()`, `.add(item)`, `.contains(item)`, `.remove(item)` | add, remove |
| `String` | `.len()` | — |

Mutating methods enforce mutable receivers at type-check time.

---

## Priority 2: Previously Blocked, Now Unblocked

These features were deferred because downstream pipeline stages didn't exist yet. Now that the full pipeline is complete, they can be implemented.

### ~~2.1 Coalesce `??` and Optional Chain `?.`~~ — Complete

Desugared in HIR to match expressions with synthetic `DefId` bindings. `x ?? default` becomes `match x { v -> v, Null -> default }`. `x?.field` becomes `match x { v -> v.field, Null -> Null }`. Works for both `Optional(T)` and `Union([..., Null, ...])` types.

### ~~2.2 `is` Type Test~~ — Complete

Implemented in Stream D. Type checker now lowers the target type, HIR passes it through correctly, and MIR emits discriminant-based type checks for enums, unions, and optionals, with compile-time constant results for primitive types.

### ~~2.3 Drop Elaboration~~ — Complete (moved to P1.2)

Implemented as part of the RC infrastructure. See P1.2 above.

---

## Priority 3: Core Language Completeness

These were deferred for complexity, not because of pipeline dependencies.

### ~~3.1 Match Exhaustiveness Checking~~ — Complete

Implemented in Stream D. `spark_types/src/exhaustive.rs` analyzes match arms using a coverage-based approach. Supports enum variants, union members, optionals, booleans, and wildcards. Reports missing patterns as errors and unreachable arms as warnings. Integrated into the type checker's match expression handling.

### ~~3.2 Lifetime Elision Rules~~ — Complete

Implemented in Stream D. `spark_types/src/lower.rs:apply_lifetime_elision()` applies Rust-like elision rules during signature collection: single input ref shares lifetime with output, self parameter donates its lifetime, multiple input refs produce an error. Integrated into `check_decl.rs`.

### ~~3.3 Propagate `!` Operator~~ — Complete

Desugared in HIR with full union type analysis. For `Optional(T)`, desugars to `match expr { v -> v, Null -> return Null }`. For union types like `User | DbError | NotFound`, uses `split_union` with expected type from binding annotation and function return type to classify success vs error members, then generates `TypeNarrow` match arms per error type. `HirPattern::TypeNarrow { def_id, ty }` added for union member matching.

### ~~3.6 `to` Type Narrowing~~ — Complete

`result to { NotFound -> ..., DbError(e) -> ... }` desugars to a `match` on the union type's error variants. Type checking validates handler patterns against union members and computes unhandled members. HIR lowering generates match arms per handler plus a wildcard passthrough arm for unhandled members. `ToHandler::Pipeline` (for `via` syntax) remains deferred — see 5.6.

Note: `to` was previously overloaded for both type narrowing and pipeline composition. Pipeline operations (`to sort`, `to take{...}`) have been moved to the `via` keyword (see 5.6).

### ~~3.4 Method Resolution and Interface Bindings~~ — Complete

User-defined method resolution via `binding` declarations works end-to-end. `DefinitionKind::BindingMethod(DefId)` tracks binding methods. Resolver collects binding methods, type checker resolves `receiver.method()` via `type_methods` map, HIR lowers method bodies as regular functions, MIR/codegen handle them transparently. Interface type checking validates that bindings implement all required methods with correct signatures.

### ~~3.5 Generic Instantiation (Monomorphization)~~ — Complete

Generic structs and functions work end-to-end via monomorphization:

- **Type checking**: `Ty::Applied { base, args }` for instantiated generic types. `Ty::Param(DefId)` for parameters. Type substitution via `substitute.rs`. Generic struct field access, struct init, and function calls with type arg inference.
- **MIR monomorphization**: `spark_mir/src/monomorphize.rs` — 4-phase pass: collect/dedup instances, clone+substitute function bodies, rewrite call sites, remove generic originals. `MirOutput.definitions` flows monomorphized entries through pipeline.
- **Codegen**: `LayoutCache::applied_struct_layout_of()` computes struct layout from substituted field types. `field_offset_for_type()` handles both `Ty::Struct` and `Ty::Applied`. `emit_place_addr`/`resolve_place_type`/`emit_aggregate` all handle Applied types.
- **Deferred**: Constraint checking (`T: Comparable`)

---

## Priority 4: Optimization

### ~~4.1 `spark_opt` — MIR Optimization Passes~~ — Complete

`spark_opt` sits between `spark_borrow` and `spark_codegen`. Five passes run in two fixed iterations:

1. **Nop elimination** — remove `Statement::Nop` placeholders
2. **Constant folding** — evaluate constant `BinaryOp`/`UnaryOp` at compile time (Int, Float, Bool; division-by-zero and overflow left to runtime)
3. **CFG simplification** — fold constant `SwitchInt`, merge goto chains, forward empty goto blocks
4. **Dead code elimination** — remove unreachable blocks (BFS from entry) and dead assignments
5. **Copy propagation** — replace single-def temp copies with source locals (conservative, single-block)

`OptLevel::None` / `OptLevel::Basic` controls whether passes run. CLI flag: `--no-opt`.

75 unit tests + full-pipeline integration tests. Zero regressions across workspace tests.

### ~~4.2 Reborrowing and Two-Phase Borrows~~ — Complete

Reborrowing works naturally (different root locals in `places_overlap`). Two-phase borrows implemented via pre-computed exempt pairs — when both loans are arguments to the same `Terminator::Call`, the conflict is suppressed. See `spark_borrow/src/check.rs`.

---

## Priority 5: Advanced Features (Need Substantial Infrastructure)

### ~~5.1 Lambda/Closure Conversion~~ — Complete

Fat pointer ABI `(fn_ptr, env_ptr)` with capture-by-value semantics. Lambda lifting for non-capturing lambdas, heap-allocated environments for capturing closures. Thunks wrap named function references used as values. Sret convention for aggregate returns. See `docs/design-notes/lambdas-closures-each.md`.

### ~~5.5 `each`/`each via` Iteration~~ — Complete

Desugared in HIR to index loops that call the callback via fat pointer. Supports inline lambdas, `fn` lambdas, and function references. COW semantics — mutation inside `each` body does not propagate.

### 5.6 `via` Pipeline Operator

**Status**: Currently parsed as `ExprKind::To` with `ToHandler::Pipeline`, deferred in HIR
**Location**: Parser done, HIR lowering emits `HirExprKind::Error`
**Design note**: `docs/design-notes/lambdas-closures-each.md`

`data via sort`, `data via take{count: 5}` — applies a named function (or partial) to the whole value. When implemented, the AST will need a new `ExprKind::Via` node (split from `ToHandler::Pipeline`).

### ~~5.2 Async/Await and Spawn~~ — Mostly Complete

`async fn`, `await`, `spawn` work end-to-end with real Tokio-backed concurrency. Phase 1 (sync shim), Phase 2 (spawn+join blocking), and Phase 3 v1 (cooperative async infrastructure with state machine transform) are all complete. 15 E2E tests. Phase 3 v2 (segmented suspension at await points, SPARK-021) and `parallel each` (Rayon, SPARK-020) remain open. See `docs/design-notes/concurrency-model.md`.

### 5.7 Standard Library (`spark.*`)

**Status**: Not started
**Design note**: `docs/design-notes/standard-library.md`

Namespaced, opt-in modules backed by native runtime functions. `spark.math`, `spark.io`, `spark.json`, `spark.net`, `spark.http`, `spark.sync`. Manifest-based architecture — no `.spark` source files needed. See design note for implementation plan.

### ~~5.3 String Interpolation~~ — Complete

Lexer recognizes `"hello {expr}"` syntax and emits interpolation tokens. Parser produces `StringLiteral` with `StringPart::Interpolation` parts. HIR desugars to runtime string concatenation calls. Works end-to-end.

### 5.4 Structural Types and Type Inference

**Status**: Rejected with error diagnostics
**Location**: `spark_types/src/lower.rs:89` (structural), `spark_types/src/lower.rs:97` (inference)

These are deliberate design decisions per the language spec — Spark requires explicit types. May revisit if the language design evolves.

---

## Summary

| Priority | Feature | Effort | Impact | Status |
|----------|---------|--------|--------|--------|
| ~~P1~~ | ~~Compiler driver~~ | ~~Medium~~ | ~~Enables running any program~~ | Done |
| ~~P1~~ | ~~Runtime library (strings)~~ | ~~Medium~~ | ~~Enables String printing~~ | Done |
| ~~P1~~ | ~~Runtime collections~~ | ~~Medium~~ | ~~Enables List/Map/Set~~ | Done |
| ~~P1~~ | ~~Drop elaboration & RC~~ | ~~Medium~~ | ~~Prevents memory leaks~~ | Done |
| ~~P1~~ | ~~COW mutation hooks~~ | ~~Low~~ | ~~Safe mutation of shared values~~ | Done |
| ~~P1~~ | ~~Collection methods~~ | ~~Medium~~ | ~~`.append()`, `.len()`, etc.~~ | Done |
| ~~P2~~ | ~~`??` / `?.` desugaring~~ | ~~Low~~ | ~~Fixes null-handling operators~~ | Done |
| ~~P2~~ | ~~`is` type test~~ | ~~Low~~ | ~~Enables type checking expressions~~ | Done |
| ~~P3~~ | ~~Match exhaustiveness~~ | ~~Medium~~ | ~~Safety — catches missing cases~~ | Done |
| ~~P3~~ | ~~Lifetime elision~~ | ~~Low~~ | ~~Ergonomics for references~~ | Done |
| ~~P3~~ | ~~`!` propagation~~ | ~~Medium~~ | ~~Error handling ergonomics~~ | Done |
| ~~P3~~ | ~~`to` type narrowing~~ | ~~Low-Medium~~ | ~~Union type error handling~~ | Done |
| ~~P3~~ | ~~Method resolution~~ | ~~Medium-High~~ | ~~User-defined methods + interfaces~~ | Done |
| ~~P3~~ | ~~Generics~~ | ~~High~~ | ~~Enables parameterized types~~ | Done |
| ~~P4~~ | ~~`spark_opt` passes~~ | ~~High~~ | ~~Performance optimization~~ | Done |
| ~~P4~~ | ~~Reborrowing~~ | ~~Low~~ | ~~Borrow checker ergonomics~~ | Done |
| ~~P5~~ | ~~Lambdas/closures~~ | ~~High~~ | ~~First-class functions~~ | Done |
| ~~P5~~ | ~~`each`/`each via`~~ | ~~Medium~~ | ~~Collection iteration~~ | Done |
| **P5** | `via` pipeline | Medium | Whole-value function application | Not started |
| ~~P5~~ | ~~Async/await/spawn~~ | ~~Very High~~ | ~~Async I/O~~ | Mostly done (v2 remaining) |
| **P5** | `parallel each` (Rayon) | Medium | CPU parallelism | Not started |
| ~~P5~~ | ~~Standard library (`spark.*`)~~ | ~~High~~ | ~~Stdlib modules~~ | Mostly done (6 modules) |
| ~~P5~~ | ~~String interpolation~~ | ~~Medium~~ | ~~Syntax convenience~~ | Done |

### Infrastructure
- **E2E test framework**: `spark_driver/tests/e2e.rs` — end-to-end tests running `.spark` fixtures
- **LSP**: `tools/spark-lsp` — semantic tokens, diagnostics
- **Design notes**:
  - `docs/design-notes/lambdas-closures-each.md` — lambda/each/via/LSP ScopeMap design (complete)
  - `docs/design-notes/concurrency-model.md` — dual-runtime concurrency (Tokio + Rayon)
  - `docs/design-notes/standard-library.md` — `spark.*` stdlib architecture

---

## Work Streams

Remaining items grouped into parallelizable streams. Streams are independent unless a dependency is noted.

### ~~Stream A: Union Type Handling~~ — Complete

All items complete (`??`, `?.`, `!`, `to`). Infrastructure: `spark_types::union` module, `HirPattern::TypeNarrow`, synthetic DefId allocation.

### ~~Stream D: Safety & Analysis~~ — Complete

All items complete (`is` type test, match exhaustiveness, lifetime elision). Also fixed resolver enum variant path resolution.

### ~~Stream E: Optimization~~ — Complete

`spark_opt` crate complete (5 passes, 2 iterations, 75 tests). Reborrowing and two-phase borrows (4.2) also complete.

### ~~Stream B: Type System Foundations~~ — Complete
> ~~Method resolution~~ → ~~Generics (monomorphization)~~

Method resolution, interface bindings, and generic instantiation via monomorphization all complete.

| Item | Effort | Dependencies | Status |
|------|--------|-------------|--------|
| ~~Interface validation + method resolution (3.4)~~ | ~~Medium-High~~ | ~~None~~ | Done |
| ~~Generic instantiation / monomorphization (3.5)~~ | ~~High~~ | ~~None~~ | Done |

### ~~Stream C: Lambdas & Functional Features~~ — Complete

Lambdas, closures, and `each`/`each via` all complete. Fat pointer ABI `(fn_ptr, env_ptr)` with capture-by-value semantics. Thunks for named function references. HIR desugars `each`/`each via` to index loops with fat pointer callbacks. `via` pipeline (5.6) still TODO.

### ~~Stream F: Syntax & Convenience~~ — Complete

String interpolation implemented end-to-end (lexer, parser, HIR, codegen).

### Stream G: Concurrency — Mostly Complete
> Design: `docs/design-notes/concurrency-model.md`

Dual-runtime architecture — Rayon for CPU parallelism, Tokio for async I/O.

| Item | Effort | Dependencies | Status |
|------|--------|-------------|--------|
| `parallel each` / `parallel each via` (Rayon) | Medium | None (reuses existing callback ABI) | Open (SPARK-020) |
| ~~Phase 1: Synchronous async shim (`Ty::Future`)~~ | ~~Medium~~ | ~~None~~ | Done |
| ~~Phase 2: `spawn` + `join` blocking (Tokio)~~ | ~~Medium-High~~ | ~~Phase 1~~ | Done |
| ~~Phase 3 v1: Cooperative async infrastructure~~ | ~~Very High~~ | ~~Phase 2~~ | Done |
| Phase 3 v2: Segmented suspension | Very High | Phase 3 v1 | Open (SPARK-021) |
| ~~Phase 4: I/O primitives~~ | ~~Medium~~ | ~~Phase 2, Stream H (stdlib)~~ | Done |

### Stream H: Standard Library (`spark.*`) — Mostly Complete
> Design: `docs/design-notes/standard-library.md`

Namespaced, opt-in modules backed by native runtime functions. Manifest-based architecture.

| Item | Effort | Dependencies | Status |
|------|--------|-------------|--------|
| ~~`spark.math`~~ | ~~Low~~ | ~~None~~ | Done |
| ~~`spark.io`~~ | ~~Low-Medium~~ | ~~None~~ | Done |
| ~~`spark.json`~~ | ~~Medium~~ | ~~None~~ | Done |
| ~~`spark.time`~~ | ~~Low~~ | ~~None~~ | Done |
| ~~`spark.net`~~ | ~~Medium~~ | ~~Stream G Phase 2+~~ | Done |
| ~~`spark.http`~~ | ~~Medium-High~~ | ~~`spark.net`, Stream G Phase 2+~~ | Done |
| `spark.sync` | Medium | Stream G Phase 2+ | Not started |

---

### Parallelism Summary

```
Remaining work:
  Stream G (Concurrency)   ─── parallel each (Rayon, SPARK-020) │ async v2 (SPARK-021)
  Stream H (Stdlib)        ─── spark.sync                       │ expansion

Completed:
  Stream A (Union types)   ─── DONE
  Stream B (Type system)   ─── DONE
  Stream C (Lambdas)       ─── DONE
  Stream D (Safety)        ─── DONE
  Stream E (Optimization)  ─── DONE
  Stream F (Interpolation) ─── DONE
  Stream G (Async)         ─── Phases 1-3v1 + Phase 4 DONE
  Stream H (Stdlib)        ─── spark.math, spark.io, spark.json, spark.time, spark.net, spark.http DONE
```

---

## Recommended Next Steps

1. **`parallel each via` (Stream G, Rayon — SPARK-020)** — Lowest risk concurrency feature. Reuses existing callback ABI. Validates `parallel` keyword plumbing.

2. **`via` pipeline operator (5.6)** — Parsed but HIR lowering emits Error. Needs `ExprKind::Via` split from `ToHandler::Pipeline`.

3. **Generic constraint validation** (`T: Comparable`) — Type params work but constraints not validated.

4. **Async Phase 3 v2 (SPARK-021)** — Segmented suspension at await points. Hardest remaining infrastructure piece. Enables true cooperative scheduling for 1M+ concurrent tasks.

See design notes for detailed plans:
- `docs/design-notes/concurrency-model.md`
- `docs/design-notes/standard-library.md`
