# Design Note: Lambdas, Closures, `each`/`each via`, and `via`

This document captures requirements and cross-cutting concerns for implementing lambdas, closures, the `each`/`each via` iteration operators, and the `via` pipeline operator. These features are tightly coupled and should be designed together.

---

## 1. `each`/`each via` Must Accept Function References

The `each` and `each via` operators are Spark's primary iteration primitives. They **must** support both inline bodies and named function references to enable clean chaining:

```
// Inline body (current parser syntax)
items each (x: Int): Void { print(x) }
items each via (x: Int): Int { x * 2 }

// Function reference (requires lambda/closure support)
items each via double
items each via transform

// Standalone via — pipeline on whole value (requires function-as-value)
numbers via sort
numbers via take{count: 5}

// Chaining with partials
numbers
    via sort
    each via double
    via take{count: 5}
    each via toString
```

If `each`/`each via`/`via` only support inline bodies, users are forced to write verbose wrappers like `items each via (x: Int): Int { double(x) }` — defeating the purpose of the chaining syntax.

### Implication

`each`, `each via`, and standalone `via` all require functions-as-values. The right-hand side can be either:
- An inline lambda: `(param: Type): RetType { body }`
- A function reference: `functionName`
- A partial application: `functionName{arg1: value}`

This means **lambdas/closures must land before `each`/`each via`/`via` can be fully implemented**. A loop-desugaring approach was considered for `each` but rejected because it only works for inline bodies.

### Keyword Semantics

| Keyword | Meaning | Scope |
|---------|---------|-------|
| `each` | Iterate for side effects | Per-element |
| `each via` | Iterate + transform (map) | Per-element |
| `via` | Apply function to whole value (pipeline) | Whole value |
| `to` | Type narrowing (union/error handling) | N/A — not function-based |

Note: `to` was previously overloaded for both type narrowing and pipeline composition. Pipeline was moved to `via` for clearer semantics. `to` is now exclusively for type narrowing and does **not** depend on lambdas.

---

## 2. Deferred MIR Constructs Needing Lambda Support

The following constructs currently emit `HirExprKind::Error` or warnings in the compiler pipeline because they require function-as-value semantics:

| Construct | Location | What's Needed |
|-----------|----------|---------------|
| `ExprKind::Each` | HIR lowering → Error | Function-as-value for body |
| `ExprKind::EachVia` | HIR lowering → Error | Function-as-value for body |
| `ExprKind::To` (pipeline) | HIR lowering → Error | Function-as-value for `via` operations |
| `ExprKind::Lambda` | HIR lowering → Error | Closure capture, function type |
| `ExprKind::Partial` | HIR lowering → Error | Partial application |
| `async`/`spawn` | MIR lowering → warning | Spawning closures |

Note: `ExprKind::To` currently handles both narrowing and pipeline. When `via` is split out as its own AST node, only the pipeline part (`ToHandler::Pipeline`) moves to `via`. The narrowing part (`ToHandler::Narrowing`) stays as `to` and does NOT need lambdas.

### Suggested Implementation Order

1. **Function types in the type system** — `Ty::Function` already exists but isn't used for first-class function values
2. **Lambda expressions** — capture analysis, closure representation
3. **Partial application** — desugar into lambda + captured args
4. **`each`/`each via`** — can now accept any callable (lambda, function ref, partial)
5. **`via` (pipeline)** — apply named function/partial to whole value
6. **`async`/`spawn`** — closures that run on separate tasks

---

## 3. LSP ScopeMap Requirement

**From the LSP team:** The LSP needs persistent scope information from `spark_resolve` to implement scope-aware code completion. This directly intersects with lambda/closure scope design.

### What's Needed: A Persistent `ScopeMap`

During name resolution, the `RibStack` tracks which names are visible at each scope level, but it's transient — it exists only during the resolution walk and is discarded. The LSP needs a way to query "what names are visible at byte offset X?" after resolution is complete.

### Proposed Export: `ScopeMap`

```rust
/// A scope entry recording which names became visible at a span boundary.
pub struct ScopeEntry {
    /// The span of this scope (e.g., function body, block).
    pub span: Span,
    /// Value-namespace names visible in this scope (accumulated from all enclosing ribs).
    pub values: Vec<(Symbol, DefId)>,
    /// Type-namespace names visible in this scope (accumulated from all enclosing ribs).
    pub types: Vec<(Symbol, DefId)>,
}

/// Persistent scope map built during resolution.
/// Maps scope spans to the names visible within them.
pub struct ScopeMap {
    scopes: Vec<ScopeEntry>,
}

impl ScopeMap {
    /// Returns all names visible at a given byte offset.
    /// Finds the innermost scope containing the offset and returns its names.
    pub fn visible_at(&self, offset: u32) -> Option<&ScopeEntry>;
}
```

### How the Resolver Would Populate It

At each `push_rib` / `pop_rib` boundary, snapshot the accumulated rib contents with the scope's span. The LSP can then binary-search for the innermost scope containing the cursor and enumerate its names.

### What This Enables

1. **General completion**: suggest all in-scope values/types at cursor position
2. **Scope-filtered results**: only show names actually visible (not every local in the file)
3. **Shadowing-aware**: innermost scope wins, matching runtime semantics
4. **Lambda capture awareness**: once lambda scopes are finalized, captured names flow naturally into the ScopeMap

### Lambda Interaction

When a lambda introduces a new scope, the `ScopeMap` should record:
- The lambda's own parameters
- Captured variables from enclosing scopes (which are "visible" inside the lambda)
- The lambda body's local bindings

This means the capture analysis done for lambda compilation naturally feeds into the LSP's completion data. **Design the capture analysis with `ScopeMap` population in mind.**

---

## 4. Open Questions

- **Capture semantics**: Does Spark capture by value (clone/copy) or by reference? Immutable-by-default suggests capture-by-value with explicit `&` for reference capture.
- **Closure representation**: Fat pointer (function ptr + environment ptr)? Or monomorphize like Rust? Fat pointer is simpler and fits the runtime's existing heap allocation model.
- **`each` on Map**: What's the iteration element? `Tup[K, V]`? Or separate `keys()`/`values()` methods? Map literals (`{k: v}`) need parser support first.
- **`each` on String**: Element type `Char` — needs `__spark_string_char_at(ptr, index)` runtime function.
