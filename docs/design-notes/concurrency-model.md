# Design Note: Concurrency Model

This document describes Spark's concurrency architecture. It supersedes `docs/tasks/stream-g-concurrency.md`.

**Prerequisites**: Lambdas and closures are complete (Stream C). The fat pointer ABI `(fn_ptr, env_ptr)` and `each`/`each via` iteration operators are stable.

---

## Dual Runtime Architecture

Spark uses two complementary concurrency backends, both hidden behind the FFI wall in `spark_runtime`:

| Dimension | I/O Concurrency (Tokio) | CPU Parallelism (Rayon) |
|-----------|------------------------|------------------------|
| **Use case** | Network, file I/O, timers, 1M+ tasks | Data transforms, math, batch processing |
| **Spark syntax** | `async fn`, `await`, `spawn` | `parallel each`, `parallel each via` |
| **Mechanism** | Cooperative async, epoll/kqueue | Work-stealing thread pool |
| **Runtime FFI** | `__spark_spawn`, `__spark_await`, `__spark_poll` | `__spark_parallel_map`, `__spark_parallel_foreach` |

The language user never picks a backend — the syntax determines which one is used. The two can coexist in the same program.

---

## Part 1: CPU Parallelism — `parallel each` / `parallel each via`

### Syntax

`parallel each` and `parallel each via` are syntactically identical to their sequential counterparts but dispatch to a Rayon-backed runtime function:

```spark
// Sequential (existing)
doubled: List[Int] = nums each via (n: Int): Int { n * 2 }

// Parallel (new)
doubled: List[Int] = nums parallel each via (n: Int): Int { n * 2 }

// Side-effect iteration
nums parallel each (n: Int): Void { process(n) }

// Function reference (existing callback ABI)
doubled: List[Int] = nums parallel each via double
```

### Why This Is Low-Risk

No state machine transformation needed. The callback is the same fat pointer `(fn_ptr, env_ptr)` already used by regular `each via`. Rayon's `par_iter` splits the work across cores internally. From the compiler's perspective, the only difference is which runtime stub gets called.

### Runtime FFI

```c
// parallel each via — returns new list with mapped elements
SparkList* __spark_parallel_map(
    SparkList* list,
    void* fn_ptr,           // callback function pointer
    void* env_ptr,          // closure environment (may be null)
    size_t result_elem_size // byte size of output element
);

// parallel each — side effects only, no return value
void __spark_parallel_foreach(
    SparkList* list,
    void* fn_ptr,
    void* env_ptr
);
```

### Implementation in the Runtime

```rust
// spark_runtime/src/parallel.rs
use rayon::prelude::*;

#[no_mangle]
pub extern "C" fn __spark_parallel_map(
    list: *mut SparkList,
    fn_ptr: extern "C" fn(*mut u8, *const u8) -> *mut u8,
    env_ptr: *mut u8,
    result_elem_size: usize,
) -> *mut SparkList {
    // Safety: list is a valid SparkList from the Spark runtime
    let items: Vec<_> = (0..list.len)
        .into_par_iter()
        .map(|i| {
            let elem = list.get_element(i);
            fn_ptr(env_ptr, elem)
        })
        .collect();
    SparkList::from_vec(items, result_elem_size)
}
```

### Compiler Pipeline Changes

**Keyword**: Add `PARALLEL` to `spark_intern/src/lib.rs` pre-interned symbols. Two options:

- **Option A — Full keyword**: Add `Parallel` to `TokenKind`. Simplest for the parser.
- **Option B — Contextual keyword**: Only recognized before `each`. Avoids reserving an identifier. Slightly more complex parsing.

Recommendation: **Option A** (full keyword). `parallel` is unlikely to be a user variable name and keeps the parser simple.

**AST**: New nodes in `spark_ast/src/expr.rs`:

```rust
ParallelEach {
    iterable: Box<Expr>,
    callback: Box<Expr>,
}
ParallelEachVia {
    iterable: Box<Expr>,
    callback: Box<Expr>,
}
```

**Parser**: After parsing `expr`, if the next token is `parallel`, consume it, expect `each` (with optional `via`), and parse the callback expression. Reuses existing callback parsing from `each`/`each via`.

