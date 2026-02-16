# SPARK-026 — Incremental Compilation Feasibility

## Status: Open

## Problem

Every source change triggers a full re-compilation of all phases (lex → parse → resolve → types →
HIR → MIR → borrow → opt → codegen → link). For small projects this is fine, but as Spark programs
grow, compile times will become a bottleneck — especially in the LSP hot-reload loop.

## Research Questions

1. **Granularity**: What is the right unit of incrementality?
   - Per-file (only re-compile changed files)?
   - Per-function (only re-check changed function bodies)?
   - Per-query (salsa-style memoized queries)?

2. **Data structure compatibility**: Which current data structures would need to change?
   - `TypeTable` uses `HashMap` — compatible with per-file invalidation?
   - `ModuleGraph` — how does it handle adding/removing files?
   - `DefId` allocation — are IDs stable across re-compilations?

3. **Salsa integration**: What would adopting salsa (or a similar framework) look like?
   - How many crate APIs would need to change?
   - What's the migration path — big bang or incremental?
   - Performance overhead of the query framework itself?

4. **LSP impact**: The LSP currently re-runs the full pipeline on each change. How much would
   incremental compilation improve responsiveness?

5. **ROI estimate**: Given current project size and growth trajectory, when does incremental
   compilation become necessary vs nice-to-have?

## Deliverables

- [ ] Survey of approaches (per-file, per-function, query-based)
- [ ] Compatibility assessment of current data structures
- [ ] Effort estimate for each approach
- [ ] Recommendation with rationale
