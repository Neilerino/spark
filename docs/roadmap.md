# Spark Compiler — Feature Roadmap

This document tracks all features remaining to be implemented, organized by priority and dependency. It reflects the state of the compiler after completing all 10 pipeline phases (lexer through codegen).

## Current State

The compiler has a complete pipeline from source to native object files:

```
Source → Lexer → Parser → Modules → Resolve → Types → HIR → MIR → Borrow → Codegen
  ✅       ✅      ✅       ✅        ✅        ✅     ✅    ✅      ✅        ✅
```

**What works**: 540+ tests pass. Programs using scalars (Int, Float, Bool, Char), function calls, if/else, match, structs, and enums compile to correct native machine code.

**What's missing**: No driver to produce executables from source files. No runtime library for heap-allocated types. Several language features deferred with stubs/warnings.

---

## Priority 1: Make Programs Runnable

These are the critical gaps preventing end-to-end execution of Spark programs.

### 1.1 Compiler Driver (`spark` CLI)

**Status**: Not started
**Blocking**: Everything — without this, no Spark program can be compiled from source

The driver chains all pipeline phases and invokes the system linker:

```
spark build program.spark → output.o → cc -o program → ./program
```

Needs to:
- Read source file(s) from disk
- Run the full pipeline: lex → parse → module resolution → name resolution → type checking → HIR → MIR → borrow checking → codegen
- Write `.o` to disk
- Invoke system linker (`cc`) to produce executable
- Report diagnostics to stderr with ariadne rendering

### 1.2 Runtime Library (`spark_runtime`)

**Status**: Not started
**Blocking**: Any program using String, List, Map, Set, or reference counting

The codegen declares these as unresolved external symbols (`Linkage::Import`):

| Symbol | Purpose |
|--------|---------|
| `__spark_string_new` | Allocate empty string |
| `__spark_string_from_literal` | String from data pointer + length |
| `__spark_list_new` | Allocate empty list |
| `__spark_map_new` | Allocate empty map |
| `__spark_set_new` | Allocate empty set |
| `__spark_rc_release` | Drop reference-counted object |

**Approach**: Thin Rust crate wrapping standard library types behind `extern "C"` FFI:

| Spark type | Rust backing | Representation |
|-----------|-------------|----------------|
| `String` | `Box<String>` | Heap pointer |
| `List[T]` | `Box<Vec<u8>>` | Type-erased, element-size tracked |
| `Map[K,V]` | `Box<HashMap<...>>` | Type-erased |
| `Set[T]` | `Box<HashSet<...>>` | Type-erased |
| RC | `Box::from_raw` + drop | Standard Rust drop |

Compiled as `crate-type = ["staticlib"]` and linked alongside the program's `.o` file.

**Collection generics**: Three possible approaches:
1. **Type-erased** (simplest) — store raw bytes + element size, memcpy operations
2. **Monomorphized** (fastest) — codegen emits `__spark_list_new_i64`, etc.
3. **Fat pointer** (most flexible) — pass vtable alongside collection pointer

Type-erased is the pragmatic starting point.

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

### 2.3 Drop Elaboration

**Status**: `Statement::Drop` exists in MIR but is a no-op in codegen
**Location**: `spark_codegen/src/cranelift/emit_stmt.rs:56`
**Blocker was**: No codegen to emit drop calls

**Fix**: Two parts:
1. **MIR pass**: Insert `Statement::Drop` at scope exits, before returns, and after moves
2. **Codegen**: Emit calls to `__spark_rc_release` for reference-counted types, no-op for scalars

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

| Priority | Feature | Effort | Impact |
|----------|---------|--------|--------|
| **P1** | Compiler driver | Medium | Enables running any program |
| **P1** | Runtime library | Medium | Enables String/List/Map/Set |
| **P2** | `??` / `?.` desugaring | Low | Fixes null-handling operators |
| **P2** | `is` type test | Low | Enables type checking expressions |
| **P2** | Drop elaboration | Medium | Enables resource cleanup |
| **P3** | Match exhaustiveness | Medium | Safety — catches missing cases |
| **P3** | Lifetime elision | Low | Ergonomics for references |
| **P3** | `!` propagation | Medium | Error handling ergonomics |
| **P3** | Method resolution | Medium-High | Enables dot-call syntax |
| **P3** | Generics | High | Enables parameterized types |
| **P4** | `spark_opt` passes | High | Performance optimization |
| **P4** | Reborrowing | Low | Borrow checker ergonomics |
| **P5** | Lambdas/closures | High | First-class functions |
| **P5** | Async/await/spawn | Very High | Concurrency model |
| **P5** | String interpolation | Medium | Syntax convenience |
