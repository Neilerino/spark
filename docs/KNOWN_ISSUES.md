# Known Issues & Loose Ends

## Closures / Lambda

### Captured RC values not decremented when env is freed (Phase 2)

Closure environments are now reference-counted (16-byte header with `rc` and `alloc_size`). When the last reference to an env is dropped, `__spark_env_rc_dec` frees the env memory. However, captured values inside the env that are themselves RC'd (e.g., captured Strings, Lists) are **not** recursively decremented. This means if a closure captures a String, the String will leak when the env is freed.

**Impact**: Programs that capture RC'd values in closures will leak those captured values (but no longer leak the env itself).

**Future fix**: `__spark_env_rc_dec` needs a type-dispatch mechanism to know which captures are RC'd and call the appropriate `_rc_dec` on them before freeing the env. This likely requires passing capture metadata (offsets + type flags) to the runtime, or generating per-closure-type destructor functions at codegen time.

### Capture-by-value only

Closures capture variables by value (copy). Mutations to the original variable after closure creation are not visible inside the closure, and vice versa. This matches Spark's immutable-by-default philosophy but means there is no way to create a closure that shares mutable state with its enclosing scope.

This is a deliberate design choice, not a bug — but worth noting as it differs from languages like Python or JavaScript where closures capture by reference.

### Nested closures are untested

A closure that captures another closure (i.e., captures a variable of type `Ty::Function`) should work in theory — the inner closure stores the entire 16-byte fat pointer in its environment. However, there are no E2E tests for this case. Edge cases around aggregate capture layout could surface.

### No recursive closures

A closure cannot refer to itself. Named recursive functions work fine, but `f = fn(x) { ... f(x-1) ... }` would try to capture `f` before it's assigned. This is a fundamental limitation of capture-by-value semantics and would require explicit fixed-point combinators or special syntax to support.

## Codegen / ABI

### Sret is the only aggregate return mechanism

Functions returning aggregates (structs, enums, function values) use the sret calling convention: the caller passes a pointer to a stack slot as a hidden first parameter. This is correct and efficient, but:

- There is no small-struct register return optimization (e.g., returning two-field structs in two registers). All aggregates go through sret regardless of size.
- The sret pointer is not marked with Cranelift's `ArgumentPurpose::StructReturn`, which means Cranelift's ABI layer doesn't know it's an sret. This could cause issues if Cranelift ever applies ABI-specific optimizations based on that annotation.

### Thunks generated for all named functions

Every non-lambda function gets a `__thunk_<name>` wrapper generated (for use when the function is referenced as a value). Most of these thunks are never called. This bloats object file size slightly but has no runtime cost for unused thunks.

**Future optimization**: Only generate thunks for functions that are actually used as values (detectable from MIR analysis of `Constant::FunctionRef` usage in non-call positions).

## References (`Ref`)

### No implicit borrow coercion for local `Ref` bindings

The lang spec describes creating local references via type annotation:

```spark
r: Ref[Int, A] = x   // should implicitly borrow x
```

The backend infrastructure is fully wired (parser, type representation, borrow checker, MIR lowering, codegen), but the type checker doesn't insert an implicit borrow when a `Ref[T]`-typed binding is initialized from a plain `T` value. It reports a type mismatch instead.

**To fix**: The type checker needs to recognize the `Ref[T] = expr_of_type_T` pattern and wrap the RHS in an implicit `HirExprKind::Borrow` during type checking or HIR lowering. Ideally this should also work with literal values (e.g., `x: Ref[Int] = 123`), where the compiler allocates a temporary, stores the value, and takes a reference — similar to Rust's automatic referencing.

### `Ref[T]` without lifetime rejected by parser

Writing `Ref[Int]` (omitting the lifetime) falls through to the named-type path and produces "unresolved type `Ref`". The parser's special `Ref` handling requires at least 2 type arguments (`Ref[Type, Lifetime]`). The lang spec shows `Ref[User]` as valid syntax (with an inferred lifetime).

**To fix**: Handle the 1-argument case in the parser's `Ref` branch, defaulting the lifetime to `A`.

## General

### E2E tests are serialized

All E2E tests share a single temp path (`/tmp/spark_output.o`) for the intermediate object file, so they run sequentially behind a mutex. This makes the E2E test suite slower than it needs to be. A per-test temp file would allow parallel execution.
