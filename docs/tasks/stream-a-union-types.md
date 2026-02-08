# Stream A: Union Type Handling

> `!` propagation → `to` type narrowing → `??`/`?.` desugaring

## Overview

These features all need **union type analysis** — the ability to distinguish success vs error variants in union types — and **fresh `DefId` synthesis** in HIR desugaring. They share infrastructure and should be done together.

**Conflicts**: None with other streams. Touches `spark_types` and `spark_hir` primarily.

---

## Shared Infrastructure: Union Type Analysis

Before any of the individual items, build a utility that can inspect a `Ty::Union(Vec<TypeId>)` and classify its members:

- Which members are "error" types? (Convention TBD — likely types named with `Error` suffix, or a marker)
- Which member is the "success" type? (The non-error member)
- Is this union an `Optional` (i.e., `T | Null`)?

**Key file**: `crates/spark_types/src/ty.rs:172-173` — `Union(Vec<TypeId>)` with sorted, deduplicated members.

**Key file**: `crates/spark_types/src/table.rs` — `TypeTable` for interned type lookups.

---

## Item 1: `??` Coalescing and `?.` Optional Chain (P2.1)

**Status**: Partially desugared, has bugs in DefId synthesis.

### Current State

- **`??` (Coalesce)**: Parsed as `ExprKind::Coalesce { lhs, rhs }` (`spark_ast/src/expr.rs:242-248`). HIR desugaring at `spark_hir/src/desugar.rs:14-55` converts to match expression. Type checking at `spark_types/src/check_expr.rs:264-279` validates optional type.

- **`?.` (Optional field)**: Parsed as `ExprKind::OptionalField { base, field }` (`spark_ast/src/expr.rs:113-119`). HIR desugaring at `spark_hir/src/desugar.rs:57-104`.

### What's Broken

Desugaring creates match arms that need fresh bindings (e.g., `match x { v -> v, Null -> default }`), but can't allocate new `DefId`s. The HIR lowering context needs a `DefId` allocator.

### Fix

Add a `DefId` counter/allocator to `HirLoweringContext` so desugaring can create synthetic `DefId`s for match bindings. Register these in the `DefinitionTable`.

---

## Item 2: `!` Propagation (P3.3)

**Status**: Partially desugared in HIR.

### Current State

- Parsed as `ExprKind::Propagate(Box<Expr>)` (`spark_ast/src/expr.rs:250-251`)
- HIR desugaring at `spark_hir/src/desugar.rs:106-150` — partial implementation
- Type checking at `spark_types/src/check_expr.rs:281-286` — unwraps optional/union but needs union type analysis

### What's Needed

Union type analysis to distinguish success vs error variants. Desugar `expr!` to:
```
match expr {
    ok_value -> ok_value,
    err_value -> return err_value
}
```

Requires the containing function's return type to include the error variant in its union.

---

## Item 3: `to` Type Narrowing (P3.6)

**Status**: Parsed, deferred in HIR (emits `HirExprKind::Error`).

### Current State

- Parsed as `ExprKind::To { expr, handler: ToHandler::Narrowing(...) }` (`spark_ast/src/expr.rs:229-237`)
- HIR lowering at `spark_hir/src/lower_expr.rs:301-304` — currently deferred to error
- Type checking at `spark_types/src/check_expr.rs:367-370` — returns `TypeId::ERROR`

### What's Needed

`result to { NotFound -> ..., DbError(e) -> ... }` desugars to a `match` on the union type's error variants. Shares union type analysis infrastructure with `!` propagation.

Note: `to` is exclusively for type narrowing. Pipeline operations use `via` (see Stream C).

---

## Key Files

| Purpose | Path |
|---------|------|
| Union type definition | `crates/spark_types/src/ty.rs:172-173` |
| Type table / interning | `crates/spark_types/src/table.rs` |
| Coalesce desugaring | `crates/spark_hir/src/desugar.rs:14-55` |
| Optional field desugaring | `crates/spark_hir/src/desugar.rs:57-104` |
| Propagate desugaring | `crates/spark_hir/src/desugar.rs:106-150` |
| HIR lowering (To) | `crates/spark_hir/src/lower_expr.rs:301-304` |
| Type checking (all) | `crates/spark_types/src/check_expr.rs` |
| Definition table | `crates/spark_resolve/src/lib.rs` |

## Reference Docs

- Language spec: `docs/lang-spec.md` (sections on `to`, `??`, `?.`, `!`)
- Compiler architecture: `docs/compiler-architecture.md`
- Roadmap: `docs/roadmap.md` (items 2.1, 3.3, 3.6)
