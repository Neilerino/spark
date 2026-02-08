# Spark Compiler — Feature Roadmap

This document tracks all features remaining to be implemented, organized by priority and dependency.

## Current State

The compiler has a complete pipeline from source to native executables:

```
Source → Lexer → Parser → Modules → Resolve → Types → HIR → MIR → Borrow → Codegen → Link
  ✅       ✅      ✅       ✅        ✅        ✅     ✅    ✅      ✅        ✅       ✅
```

**What works**: 568 tests pass. Programs using scalars (Int, Float, Bool, Char), strings, function calls, if/else, loops, match, structs, enums, and collection literals (List, Map, Set) compile to native executables and run correctly. `print` supports Int, Float, Bool, and String arguments. List/Map index access (`list[i]`, `map[key]`) works via runtime calls. Reference counting and drop elaboration prevent memory leaks for all heap-allocated types. Install script (`install.sh`) provided.

```spark
fn main(): Int {
    numbers: List[Int] = [10, 20, 30]
    print("hello world")
    print(42)
    print(true)
    0
}
```

**What's missing**: Collection methods (`.append()`, `.len()`) not yet supported. COW mutation hooks prepared but not wired (no mutable collection operations yet). Several language features deferred with stubs/warnings (closures, async, generics, method resolution).

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

**COW functions prepared** (not yet wired into codegen): `__spark_string_cow`, `__spark_list_cow`, `__spark_map_cow` — if RC=1 return as-is; if RC>1 copy buffer, dec old, return new with RC=1.

**Not yet implemented**: Collection methods (`.append()`, `.len()`, `.remove()`, etc.), proper `Optional` wrapping on index access, set literals (parser ambiguity with blocks).

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

### 1.3 COW Mutation Hooks

**Status**: Runtime functions prepared, codegen not wired
**Depends on**: Collection methods (1.4)
**Impact**: Enables safe mutation of shared values

Before any mutable collection operation (append, index-assign, remove), emit COW uniqueness check. If RC>1, copy buffer first. Update local to point to new allocation. Wire up when collection methods are implemented.

### 1.4 Collection Methods

**Status**: Not implemented
**Impact**: No `.append()`, `.len()`, `.remove()`, `.contains()`, etc.

Needs method resolution infrastructure (P3.4) or special-cased built-in method dispatch similar to `print`.

---

## Priority 2: Previously Blocked, Now Unblocked

These features were deferred because downstream pipeline stages didn't exist yet. Now that the full pipeline is complete, they can be implemented.

### 2.1 Coalesce `??` and Optional Chain `?.`

**Status**: Partially desugared in HIR with `HirExprKind::Error` placeholders
**Location**: `spark_hir/src/desugar.rs:35` (coalesce), `spark_hir/src/desugar.rs:80` (optional chain)
**Blocker was**: Fresh `DefId` synthesis — desugaring creates match arms that need bindings, but can't allocate new `DefId`s

**Fix**: Add a `DefId` allocator to the HIR lowering context. When desugaring `x ?? default`:
```
// Current (broken):    match x { _ -> Error, Null -> default }
// Needed:              match x { v -> v, Null -> default }
```
Create a synthetic `DefId` for the binding `v`, register it in the `DefinitionTable`.

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

### 3.3 Propagate `!` Operator

**Status**: Partially desugared in HIR
**Location**: `spark_hir/src/desugar.rs:118`
**Impact**: Union-type error propagation doesn't work

Needs union type analysis to distinguish success vs error variants, then desugar to:
```
match expr { ok -> ok, err -> return err }
```

### 3.4 Method Resolution and Interface Bindings

**Status**: Not implemented — method calls return `TypeId::ERROR`
**Location**: Would go in `spark_types` and `spark_resolve`
**Impact**: No dot-call syntax, no interface validation

Two parts:
1. **Interface validation**: Verify `binding Foo for Bar { ... }` implements all required methods
2. **Method resolution**: `expr.method(args)` looks up method via type's bindings

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

### 4.1 `spark_opt` — MIR Optimization Passes

**Status**: Not started (planned as next main crate)
**Location**: Inserts between `spark_borrow` and `spark_codegen`
**Impact**: Performance only — correctness is unaffected

Planned passes:
- **Constant folding**: Evaluate constant expressions at compile time
- **Dead code elimination**: Remove unreachable blocks and unused assignments
- **SSA construction**: Convert pre-SSA MIR to SSA form with phi nodes
- **Copy propagation**: Eliminate redundant copies
- **Decision-tree match compilation**: Optimize sequential pattern tests into switch trees

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

Needs:
- Closure capture analysis (identify free variables in lambda body)
- Closure environment struct (captured variables as fields)
- Closure representation (function pointer + environment pointer)
- Borrow checker support for captures
- Codegen for closure allocation and invocation

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
| **P1** | COW mutation hooks | Low | Safe mutation of shared values | Prepared |
| **P1** | Collection methods | Medium | `.append()`, `.len()`, etc. | Not started |
| **P2** | `??` / `?.` desugaring | Low | Fixes null-handling operators | Partial |
| **P2** | `is` type test | Low | Enables type checking expressions | MIR stub |
| **P3** | Match exhaustiveness | Medium | Safety — catches missing cases | Not started |
| **P3** | Lifetime elision | Low | Ergonomics for references | Not started |
| **P3** | `!` propagation | Medium | Error handling ergonomics | Partial |
| **P3** | Method resolution | Medium-High | Enables dot-call syntax | Not started |
| **P3** | Generics | High | Enables parameterized types | Not started |
| **P4** | `spark_opt` passes | High | Performance optimization | Not started |
| **P4** | Reborrowing | Low | Borrow checker ergonomics | Not started |
| **P5** | Lambdas/closures | High | First-class functions | MIR stub |
| **P5** | Async/await/spawn | Very High | Concurrency model | MIR stub |
| **P5** | String interpolation | Medium | Syntax convenience | Not started |
