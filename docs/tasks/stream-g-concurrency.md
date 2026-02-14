# Stream G: Concurrency

> **This document is superseded by [`docs/design-notes/concurrency-model.md`](../design-notes/concurrency-model.md).**

The original design assumed closures were not yet available. Now that Stream C (lambdas, closures, `each`/`each via`) is complete, the concurrency design has been updated with a dual-runtime architecture (Tokio for async I/O, Rayon for CPU parallelism) and phased implementation plan.

See the new design document for:
- `parallel each` / `parallel each via` (Rayon path)
- `async`/`await`/`spawn` phased implementation (Tokio path)
- Tokio + Rayon interaction
- Key files to modify and implementation order
