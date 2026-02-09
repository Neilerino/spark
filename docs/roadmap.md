# Spark Compiler — Feature Roadmap

This document tracks all features remaining to be implemented, organized by priority and dependency.

## Current State

The compiler has a complete pipeline from source to native executables:

```
Source → Lexer → Parser → Modules → Resolve → Types → HIR → MIR → Borrow → Opt → Codegen → Link
  ✅       ✅      ✅       ✅        ✅        ✅     ✅    ✅      ✅      ✅      ✅       ✅
```

**What works**: 632 tests pass (including 8 end-to-end tests). Programs using scalars (Int, Float, Bool, Char), strings, function calls, if/else, loops, match, structs, enums, and collection literals (List, Map, Set) compile to native executables and run correctly. `print` supports Int, Float, Bool, and String arguments. List/Map index access (`list[i]`, `map[key]`) works via runtime calls. Collection methods (`.len()`, `.append()`, `.contains()`, `.remove()`) work for List/Map/Set/String with COW enforcement on mutating operations. Reference counting and drop elaboration prevent memory leaks for all heap-allocated types. Union type handling complete: `??` coalesce, `?.` optional chain, `!` propagation, and `to` type narrowing all desugar to match expressions with proper bindings. E2E test framework in `spark_driver/tests/e2e.rs`. LSP with semantic tokens in `tools/spark-lsp`. Install script (`install.sh`) provided.

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

**What's missing**: `each`/`each via` iteration (blocked on lambdas/closures — see `docs/design-notes/lambdas-closures-each.md`). Several language features deferred with stubs/warnings (closures, async, generics, interface method resolution).

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

### 2.2 `is` Type Test

**Status**: Warning in MIR lowering
**Location**: `spark_mir/src/lower_expr.rs:289`
**Blocker was**: No codegen to verify generated discriminant checks

**Fix**: MIR already has `Rvalue::Discriminant` and `SwitchInt`. Lower `HirExprKind::Is` to:
1. Extract discriminant from the value
2. Compare against the target type's variant tag
3. Return Bool result

For enum/union types this is a straightforward integer comparison. Codegen already handles all the pieces.

### ~~2.3 Drop Elaboration~~ — Complete (moved to P1.2)

Implemented as part of the RC infrastructure. See P1.2 above.

---

## Priority 3: Core Language Completeness

These were deferred for complexity, not because of pipeline dependencies.

### 3.1 Match Exhaustiveness Checking

**Status**: Not implemented
**Location**: Would go in `spark_types`
**Impact**: Programs with non-exhaustive matches compile but hit `Unreachable` trap at runtime

Pure analysis pass in the type checker. For each match expression:
- Collect scrutinee type and all patterns
- Verify all variants/values are covered
- Report missing patterns as errors
- Report unreachable arms as warnings

### 3.2 Lifetime Elision Rules

**Status**: Not implemented — all lifetimes are `LifetimeId::ANONYMOUS`
**Location**: Would go in `spark_types`
**Impact**: Borrow checker works but requires explicit lifetimes for multi-reference functions

Rules:
- Single input reference → lifetime applies to output
- Multiple input references → require explicit lifetimes
- `self` parameter → implicit lifetime on output

### ~~3.3 Propagate `!` Operator~~ — Complete

Desugared in HIR with full union type analysis. For `Optional(T)`, desugars to `match expr { v -> v, Null -> return Null }`. For union types like `User | DbError | NotFound`, uses `split_union` with expected type from binding annotation and function return type to classify success vs error members, then generates `TypeNarrow` match arms per error type. `HirPattern::TypeNarrow { def_id, ty }` added for union member matching.

### ~~3.6 `to` Type Narrowing~~ — Complete

`result to { NotFound -> ..., DbError(e) -> ... }` desugars to a `match` on the union type's error variants. Type checking validates handler patterns against union members and computes unhandled members. HIR lowering generates match arms per handler plus a wildcard passthrough arm for unhandled members. `ToHandler::Pipeline` (for `via` syntax) remains deferred — see 5.6.