**Type checker**: Same validation as `each`/`each via` — `validate_each_callback()` checks the callback signature. The only addition: the iterable must be a `List[T]` (maps and sets don't have meaningful parallel iteration order).

**HIR lowering**: Desugar identically to `each`/`each via` but emit calls to `__spark_parallel_map` / `__spark_parallel_foreach` instead of the sequential runtime stubs.

**MIR / Codegen**: No changes. The desugared HIR uses the same call mechanics — just different `DefId`s pointing to different runtime stubs.

### Thread Safety Constraint

Callbacks passed to `parallel each` must not capture mutable state. Since Spark captures by value (COW semantics), this is naturally enforced — each thread gets its own copy. No language-level changes needed.

---

## Part 2: I/O Concurrency — `async`/`await`/`spawn`

### Phased Implementation

Each phase is independently shippable and testable.

### Phase 1 — Synchronous Shim

**Goal**: Validate the type system without state machine complexity.

- `async fn foo(): Int` compiles as a regular function returning `Future[Int]`
- `await expr` is a no-op that unwraps `Future[T]` to `T`
- `spawn expr` evaluates `expr` immediately, wraps result in `Future[T]`

**Type system changes**:

```rust
// spark_types/src/ty.rs
Ty::Future { inner: TypeId }
```

- `async fn foo(): T` has return type `Future[T]`
- `await expr` requires `expr: Future[T]`, produces `T`
- `spawn expr` requires `expr: Future[T]` (from calling an async fn), produces `Future[T]`

**MIR changes**: Replace the warning stubs at `spark_mir/src/lower_expr.rs` (lines ~299, ~304) with real lowering:

- `await` → call the inner expression, assign to dest (synchronous in Phase 1)
- `spawn` → call the inner expression, assign to dest (synchronous in Phase 1)

**Runtime**: `__spark_await(future_ptr) -> value` just returns the already-computed value. `__spark_spawn(fn_ptr, env_ptr) -> future_ptr` calls the function immediately.

**Tests**: Verify type checking (`await` on non-future is error, `spawn` of non-async is error), verify programs produce correct results (same as synchronous).

### Phase 2 — Spawn + Join (Blocking)

**Goal**: Enable concurrent I/O by running spawned tasks on Tokio's thread pool.

- `spawn` queues a closure on Tokio's runtime, returns `Future[T]`
- `await` blocks the calling thread until the result is ready

This is simple but enables real concurrency — spawned tasks run in parallel.

**Runtime changes**:

```rust
// spark_runtime/src/async_rt.rs
use tokio::runtime::Runtime;
use std::sync::OnceLock;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

fn get_runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        Runtime::new().expect("failed to create Tokio runtime")
    })
}

#[no_mangle]
pub extern "C" fn __spark_spawn(
    fn_ptr: extern "C" fn(*mut u8) -> *mut u8,
    env_ptr: *mut u8,
) -> *mut SparkFuture {
    let handle = get_runtime().spawn_blocking(move || {
        fn_ptr(env_ptr)
    });
    SparkFuture::new(handle)
}

#[no_mangle]
pub extern "C" fn __spark_await(future: *mut SparkFuture) -> *mut u8 {
    get_runtime().block_on(future.join_handle.take().unwrap())
}
```

**`SparkFuture`** is a heap-allocated struct:

```rust
#[repr(C)]
pub struct SparkFuture {
    rc: usize,
    handle: Option<tokio::task::JoinHandle<*mut u8>>,
}
```

RC-managed like other Spark heap types. `__spark_future_rc_dec` added.

**Codegen**: `Ty::Future` is an aggregate (pointer to `SparkFuture`), stored as `StackSlot` if used as local, treated like other RC'd types.

**Limitation**: `await` inside an async function blocks the Tokio worker. This is fine for Phase 2 — programs behave correctly, just without cooperative scheduling.

### Phase 3 — Cooperative Async (State Machine)

**Goal**: Enable 1M+ concurrent tasks via cooperative scheduling.

Each `await` point in an async function becomes a state transition. This is structurally similar to closure capture analysis — locals live across await points are captured in the state struct.

**MIR transformation** (new pass in `spark_mir`):

1. **Identify yield points**: Every `await` in an async function body
2. **Liveness analysis**: Determine which locals are live across each yield point (reuse `spark_borrow`'s liveness infrastructure)
3. **State enum generation**: One variant per yield point, fields are live locals

```
// Conceptual output for:
//   async fn fetch(): String {
//       data = await http_get(url)
//       result = await parse(data)
//       return result
//   }

struct FetchState {
    state_tag: u8,        // which yield point
    locals: union {
        s0: { url: *SparkString },              // before first await
        s1: { data: *SparkString },             // between awaits
        s2: { result: *SparkString },           // after second await
    }
}

fn fetch_poll(state: *mut FetchState) -> PollResult {
    match state.state_tag {
        0 => { /* start http_get, save to s0, transition to 1 */ }
        1 => { /* check http_get result, start parse, transition to 2 */ }
        2 => { /* check parse result, return Ready(result) */ }
    }
}
```

**Runtime changes**: Replace blocking `__spark_await` with a poll-based executor:

```rust
#[no_mangle]
pub extern "C" fn __spark_poll(future: *mut SparkFuture) -> PollResult {
    // Returns Ready(*mut u8) or Pending
}
```

The executor runs a cooperative loop: poll each ready task, park pending tasks, wake on I/O readiness (via Tokio's reactor).

**Codegen**: Async functions emit two artifacts:
- The state struct (allocated on the heap via `__spark_env_alloc` or a dedicated allocator)
- The poll function (switch on state tag, resume at the right point)

This is the most complex phase. The liveness-across-yields analysis is similar to what `spark_borrow` already does but applied at MIR level before codegen.

### Phase 4 — I/O Primitives

Depends on the standard library design (see `docs/design-notes/standard-library.md`). These are Tokio-backed async functions exposed through `spark.net`, `spark.http`, etc.

```spark
import spark.http { get }

async fn fetch_data(url: String): String | HttpError {
    response: String | HttpError = await get(url)
    return response
}
```

---

## Tokio + Rayon Interaction

### The Problem

Calling `parallel each via` (Rayon, blocking) inside an `async fn` would block a Tokio worker thread, degrading throughput.

### The Solution

The runtime's `__spark_parallel_map` detects whether it's running inside a Tokio context and, if so, uses `tokio::task::spawn_blocking()` to offload Rayon work to a dedicated thread pool:

```rust
#[no_mangle]
pub extern "C" fn __spark_parallel_map(/* ... */) -> *mut SparkList {
    if tokio::runtime::Handle::try_current().is_ok() {
        // Inside async context — offload to blocking pool
        tokio::task::block_in_place(|| {
            rayon_parallel_map(/* ... */)
        })
    } else {
        // Synchronous context — run directly
        rayon_parallel_map(/* ... */)
    }
}
```

This is a runtime implementation detail. The user's Spark code is identical either way. No language-level changes needed.

---

## Key Files to Modify

| Area | Files | Phase |
|------|-------|-------|
| Keywords | `spark_intern/src/lib.rs` — add `PARALLEL`, `ASYNC`, `AWAIT`, `SPAWN` | All |
| Lexer | `spark_lexer/src/token.rs` — add `Parallel` token kind (if full keyword) | Rayon |
| AST | `spark_ast/src/expr.rs` — `ParallelEach`, `ParallelEachVia` | Rayon |
| Type system | `spark_types/src/ty.rs` — add `Ty::Future { inner: TypeId }` | Phase 1 |
| Type checker | `spark_types/src/check_expr.rs` — validate async/await/spawn types | Phase 1 |
| MIR | `spark_mir/src/lower_expr.rs` — replace stubs (~lines 299, 304) | Phase 1 |
| MIR | `spark_mir/src/async_transform.rs` (new) — state machine pass | Phase 3 |
| Runtime | `spark_runtime/Cargo.toml` — add `rayon` (behind `parallel` feature) | Rayon |
| Runtime | `spark_runtime/Cargo.toml` — add `tokio` (behind `async` feature) | Phase 2 |
| Runtime | `spark_runtime/src/parallel.rs` (new) | Rayon |
| Runtime | `spark_runtime/src/async_rt.rs` (new) | Phase 2 |
| Codegen | `spark_codegen/src/cranelift/emit_expr.rs` — Future type handling | Phase 1 |
| Drop | `spark_drop/src/lib.rs` — `needs_rc(Ty::Future)` returns true | Phase 2 |

---

## Suggested Implementation Order

1. **`parallel each via` (Rayon)** — Lowest risk. No state machine, reuses existing callback ABI. Validates the `parallel` keyword and AST plumbing.
2. **Phase 1: Synchronous async shim** — Validates `Ty::Future` and type checking without runtime complexity.
3. **Phase 2: `spawn` + `join` blocking (Tokio)** — Real concurrency. Moderate effort, adds runtime executor.
4. **Phase 3: Cooperative async (Tokio)** — State machine codegen. Hardest phase, requires MIR transformation pass.
5. **Phase 4: I/O primitives** — Depends on stdlib design (`spark.net`, `spark.http`).

---

## Open Questions

- **`parallel each` on Map/Set**: Should this be supported? Maps and sets have no defined order, so parallel iteration is semantically valid but the callback sees elements in arbitrary order. Recommendation: support it — the user opted into `parallel` explicitly.
- **Rayon thread pool configuration**: Expose via `spark.sync.set_thread_count(n)` in the stdlib, or use Rayon's default (number of cores)?
- **Cancellation**: Should `Future[T]` support cancellation? Phase 2 doesn't need it. Phase 3 could add `cancel(future)` that drops the state machine.
- **Structured concurrency**: Should `spawn` require an explicit scope (like Swift's `TaskGroup`)? Defer to Phase 4 — unscoped `spawn` is simpler and sufficient for initial release.

---

## References

- Existing `each`/`each via` implementation: `docs/design-notes/lambdas-closures-each.md`
- Lambda/closure ABI (fat pointer): see Memory section of MEMORY.md
- Standard library design: `docs/design-notes/standard-library.md`
- Language spec async/spawn sections: `docs/lang-spec.md`
