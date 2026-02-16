# SPARK-025 — Pipeline Ownership Model Redesign

## Status: Open

## Problem

Three related issues share a root cause — the pipeline's ownership model where each phase
consumes its input via `move`:

| Ticket    | Symptom                                                       |
| --------- | ------------------------------------------------------------- |
| SPARK-004 | Double lex/parse/resolve (2× front-end time)                  |
| SPARK-005 | Heavyweight `.clone()` of TypeTable 3×, ModuleGraph 2×, ASTs  |
| SPARK-006 | Span-keyed expression types collide on synthetic/shared spans |

## Root Cause

In `pipeline.rs`, each phase takes ownership of the previous phase's output. The type checker
consumes the ASTs (via `move`), so the pipeline must re-lex/re-parse/re-resolve all source files
before HIR lowering. Large structures like `TypeTable` (13 HashMaps + DefTable) are cloned
multiple times to satisfy ownership requirements of downstream phases.

## Research Questions

1. **Borrow vs Rc vs CompilationContext**: What's the minimal change to stop re-parsing?
   - Can the type checker borrow ASTs (`&ResolvedAst`) instead of consuming them?
   - Would `Rc<ResolvedAst>` be sufficient, or do downstream phases need mutation?
   - Is a single `CompilationContext` struct (passed by `&mut`) the right long-term model?

2. **TypeTable sharing**: Can downstream phases (HIR, MIR, codegen) share a single `TypeTable`
   instance, or do they legitimately need independent copies (e.g., monomorphization adds entries)?

3. **ExprId introduction**: If we're already refactoring TypeTable, should we replace `Span`-keyed
   expression types with a dedicated `ExprId` at the same time?

4. **Test helper impact**: The double-parse workaround is replicated in every HIR/MIR test helper.
   How many test files need updating?

5. **Salsa/query feasibility**: Is a query-based architecture (like salsa) worth the migration cost
   at this stage, or is a simpler ownership fix sufficient for now?

## Deliverables

- [ ] Trade-off analysis of the three approaches (borrow, Rc, CompilationContext)
- [ ] Prototype of the chosen approach on a branch
- [ ] Migration plan with ordering of changes
- [ ] Estimated test breakage and remediation effort
