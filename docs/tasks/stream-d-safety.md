# Stream D: Safety & Analysis

> Independent analysis passes

## Overview

These are independent of each other and of most other streams. Pure analysis passes that improve correctness and ergonomics without changing the core compilation pipeline.

**Conflicts**: None. These are read-only analysis passes in `spark_types` and `spark_borrow`. Can safely run in parallel with any other stream.

---

## Item 1: `is` Type Test (P2.2)

**Status**: Warning in MIR lowering. HIR node exists and type-checks correctly.

### Current State

- Parsed as `ExprKind::Is { expr, ty }` (`spark_ast/src/expr.rs:256-262`)
- HIR lowering at `spark_hir/src/lower_expr.rs:246-250` — converts to `HirExprKind::Is { expr, target_ty }`
- Type checking at `spark_types/src/check_expr.rs:288-292` — validates and returns `Bool`
- MIR lowering at `spark_mir/src/lower_expr.rs:294` — currently emits warning and returns `Bool(false)`

### What's Needed

Lower `HirExprKind::Is` to MIR:

1. Evaluate the expression to get the value
2. Extract the discriminant (`Rvalue::Discriminant` already exists in MIR)
3. Compare against the target type's variant tag (`SwitchInt` already exists)
4. Return `Bool` result

For enum/union types this is a straightforward integer comparison. All the MIR building blocks exist — this is primarily wiring them together.

### Key Files
- `crates/spark_mir/src/lower_expr.rs:294` — MIR lowering (currently deferred)
- `crates/spark_mir/src/mir.rs` — `Rvalue::Discriminant`, `Terminator::SwitchInt`
- `crates/spark_hir/src/hir.rs:227-233` — `HirExprKind::Is`
- `crates/spark_types/src/check_expr.rs:288-292` — type checking

---

## Item 2: Match Exhaustiveness Checking (P3.1)

**Status**: Not implemented. Non-exhaustive matches compile but hit `Unreachable` trap at runtime.

### Current State

- Match type checking at `spark_types/src/check_expr.rs:174-194` — validates arm types but no exhaustiveness check
- Pattern types in `spark_hir/src/hir.rs:318-351`:
  - `HirPattern::Wildcard` — matches anything
  - `HirPattern::Binding` — binds matched value
  - `HirPattern::Literal` — matches specific literal
  - `HirPattern::Variant` — matches enum variant
  - `HirPattern::Struct` — matches struct fields
  - `HirPattern::Tuple` — matches tuple elements
  - `HirPattern::Or` — matches any of multiple patterns
  - `HirPattern::Error` — error recovery

### What's Needed

Pure analysis pass in the type checker. For each match expression:

1. Collect the scrutinee type and all patterns
2. For enum types: verify all variants are covered
3. For union types: verify all member types are covered
4. For Bool: verify both `true` and `false` are covered (or wildcard)
5. For Int/String: require a wildcard/catch-all arm
6. Report missing patterns as errors
7. Report unreachable arms as warnings (arm after wildcard, duplicate patterns)

### Algorithm

Consider the "usefulness" algorithm (similar to Rust's exhaustiveness checker):
- A pattern is useful if there exists a value that matches it but doesn't match any previous pattern
- A match is exhaustive if the "catch-all" pattern `_` is not useful after all arms

For Spark's simpler pattern system (no nested destructuring yet), a simpler variant-counting approach may suffice.

### Key Files
- `crates/spark_types/src/check_expr.rs:174-194` — match type checking (add exhaustiveness here)
- `crates/spark_hir/src/hir.rs:318-351` — HirPattern variants
- `crates/spark_types/src/ty.rs` — type definitions (Enum variants, Union members)

---

## Item 3: Lifetime Elision Rules (P3.2)

**Status**: Not implemented — all lifetimes are `LifetimeId::ANONYMOUS`.

### Current State

- `LifetimeId` at `spark_types/src/ty.rs:11-33` — `ANONYMOUS = LifetimeId(0)` constant
- `Ty::Ref { inner, mutable, lifetime }` at `spark_types/src/ty.rs:178-186`
- All references currently get `LifetimeId::ANONYMOUS`
- Borrow checker works but can't distinguish lifetimes on multi-reference functions

### What's Needed

Implement Spark's lifetime elision rules (similar to Rust's):

1. **Single input reference**: The output reference gets the same lifetime as the input
   ```spark
   fn first(list: &List[Int]): &Int  // elided: both share lifetime 'a
   ```

2. **Multiple input references**: Require explicit lifetime annotations (error if elided)
   ```spark
   fn longer(a: &String, b: &String): &String  // error: ambiguous lifetime
   ```

3. **`self` parameter**: If a method has `&self`, the output reference gets `self`'s lifetime

### Where It Goes

In `spark_types/src/lower.rs` where `TypeExpr` → `TypeId` conversion happens. After converting function signatures, apply elision rules to assign concrete `LifetimeId`s to references that use `ANONYMOUS`.

### Key Files
- `crates/spark_types/src/ty.rs:11-33` — LifetimeId definition
- `crates/spark_types/src/ty.rs:178-186` — Ref type with lifetime
- `crates/spark_types/src/lower.rs` — TypeExpr → TypeId conversion
- `crates/spark_borrow/src/lib.rs` — borrow checker (consumer of lifetime info)

---

## Reference Docs

- Language spec: `docs/lang-spec.md` (sections on `is`, match, references)
- Compiler architecture: `docs/compiler-architecture.md`
- Roadmap: `docs/roadmap.md` (items 2.2, 3.1, 3.2)
