# Stream E: Optimization

> Independent optimization crate + borrow checker ergonomics

## Overview

Completely independent of all other streams. `spark_opt` inserts between `spark_borrow` and `spark_codegen` in the pipeline. Reborrowing is isolated to `spark_borrow`.

**Conflicts**: None. `spark_opt` operates on MIR which is stable.

---

## Item 1: `spark_opt` — MIR Optimization Passes (P4.1)

**Status**: Not started. This is a new crate.

### Pipeline Insertion Point

```
spark_borrow → spark_opt (NEW) → spark_codegen
```

**Borrow checker output** (`spark_borrow/src/lib.rs:67-76`):
```rust
pub struct BorrowOutput {
    pub mir: Mir,           // Passed through unchanged
    pub table: TypeTable,   // Passed through unchanged
    pub diagnostics: Diagnostics,
}
```

**Codegen input** (`spark_codegen/src/lib.rs:73-84`):
```rust
pub struct CodegenInput {
    pub mir: Mir,
    pub table: TypeTable,
    pub definitions: DefinitionTable,
    pub target: TargetSpec,
}
```

`spark_opt` takes `Mir` + `TypeTable` in, returns optimized `Mir` + `TypeTable` out. Same pattern as other pipeline crates.

### Crate Setup

1. Create `crates/spark_opt/src/lib.rs`
2. Add to workspace in root `Cargo.toml`
3. Define `OptInput` / `OptOutput` following the pipeline API pattern
4. Wire into `spark_driver/src/main.rs` between borrow check and codegen

### Planned Passes

#### Pass 1: Constant Folding
Evaluate constant expressions at compile time:
- `3 + 4` → `7`
- `true && false` → `false`
- Walk MIR statements, replace `Rvalue::BinaryOp(op, Operand::Constant(a), Operand::Constant(b))` with `Rvalue::Use(Operand::Constant(result))`

#### Pass 2: Dead Code Elimination
Remove unreachable blocks and unused assignments:
- Find blocks with no predecessors (except entry block) — remove them
- Find `Local`s that are assigned but never read — remove the assignment statement
- Iterate until fixpoint

#### Pass 3: Copy Propagation
Eliminate redundant copies:
- If `_tmp = _x` and `_tmp` is only used once, replace the use of `_tmp` with `_x`
- Track use counts per Local

#### Pass 4: SSA Construction (Future)
Convert pre-SSA MIR to SSA form with phi nodes. This enables more advanced optimizations but is a significant undertaking. Consider deferring.

#### Pass 5: Decision-Tree Match Compilation (Future)
Optimize sequential pattern tests into switch trees. Current match compilation generates linear test-and-branch chains; a decision tree reduces redundant tests.

### MIR Structure Reference

- `Local(u32)` — variable/temp identifier (`spark_mir/src/mir.rs:44`)
- `LocalDecl` — type + metadata for each local (`spark_mir/src/mir.rs:91`)
- `BasicBlock` — list of statements + terminator (`spark_mir/src/mir.rs:110`)
- `Statement` — assignments, drops, etc.
- `Terminator` — return, goto, call, switch, unreachable
- `BlockId(0)` = entry block
- `Local(0)` = return place, `1..=N` = params

### Key Files
- `crates/spark_mir/src/mir.rs` — all MIR types
- `crates/spark_mir/src/lib.rs:80-111` — MIR lowering entry point
- `crates/spark_borrow/src/lib.rs:67-76` — borrow output (input to opt)
- `crates/spark_codegen/src/lib.rs:73-84` — codegen input (output of opt)
- `crates/spark_driver/src/main.rs` — pipeline wiring

---

## Item 2: Reborrowing & Two-Phase Borrows (P4.2)

**Status**: Borrow checker rejects these cases. Low effort, ergonomics improvement.

### What's Needed

- **Reborrowing**: When you have `&mut x`, allow `&*x` to create a shorter-lived `&x`. The borrow checker should automatically shorten the region.

- **Two-phase borrows**: Allow `x.method(&x)` by splitting the mutable borrow into an "issue" phase (reservation) and "use" phase (activation). The shared borrow of `x` is allowed during the reservation phase.

### Key Files
- `crates/spark_borrow/src/lib.rs` — main borrow checker
- `crates/spark_borrow/src/` — fact extraction, liveness, region inference, conflict detection

---

## Reference Docs

- Compiler architecture: `docs/compiler-architecture.md` (pipeline design)
- Roadmap: `docs/roadmap.md` (items 4.1, 4.2)
- MIR README: `crates/spark_mir/README.md`
