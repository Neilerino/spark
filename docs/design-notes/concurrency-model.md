# Design Note: Concurrency Model

This document describes Spark's concurrency architecture.

---

## Dual Runtime Architecture

Spark uses two complementary concurrency backends, both hidden behind the FFI wall in `spark_runtime`:

| Dimension | I/O Concurrency (Tokio) | CPU Parallelism (Rayon) |
|-----------|------------------------|------------------------|
| **Use case** | Network, file I/O, timers, 1M+ tasks | Data transforms, math, batch processing |
| **Spark syntax** | `async fn`, `await`, `spawn` | `parallel each`, `parallel each via` |
| **Mechanism** | Cooperative async, epoll/kqueue | Work-stealing thread pool |
| **Runtime FFI** | `__spark_spawn`, `__spark_await`, `__spark_spawn_coop`, `__spark_try_poll` | `__spark_parallel_map`, `__spark_parallel_foreach` |

The language user never picks a backend — the syntax determines which one is used. The two can coexist in the same program.

---

## Part 1: CPU Parallelism — `parallel each` / `parallel each via`

**Status**: Not yet implemented (SPARK-020 in `tasks/todo.md`)

### Syntax

`parallel each` and `parallel each via` are syntactically identical to their sequential counterparts but dispatch to a Rayon-backed runtime function:

```spark
// Sequential (existing, working)
doubled: List[Int] = nums each via (n: Int): Int { n * 2 }

// Parallel (not yet implemented)
doubled: List[Int] = nums parallel each via (n: Int): Int { n * 2 }

// Side-effect iteration
nums parallel each (n: Int): Void { process(n) }
```

### Design

No state machine transformation needed. The callback is the same fat pointer `(fn_ptr, env_ptr)` already used by regular `each via`. Rayon's `par_iter` splits the work across cores internally. From the compiler's perspective, the only difference is which runtime stub gets called.

Callbacks passed to `parallel each` must not capture mutable state. Since Spark captures by value (COW semantics), this is naturally enforced — each thread gets its own copy. No language-level changes needed.

---

## Part 2: I/O Concurrency — `async`/`await`/`spawn`

### Implementation Status

| Phase | Description | Status |
|-------|-------------|--------|
| Phase 1 | Synchronous shim — validates type system (`Ty::Future`) | **Complete** |
| Phase 2 | Spawn + join blocking via Tokio `spawn_blocking` | **Complete** |
| Phase 3 v1 | Cooperative async infrastructure — state machine transform, poll functions | **Complete** |
| Phase 3 v2 | Segmented suspension at await points | Open (SPARK-021) |
| Phase 4 | I/O primitives (`spark.net`, `spark.http`) | **Complete** |

### Phase 1 — Synchronous Shim (Complete)

Validates the type system without state machine complexity.

- `async fn foo(): Int` compiles as a regular function returning `Future[Int]`
- `await expr` unwraps `Future[T]` to `T`
- `spawn expr` evaluates `expr` immediately, wraps result in `Future[T]`

Type system: `Ty::Future { inner: TypeId }` in `spark_types/src/ty.rs`. `async fn foo(): T` has return type `Future[T]`. `await` requires `Future[T]`, produces `T`. `spawn` requires `Future[T]` (from calling an async fn), produces `Future[T]`.

### Phase 2 — Spawn + Join Blocking (Complete)

Real concurrency via Tokio's thread pool.

- `spawn { body }` lifts the body into a zero-arg closure, spawns it on Tokio's blocking thread pool via `__spark_spawn`, returns `SparkFuture*`
- `await future` joins the handle via `__spark_await` and returns the inner `T`
- Direct `await async_fn()` (no spawn) stays synchronous for backward compatibility

**Thread safety**: Spawn blocks cannot capture `Ref[T]` or mutable bindings.

**`SparkFuture`** is a heap-allocated RC'd struct:

```rust
#[repr(C)]
pub struct SparkFuture {
    rc: usize,
    handle: Option<tokio::task::JoinHandle<*mut u8>>,
}
```

**Runtime stubs**: `__spark_spawn`, `__spark_await`, `__spark_future_rc_dec`.

