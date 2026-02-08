# Stream G: Concurrency

> Async/await/spawn — blocked on Stream C (lambdas)

## Overview

The largest remaining feature in the project. Requires stable closures for `spawn`. Should start only after lambda support (Stream C) is stable.

**Conflicts with Stream C**: Directly depends on closures for spawn.

---

## Item: Async/Await and Spawn (P5.2)

**Status**: Warning in MIR lowering.

### Current State

- `await` at `spark_mir/src/lower_expr.rs:299` — emits warning, returns unit
- `spawn` at `spark_mir/src/lower_expr.rs:304` — emits warning, returns unit
- Both have AST and HIR nodes but no meaningful lowering

### What's Needed

This is a very high-effort feature requiring substantial infrastructure:

#### 1. Future Type System

Add `Ty::Future { inner: TypeId }` to represent the result of an async function. An async function `async fn foo(): Int` returns `Future[Int]`.

- `crates/spark_types/src/ty.rs` — add `Future` variant
- Type checking: async function return types are wrapped in `Future`
- `await` unwraps `Future[T]` to `T`

#### 2. State Machine Transformation

Each `await` point in an async function becomes a state transition. The async function body is transformed into a state machine:

```
// Source
async fn fetch(): String {
    data: String = await http_get(url)
    result: String = await parse(data)
    return result
}

// Transformed to (conceptual)
enum FetchState {
    Start,
    AfterHttpGet { /* locals alive across await */ },
    AfterParse { /* locals alive across await */ },
    Done,
}
```

This is similar to Rust's async transformation. Each state captures the locals that are live across the await point.

#### 3. Async Runtime / Executor

A task executor that polls futures to completion. Minimal viable version:
- Single-threaded event loop
- Task queue with `Future` objects
- `poll()` method on futures that returns `Ready(T)` or `Pending`
- `spawn` queues a new task

This goes in `spark_runtime` as `extern "C"` functions.

#### 4. Task Scheduler for Spawn

`spawn expr` creates a new concurrent task:
- Evaluate `expr` (which must be a closure or async function call)
- Queue it on the executor
- Return a `Future[T]` handle

#### 5. Codegen

- Async functions compile to state machine structs + poll functions
- `await` compiles to state save + return Pending / resume
- `spawn` compiles to task queue insertion + handle construction

### Suggested Approach

Given the complexity, consider phased implementation:

1. **Phase 1**: Async functions that don't actually yield — compile `async fn` as regular functions, `await` as synchronous calls. This validates the type system without the state machine.

2. **Phase 2**: Single-threaded executor with cooperative scheduling. Functions actually yield at await points.

3. **Phase 3**: Multi-threaded executor with work stealing (if desired).

### Key Files

| Purpose | Path |
|---------|------|
| Await MIR stub | `crates/spark_mir/src/lower_expr.rs:299` |
| Spawn MIR stub | `crates/spark_mir/src/lower_expr.rs:304` |
| Type definitions | `crates/spark_types/src/ty.rs` |
| Runtime library | `crates/spark_runtime/src/lib.rs` |
| Codegen entry | `crates/spark_codegen/src/lib.rs` |
| MIR builder | `crates/spark_mir/src/builder.rs` |

## Reference Docs

- Language spec: `docs/lang-spec.md` (async/await section)
- Roadmap: `docs/roadmap.md` (item 5.2)
- Design note: `docs/design-notes/lambdas-closures-each.md` (lambda dependency)
