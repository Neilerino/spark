# Stream C: Lambdas & Functional Features

> Lambdas → Partial application → `each`/`each via` → `via` pipeline

## Overview

The largest dependency chain. Everything here requires function-as-value semantics. This stream is **blocked by Stream B (method resolution)** for the `via` pipeline operator, and should start after method resolution is stable.

**Conflicts with Stream B**: Both modify how functions/methods are resolved and called. `via` specifically needs method resolution to look up named operations.

**Design note**: `docs/design-notes/lambdas-closures-each.md` — comprehensive requirements document.

---

## Item 1: Function Types as First-Class Values

**Status**: `Ty::Function` exists but isn't used for first-class function values.

### What's Needed

- Allow variables to hold function values: `mapper: (Int): Int = double`
- Allow passing functions as arguments: `apply(data, transform)`
- Allow returning functions from functions

### Key Decisions

- **Representation**: Function pointer vs closure (fat pointer: fn ptr + env ptr). Recommend starting with bare function pointers for named functions, then adding closure support.
- **Type checking**: `Ty::Function` already has param types and return type. Wire it into assignment/call checking.

### Key Files
- `crates/spark_types/src/ty.rs` — `Ty::Function` definition
- `crates/spark_types/src/check_expr.rs` — call expression checking
- `crates/spark_resolve/src/lib.rs` — function name resolution

---

## Item 2: Lambda/Closure Conversion (P5.1)

**Status**: Warning in MIR lowering at `spark_mir/src/lower_expr.rs:294`.

### What's Needed

1. **Capture analysis**: Walk the lambda body, identify free variables (references to names defined in enclosing scopes). Key consideration: Spark is immutable-by-default, suggesting capture-by-value (clone/copy) with explicit `&` for reference capture.

2. **Closure environment struct**: For each lambda, generate a struct containing captured variables as fields. This is a synthetic type not visible to the user.

3. **Closure representation**: Fat pointer — function pointer + environment pointer. The function pointer points to the lambda body (compiled as a regular function with an extra env parameter). The environment pointer points to the heap-allocated capture struct.

4. **Borrow checker integration**: Captures that are references need lifetime tracking. Capture-by-value avoids most complexity here.

5. **Codegen**: Allocate closure environment on heap (RC'd like other heap types), emit function body with env parameter, emit closure construction at the lambda expression site.

6. **LSP `ScopeMap` integration**: The resolver should export persistent scope information for LSP code completion. Lambda scopes should record their parameters, captured variables, and local bindings in the `ScopeMap`. See design note Section 3 for full proposed API.

### Lambda Syntax (from lang spec)

```spark
// Inline lambda
(x: Int): Int { x * 2 }

// As argument to each
items each (x: Int): Void { print(x) }

// As argument to each via (map)
items each via (x: Int): Int { x * 2 }
```

### Key Files
- `crates/spark_ast/src/expr.rs` — `ExprKind::Lambda`
- `crates/spark_hir/src/lower_expr.rs` — lambda HIR lowering (currently Error)
- `crates/spark_mir/src/lower_expr.rs:294` — lambda MIR lowering (currently warning)
- `crates/spark_resolve/src/lib.rs` — scope/rib management for capture analysis
- `crates/spark_codegen/src/cranelift/` — function compilation

---

## Item 3: Partial Application

**Status**: Parsed as `ExprKind::Partial`, deferred in HIR.

### What's Needed

Partial application desugars into a lambda + captured args:
```spark
// Source
double{factor: 2}

// Desugared to (conceptually)
(x: Int): Int { double(x, factor: 2) }
```

Once lambdas work, this is straightforward desugaring in HIR.

### Key Files
- `crates/spark_ast/src/expr.rs` — `ExprKind::Partial`
- `crates/spark_hir/src/lower_expr.rs` — partial HIR lowering (currently Error)

---

## Item 4: `each`/`each via` Iteration (P5.5)

**Status**: Parsed, deferred in HIR (emits `HirExprKind::Error`).

**Blocked by**: Lambda/closure support (Items 1-2).

### What's Needed

`each` and `each via` must accept both:
- **Inline bodies**: `items each (x: Int): Void { print(x) }`
- **Function references**: `items each via double`
- **Partial applications**: `items each via multiply{factor: 2}`

An inline-only loop desugaring was rejected because it can't support function-as-value arguments.

### Semantics

| Keyword | Meaning | Scope |
|---------|---------|-------|
| `each` | Iterate for side effects (forEach) | Per-element |
| `each via` | Iterate + transform (map) | Per-element |

### Key Files
- `crates/spark_parser/src/expr.rs:524` — `each`/`each via` parsing
- `crates/spark_hir/src/lower_expr.rs` — HIR lowering (currently Error)
- `crates/spark_types/src/check_expr.rs` — type checking

### Future: ISequence

When `ISequence` (iterable interface) is implemented, two places need updating:
1. `iterable_element_type()` in the type checker — determines element type from receiver
2. The desugaring function in MIR — generates iteration code

---

## Item 5: `via` Pipeline Operator (P5.6)

**Status**: Currently parsed as `ExprKind::To` with `ToHandler::Pipeline`, deferred in HIR.

**Blocked by**: Lambdas (Items 1-2), method resolution (Stream B).

### What's Needed

`data via sort` — applies a named function (or partial) to the whole value. The left-hand side becomes the first argument to the right-hand side function.

```spark
// Standalone via — pipeline on whole value
numbers via sort
numbers via take{count: 5}

// Chaining
numbers
    via sort
    each via double
    via take{count: 5}
    each via toString
```

### Implementation

1. Split `ToHandler::Pipeline` into a new `ExprKind::Via` AST node (separate from `ExprKind::To` which is now type narrowing only)
2. Type-check: resolve the RHS as a function, verify the LHS type matches its first parameter
3. Lower to a function call: `via sort` on `numbers` → `sort(numbers)`

### Key Files
- `crates/spark_ast/src/expr.rs:229-237` — currently `ExprKind::To` with `ToHandler`
- `crates/spark_parser/src/expr.rs` — parser (needs new `via` token handling)
- `crates/spark_hir/src/lower_expr.rs:301-304` — HIR lowering (currently Error)

---

## Reference Docs

- Design note: `docs/design-notes/lambdas-closures-each.md` (comprehensive requirements)
- Language spec: `docs/lang-spec.md` (sections on lambdas, `each`, `via`)
- Roadmap: `docs/roadmap.md` (items 5.1, 5.5, 5.6)