**Tests**: 7 E2E tests covering basic spawn, parallel execution, closure capture, and error cases.

### Phase 3 v1 — Cooperative Async Infrastructure (Complete)

MIR-level async state machine transform (`spark_mir/src/async_transform.rs`).

For each async function with await points:
1. Computes live-across-await sets via backward dataflow liveness
2. Builds `Ty::AsyncState` struct type
3. Generates a poll function (`fn(state_ptr) -> i8`, returns 0=Pending / 1=Ready)

**v1 simplification**: Poll function calls the original function synchronously on first poll. Full infrastructure is in place — the poll/state machinery works — but true suspension at await points requires v2.

**Runtime stubs**: `__spark_alloc_state`, `__spark_spawn_coop`, `__spark_try_poll`. `SparkTask` Rust Future adapter with thread-local waker bridge. Pipeline integration between borrow checking and optimization.

Original function preserved for sync callers.

**Tests**: 3 E2E tests (`coop_basic`, `coop_chain`, `coop_parallel`).

### Phase 3 v2 — Segmented Suspension (Open — SPARK-021)

**Status**: Not yet implemented. Tracked as SPARK-021 in `tasks/todo.md`.

Complete the cooperative async state machine: segmented poll functions that suspend/resume at each await point. v1 calls the original function synchronously; v2 requires splitting function bodies at await points into state machine states, saving/restoring live variables.

Each `await` point becomes a state transition. Locals live across await points are captured in the state struct (similar to closure capture analysis).

```
// Conceptual output for:
//   async fn fetch(): String {
//       data = await http_get(url)
//       result = await parse(data)
//       return result
//   }

struct FetchState {
    state_tag: u8,
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

### Phase 4 — I/O Primitives (Complete)

Async I/O is exposed through the standard library modules `spark.net` and `spark.http`:

- **`spark.net`**: TCP (listen/accept/connect/read/write/shutdown), UDP (bind/send/recv), DNS resolve. Backed by `tokio::net` via `block_on`. Handle-table architecture with `Arc`-wrapped resources.
- **`spark.http`**: Client (GET/POST/PUT/DELETE via `reqwest`), Server (HTTP/1.1 via `tokio::net` TCP with manual parsing). Handle-table for servers and pending requests.

```spark
import spark.http { get }

async fn fetch_data(url: String): String | HttpError {
    response: String | HttpError = await get(url)
    return response
}
```

---

## Tokio + Rayon Interaction

Calling `parallel each via` (Rayon, blocking) inside an `async fn` would block a Tokio worker thread, degrading throughput.

The runtime's `__spark_parallel_map` detects whether it's running inside a Tokio context and, if so, uses `tokio::task::block_in_place()` to offload Rayon work to a dedicated thread pool. This is a runtime implementation detail — the user's Spark code is identical either way.

---

## Key Implementation Files

| Area | Files | Status |
|------|-------|--------|
| Type system | `spark_types/src/ty.rs` — `Ty::Future { inner: TypeId }` | Complete |
| Type checker | `spark_types/src/check_expr.rs` — async/await/spawn type validation | Complete |
| MIR lowering | `spark_mir/src/lower_expr.rs` — await, spawn lowering | Complete |
| Async transform | `spark_mir/src/async_transform.rs` — state machine pass | Complete (v1) |
| Runtime (Tokio) | `spark_runtime/src/async_rt.rs` — spawn, await, coop poll | Complete |
| Runtime (Rayon) | `spark_runtime/src/parallel.rs` | Not yet wired to compiler |
| Codegen | `spark_codegen/src/cranelift/emit_expr.rs` — Future type handling | Complete |
| Drop | `spark_drop/src/lib.rs` — `needs_rc(Ty::Future)` | Complete |

---

## References

- Existing `each`/`each via` implementation: `docs/design-notes/lambdas-closures-each.md`
- Lambda/closure ABI (fat pointer): see Memory section of MEMORY.md
- Standard library design: `docs/design-notes/standard-library.md`
- Language spec async/spawn sections: `docs/lang-spec.md`