Note: `to` was previously overloaded for both type narrowing and pipeline composition. Pipeline operations (`to sort`, `to take{...}`) have been moved to the `via` keyword (see 5.6).

### 3.4 Method Resolution and Interface Bindings

**Status**: Partial — built-in collection methods work, user-defined methods do not
**Location**: `spark_types` and `spark_resolve`
**Impact**: No user-defined methods, no interface validation

**Done**: Built-in method dispatch for collection types (`.len()`, `.append()`, etc.) via `DefinitionKind::BuiltinMethod`. Type checker dispatches by receiver type.

**Remaining** (two parts):
1. **Interface validation**: Verify `binding Foo for Bar { ... }` implements all required methods
2. **User-defined method resolution**: `expr.method(args)` looks up method via type's bindings

### 3.5 Generic Instantiation

**Status**: Type args parsed but ignored
**Location**: Would go in `spark_types`
**Impact**: No generic functions or types

Needs:
- Generic parameter tracking in type system (`Ty::Param` exists but isn't used)
- Type argument inference or explicit annotation
- Monomorphization pass (clone function bodies with types substituted)

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

75 unit tests + full-pipeline integration tests. Zero regressions across 641 workspace tests.

### 4.2 Reborrowing and Two-Phase Borrows

**Status**: Borrow checker rejects these cases
**Location**: `spark_borrow`
**Impact**: Ergonomics only — workarounds exist

- **Reborrowing**: Allow re-borrowing a reference (shortens region automatically)
- **Two-phase borrows**: Allow `x.method(&x)` by splitting borrow into issue/use phases

---

## Priority 5: Advanced Features (Need Substantial Infrastructure)

### 5.1 Lambda/Closure Conversion

**Status**: Warning in MIR lowering
**Location**: `spark_mir/src/lower_expr.rs:294`
**Design note**: `docs/design-notes/lambdas-closures-each.md`

Needs:
- Closure capture analysis (identify free variables in lambda body)
- Closure environment struct (captured variables as fields)
- Closure representation (function pointer + environment pointer)
- Borrow checker support for captures
- Codegen for closure allocation and invocation
- LSP `ScopeMap` integration (see design note for LSP team requirements)

**Blocks**: `each`/`each via` (5.5), partial application

### 5.5 `each`/`each via` Iteration

**Status**: Parsed, deferred in HIR (emits `HirExprKind::Error`)
**Location**: Parser done (`spark_parser/src/expr.rs:524`), resolver creates param DefIds
**Blocked by**: Lambda/closure support (5.1)
**Design note**: `docs/design-notes/lambdas-closures-each.md`

`each`/`each via` must accept both inline bodies AND function references (e.g., `items each via double`) to enable clean chaining with partial application. An inline-only loop desugaring was rejected because it can't support function-as-value arguments. Implementation deferred until lambdas land.

### 5.6 `via` Pipeline Operator

**Status**: Currently parsed as `ExprKind::To` with `ToHandler::Pipeline`, deferred in HIR
**Location**: Parser done, HIR lowering emits `HirExprKind::Error`
**Blocked by**: Lambda/closure support (5.1), method resolution (3.4)
**Design note**: `docs/design-notes/lambdas-closures-each.md`

`data via sort`, `data via take{count: 5}` — applies a named function (or partial) to the whole value. Requires function-as-value semantics. Previously part of the `to` keyword; split out for clearer semantics (`to` is now exclusively type narrowing).

When implemented, the AST will need a new `ExprKind::Via` node (split from `ToHandler::Pipeline`).

### 5.2 Async/Await and Spawn

**Status**: Warning in MIR lowering
**Location**: `spark_mir/src/lower_expr.rs:299` (await), `spark_mir/src/lower_expr.rs:304` (spawn)

Needs:
- Future type system (`Ty::Future { inner }`)
- State machine transformation (each await = state transition)
- Async runtime/executor
- Task scheduler for spawn
- Substantial codegen work

### 5.3 String Interpolation

**Status**: Not tokenized or parsed
**Location**: Would need changes to `spark_lexer` and `spark_parser`

Needs lexer to recognize `"hello {expr}"` syntax and emit interpolation tokens. Parser desugars to string concatenation or format call.

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
| **P2** | `is` type test | Low | Enables type checking expressions | MIR stub |
| **P3** | Match exhaustiveness | Medium | Safety — catches missing cases | Not started |
| **P3** | Lifetime elision | Low | Ergonomics for references | Not started |
| ~~P3~~ | ~~`!` propagation~~ | ~~Medium~~ | ~~Error handling ergonomics~~ | Done |
| ~~P3~~ | ~~`to` type narrowing~~ | ~~Low-Medium~~ | ~~Union type error handling~~ | Done |
| **P3** | Method resolution | Medium-High | User-defined methods + interfaces | Partial (builtins done) |
| **P3** | Generics | High | Enables parameterized types | Not started |
| ~~P4~~ | ~~`spark_opt` passes~~ | ~~High~~ | ~~Performance optimization~~ | Done |
| **P4** | Reborrowing | Low | Borrow checker ergonomics | Not started |
| **P5** | Lambdas/closures | High | First-class functions | MIR stub |
| **P5** | `each`/`each via` | Medium | Collection iteration | Blocked on lambdas |
| **P5** | `via` pipeline | Medium | Whole-value function application | Blocked on lambdas |
| **P5** | Async/await/spawn | Very High | Concurrency model | MIR stub |
| **P5** | String interpolation | Medium | Syntax convenience | Not started |

### Infrastructure
- **E2E test framework**: `spark_driver/tests/e2e.rs` — 8 tests running `.spark` fixtures end-to-end
- **LSP**: `tools/spark-lsp` — semantic tokens, diagnostics
- **Design notes**: `docs/design-notes/lambdas-closures-each.md` — captures lambda/each/via/LSP ScopeMap requirements

---

## Work Streams

Remaining items grouped into parallelizable streams. Streams are independent unless a dependency is noted.

### ~~Stream A: Union Type Handling~~ — Complete
> ~~`!` propagation → `to` type narrowing → `??`/`?.` desugaring~~

All items complete. Infrastructure added: `DefinitionTable::next_synthetic_id()` for fresh DefIds during desugaring, `spark_types::union` module (`strip_null`, `split_union`, `is_nullable`, `union_members`), `HirPattern::TypeNarrow` for union member matching, `check_expr_expected` for binding-type-driven union splitting, `&mut DefinitionTable` threading through HIR lowering.

| Item | Status |
|------|--------|
| Union type analysis infrastructure | Done — `spark_types::union` |
| `!` propagation (3.3) | Done |
| `to` type narrowing (3.6) | Done |
| `??` / `?.` desugaring (2.1) | Done |

### Stream B: Type System Foundations
> Method resolution → Generics

Foundational type system features. Method resolution is a prerequisite for many downstream features. Generics are the highest-effort item in the project.

| Item | Effort | Dependencies |
|------|--------|-------------|
| Interface validation + method resolution (3.4) | Medium-High | None |
| Generic instantiation / monomorphization (3.5) | High | Method resolution helps but not strictly required |

**Conflicts with Stream C**: Both touch `spark_types` and `spark_resolve` heavily. If done in parallel with lambdas, coordinate carefully on type system changes (e.g., `Ty::Function` usage, method lookup paths). Method resolution is also a soft dependency for `via` pipeline (Stream C needs a way to look up named operations like `sort`, `take`).

### Stream C: Lambdas & Functional Features
> Lambdas → Partial application → `each`/`each via` → `via` pipeline

The largest dependency chain. Everything here requires function-as-value semantics.

| Item | Effort | Dependencies |
|------|--------|-------------|
| Function types as values (`Ty::Function`) | Medium | None |
| Lambda/closure conversion (5.1) | High | Function types, LSP ScopeMap coordination |
| Partial application | Medium | Lambdas |
| `each`/`each via` (5.5) | Medium | Lambdas, partials |
| `via` pipeline (5.6) | Medium | Lambdas, partials, method resolution (Stream B) |

**Conflicts with Stream B**: Both modify how functions/methods are resolved and called. `via` specifically needs method resolution to look up named operations. LSP `ScopeMap` (from LSP team requirements) should be built alongside lambda scope handling in `spark_resolve`.

### Stream D: Safety & Analysis
> Independent analysis passes

These are independent of each other and of most other streams.

| Item | Effort | Dependencies |
|------|--------|-------------|
| `is` type test (2.2) | Low | None |
| Match exhaustiveness checking (3.1) | Medium | None |
| Lifetime elision rules (3.2) | Low | None |

**Conflicts**: None. These are read-only analysis passes in `spark_types` and `spark_borrow`. Can safely run in parallel with any other stream.

### Stream E: Optimization
> Independent optimization crate

Completely independent. Inserts between `spark_borrow` and `spark_codegen`.

| Item | Effort | Dependencies |
|------|--------|-------------|
| ~~`spark_opt` crate (4.1)~~ | ~~High~~ | ~~None~~ | **Done** |
| Reborrowing / two-phase borrows (4.2) | Low | None |

**Conflicts**: None. `spark_opt` operates on MIR which is stable. Reborrowing is isolated to `spark_borrow`.

### Stream F: Syntax & Convenience

| Item | Effort | Dependencies |
|------|--------|-------------|
| String interpolation (5.3) | Medium | None (lexer + parser only) |

**Conflicts**: None. Purely lexer/parser changes, independent of all other streams.

### Stream G: Concurrency
> Blocked on Stream C (lambdas)

| Item | Effort | Dependencies |
|------|--------|-------------|
| Async/await/spawn (5.2) | Very High | Lambdas (Stream C), runtime executor |

**Conflicts with Stream C**: Directly depends on closures for spawn. Should start after lambda support is stable.

---

### Parallelism Summary

```
Can run simultaneously:
  Stream A (Union types)  ─── no conflicts ───  Stream D (Safety)
  Stream A (Union types)  ─── no conflicts ───  Stream E (Optimization)
  Stream A (Union types)  ─── no conflicts ───  Stream F (String interpolation)
  Stream D (Safety)       ─── no conflicts ───  Stream E (Optimization)
  Stream D (Safety)       ─── no conflicts ───  Stream F (String interpolation)

Must coordinate:
  Stream B (Type system)  ─── shared code ────  Stream C (Lambdas)

Sequential dependencies:
  Stream C (Lambdas)      ─── blocks ─────────  Stream G (Async)
  Stream B (Methods)      ─── soft blocks ────  Stream C (`via` pipeline)
```

**Recommended parallel tracks**:
1. **Track 1**: Stream A + Stream D (union types + safety analysis) — low risk, high impact
2. **Track 2**: Stream B (type system foundations) — prepare for lambdas
3. **Track 3**: Stream E + Stream F (optimization + string interpolation) — fully independent

Once Track 2 is stable, start Stream C (lambdas), then Stream G (async).

---

## Recommended Implementation Order

The streams above can be parallelized, but for the **critical path** (features that unlock the most downstream work), the recommended sequential order is:

1. **Method resolution & interface bindings (Stream B, 3.4)** — Foundation for user-defined types. Many features need to look up methods by receiver type. Should come before lambdas because lambda support will need method-like call dispatch.

2. **Lambdas/closures (Stream C, 5.1)** — Unlocks all functional features. Requires function types as first-class values, capture analysis, closure environments, and LSP `ScopeMap` integration.

3. **`each`/`each via`/`via`/partial application (Stream C, 5.5/5.6)** — Once lambdas land, these follow naturally. Partial application desugars into lambda + captured args.

4. **Generics (Stream B, 3.5)** — Highest effort single item. Benefits from method resolution being stable. Monomorphization is independent of lambda support.

5. **Async/await/spawn (Stream G, 5.2)** — Requires stable closures for `spawn`. State machine transformation is the largest remaining infrastructure piece.

Streams A (union types), D (safety analysis), E (optimization), and F (string interpolation) can run **in parallel with any step above** since they touch independent parts of the codebase.
