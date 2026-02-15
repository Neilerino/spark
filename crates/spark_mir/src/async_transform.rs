//! Async state machine transform for cooperative scheduling.
//!
//! Transforms async functions that contain `await` points into poll-based
//! state machines. Each async function is split into segments at await
//! boundaries. A new **poll function** is generated that dispatches to the
//! correct segment based on a state tag, saves/restores live locals across
//! suspend points, and returns `Pending` (0) or `Ready` (1).
//!
//! The original async function body is kept unchanged for synchronous
//! callers (Phase 2 compatibility). The poll function is used by the
//! cooperative spawn path (`__spark_spawn_coop`).
//!
//! ## Design
//!
//! Follows the `monomorphize.rs` 4-phase pattern:
//! 1. **Collect** — identify async functions with await points
//! 2. **Analyze** — compute live-across-await sets via backward liveness
//! 3. **Generate** — create poll functions and state struct types
//! 4. **Record** — store `original → poll` mapping for codegen

use rustc_hash::{FxHashMap, FxHashSet};
use spark_diagnostics::Diagnostics;
use spark_intern::{kw, sym, Symbol};
use spark_resolve::{DefId, DefinitionInfo, DefinitionKind, DefinitionTable, Visibility};
use spark_types::{IntWidth, Ty, TypeId, TypeTable};

use crate::mir::{
    BasicBlock, BlockId, Constant, Local, LocalDecl, Mir, MirFunction, Operand, Place, Projection,
    Rvalue, Statement, Terminator,
};

// ── Public API ──────────────────────────────────────────────────────────

/// Input to the async transform pass.
#[derive(Debug)]
pub struct AsyncTransformInput {
    /// The MIR from the previous phase.
    pub mir: Mir,
    /// The type table.
    pub table: TypeTable,
    /// Definition table.
    pub definitions: DefinitionTable,
}

/// Output from the async transform pass.
#[derive(Debug)]
pub struct AsyncTransformOutput {
    /// The transformed MIR (with poll functions added).
    pub mir: Mir,
    /// The type table (may have new types for state structs).
    pub table: TypeTable,
    /// Definition table (may have new entries for poll functions).
    pub definitions: DefinitionTable,
    /// Mapping from original async function DefId → poll function DefId.
    pub poll_fn_map: FxHashMap<DefId, PollFnInfo>,
    /// Diagnostics accumulated during the transform.
    pub diagnostics: Diagnostics,
}

/// Information about a generated poll function.
#[derive(Debug, Clone)]
pub struct PollFnInfo {
    /// The poll function's DefId.
    pub poll_def_id: DefId,
    /// The state struct type (Ty::AsyncState).
    pub state_type_id: TypeId,
    /// Field names and types in the state struct, in order.
    pub state_fields: Vec<(Symbol, TypeId)>,
}

/// Runs the async state machine transform.
///
/// For each async function that contains await points, generates a poll
/// function (state machine) and records the mapping. The original function
/// body is preserved for sync callers.
#[must_use]
pub fn transform(input: AsyncTransformInput) -> AsyncTransformOutput {
    let AsyncTransformInput {
        mut mir,
        mut table,
        mut definitions,
    } = input;

    let diagnostics = Diagnostics::new();
    let mut poll_fn_map = FxHashMap::default();

    // Phase 1: Collect async functions with await points.
    let async_def_ids: Vec<DefId> = mir
        .functions
        .iter()
        .filter(|(_, f)| f.is_async)
        .map(|(def_id, _)| *def_id)
        .collect();

    for def_id in &async_def_ids {
        let func = match mir.functions.get(def_id) {
            Some(f) => f,
            None => continue,
        };

        // Find await points (calls to __spark_await).
        let await_points = find_await_points(func, &definitions);
        if await_points.is_empty() {
            // No awaits — no state machine needed.
            continue;
        }

        // Phase 2: Compute live-across-await sets.
        let live_across = compute_live_across_awaits(func, &await_points);

        // Phase 3: Build the state struct type.
        let all_live: FxHashSet<Local> = live_across.iter().flat_map(|s| s.iter().copied()).collect();
        let state_fields = build_state_fields(func, &all_live, &mut table);
        let state_type = Ty::AsyncState {
            func_def_id: *def_id,
            fields: state_fields.clone(),
        };
        let state_type_id = table.intern(state_type);

        // Allocate DefId for the poll function.
        let original_name = definitions
            .get(*def_id)
            .map_or_else(|| sym("__poll"), |info| info.name);
        let poll_name = sym(&format!("__poll_{}", original_name.as_str()));
        let poll_def_id = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            poll_name,
            func.span,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));

        // Phase 3b: Generate the poll function MIR.
        let poll_fn = generate_poll_function(
            func,
            poll_def_id,
            state_type_id,
            &state_fields,
            &await_points,
            &live_across,
            &mut table,
        );

        poll_fn_map.insert(
            *def_id,
            PollFnInfo {
                poll_def_id,
                state_type_id,
                state_fields: state_fields.clone(),
            },
        );

        mir.functions.insert(poll_def_id, poll_fn);
    }

    AsyncTransformOutput {
        mir,
        table,
        definitions,
        poll_fn_map,
        diagnostics,
    }
}

// ── Await Point Detection ───────────────────────────────────────────────

/// An await point in an async function.
#[derive(Debug, Clone)]
struct AwaitPoint {
    /// The block containing the await call.
    call_block_idx: usize,
    /// The block to resume at after the await (the call's continuation).
    /// Used in segmented poll (Phase 3 v2).
    #[allow(dead_code)]
    resume_block: BlockId,
    /// The local that receives the await result.
    dest_local: Local,
    /// The local holding the future being awaited (argument to `__spark_await`).
    /// Used in segmented poll (Phase 3 v2).
    #[allow(dead_code)]
    future_local: Local,
}

/// Finds all await points in a function by scanning for calls to `__spark_await`.
fn find_await_points(func: &MirFunction, definitions: &DefinitionTable) -> Vec<AwaitPoint> {
    let mut points = Vec::new();

    for (block_idx, block) in func.blocks.iter().enumerate() {
        if let Terminator::Call {
            func: callee_op,
            args,
            destination,
            target,
            ..
        } = &block.terminator
        {
            // Check if the callee is __spark_await by tracing through FunctionRef.
            let callee_def_id = extract_callee_def_id(callee_op, func);
            if let Some(def_id) = callee_def_id {
                if let Some(info) = definitions.get(def_id) {
                    if info.name == kw::SPARK_AWAIT {
                        // Extract the future local from the first argument.
                        let future_local = match args.first() {
                            Some(Operand::Copy(place) | Operand::Move(place))
                                if place.projections.is_empty() =>
                            {
                                place.local
                            }
                            _ => continue,
                        };

                        points.push(AwaitPoint {
                            call_block_idx: block_idx,
                            resume_block: *target,
                            dest_local: destination.local,
                            future_local,
                        });
                    }
                }
            }
        }
    }

    points
}

/// Extracts the DefId from a callee operand by tracing FunctionRef assignments.
fn extract_callee_def_id(callee: &Operand, func: &MirFunction) -> Option<DefId> {
    match callee {
        Operand::Constant(Constant::FunctionRef(def_id)) => Some(*def_id),
        Operand::Copy(place) | Operand::Move(place) if place.projections.is_empty() => {
            // Search backwards for the FunctionRef assignment to this local.
            let local = place.local;
            for block in &func.blocks {
                for stmt in &block.statements {
                    if let Statement::Assign {
                        place: assign_place,
                        rvalue: Rvalue::Use(Operand::Constant(Constant::FunctionRef(def_id))),
                        ..
                    } = stmt
                    {
                        if assign_place.local == local && assign_place.projections.is_empty() {
                            return Some(*def_id);
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

// ── Liveness Analysis ───────────────────────────────────────────────────

/// Computes the set of locals that are live across each await point.
///
/// A local is "live across" an await if it is used after the await resumes
/// and was defined (or last assigned) before the await.
///
/// Uses a simplified backward dataflow analysis.
fn compute_live_across_awaits(
    func: &MirFunction,
    await_points: &[AwaitPoint],
) -> Vec<FxHashSet<Local>> {
    let num_blocks = func.blocks.len();

    // Compute gen/kill sets for each block.
    let mut gen: Vec<FxHashSet<Local>> = vec![FxHashSet::default(); num_blocks];
    let mut kill: Vec<FxHashSet<Local>> = vec![FxHashSet::default(); num_blocks];

    for (idx, block) in func.blocks.iter().enumerate() {
        // Process terminator (uses first, then kills from defs).
        add_terminator_uses(&block.terminator, &mut gen[idx]);

        // Process statements in reverse order (backward analysis within block).
        for stmt in block.statements.iter().rev() {
            match stmt {
                Statement::Assign { place, rvalue, .. } => {
                    // The LHS is killed (defined).
                    if place.projections.is_empty() {
                        kill[idx].insert(place.local);
                        gen[idx].remove(&place.local);
                    }
                    // The RHS generates uses.
                    add_rvalue_uses(rvalue, &mut gen[idx]);
                    // Projections on LHS generate uses too (e.g., _x.field = ...).
                    for proj in &place.projections {
                        if let Projection::Index(idx_local) = proj {
                            gen[idx].insert(*idx_local);
                        }
                    }
                    if !place.projections.is_empty() {
                        gen[idx].insert(place.local);
                    }
                }
                Statement::Drop { place, .. } => {
                    gen[idx].insert(place.local);
                }
                Statement::Nop => {}
            }
        }
    }

    // Build successor map.
    let successors = build_successor_map(func);

    // Backward dataflow: live_in[b] = gen[b] ∪ (live_out[b] − kill[b])
    //                     live_out[b] = ∪ { live_in[s] : s ∈ successors(b) }
    let mut live_in: Vec<FxHashSet<Local>> = vec![FxHashSet::default(); num_blocks];
    let mut live_out: Vec<FxHashSet<Local>> = vec![FxHashSet::default(); num_blocks];

    // Worklist fixpoint.
    let mut worklist: Vec<usize> = (0..num_blocks).collect();
    let mut changed = true;
    while changed {
        changed = false;
        // Process in reverse order (backward).
        worklist.reverse();
        for &idx in &worklist {
            // Compute live_out from successors' live_in.
            let mut new_out = FxHashSet::default();
            for &succ in &successors[idx] {
                for local in &live_in[succ] {
                    new_out.insert(*local);
                }
            }

            // Compute live_in = gen ∪ (live_out − kill).
            let mut new_in = gen[idx].clone();
            for local in &new_out {
                if !kill[idx].contains(local) {
                    new_in.insert(*local);
                }
            }

            if new_in != live_in[idx] || new_out != live_out[idx] {
                live_in[idx] = new_in;
                live_out[idx] = new_out;
                changed = true;
            }
        }
    }

    // For each await point, the live-across set = live_out of the call block,
    // excluding the destination local (it's produced by the await, not live before).
    // Also exclude Local(0) (return place) and the callee function temp.
    await_points
        .iter()
        .map(|ap| {
            let mut live = live_out[ap.call_block_idx].clone();
            live.remove(&ap.dest_local);
            live.remove(&Local::RETURN_PLACE);
            // Exclude zero-param locals (callee temps for __spark_await).
            live
        })
        .collect()
}

/// Adds uses from a terminator to the gen set.
fn add_terminator_uses(term: &Terminator, gen: &mut FxHashSet<Local>) {
    match term {
        Terminator::Goto { .. } | Terminator::Return | Terminator::Unreachable => {}
        Terminator::SwitchInt { discriminant, .. } => {
            add_operand_uses(discriminant, gen);
        }
        Terminator::Call {
            func: callee, args, ..
        } => {
            add_operand_uses(callee, gen);
            for arg in args {
                add_operand_uses(arg, gen);
            }
        }
    }
}

/// Adds uses from an rvalue to the gen set.
fn add_rvalue_uses(rvalue: &Rvalue, gen: &mut FxHashSet<Local>) {
    match rvalue {
        Rvalue::Use(op) | Rvalue::Cast { operand: op, .. } => {
            add_operand_uses(op, gen);
        }
        Rvalue::BinaryOp(_, a, b) => {
            add_operand_uses(a, gen);
            add_operand_uses(b, gen);
        }
        Rvalue::UnaryOp(_, op) => {
            add_operand_uses(op, gen);
        }
        Rvalue::Ref { place, .. } => {
            gen.insert(place.local);
        }
        Rvalue::Aggregate(_, ops) | Rvalue::StringInterp(ops) => {
            for op in ops {
                add_operand_uses(op, gen);
            }
        }
        Rvalue::ListRange { start, end, step, .. } => {
            add_operand_uses(start, gen);
            add_operand_uses(end, gen);
            add_operand_uses(step, gen);
        }
        Rvalue::Discriminant(place) => {
            gen.insert(place.local);
        }
    }
}

/// Adds uses from an operand to the gen set.
fn add_operand_uses(op: &Operand, gen: &mut FxHashSet<Local>) {
    match op {
        Operand::Copy(place) | Operand::Move(place) => {
            gen.insert(place.local);
            for proj in &place.projections {
                if let Projection::Index(idx_local) = proj {
                    gen.insert(*idx_local);
                }
            }
        }
        Operand::Constant(_) => {}
    }
}

/// Builds a map from block index → successor block indices.
fn build_successor_map(func: &MirFunction) -> Vec<Vec<usize>> {
    func.blocks
        .iter()
        .map(|block| match &block.terminator {
            Terminator::Goto { target } => vec![target.index()],
            Terminator::Return | Terminator::Unreachable => vec![],
            Terminator::SwitchInt {
                targets, otherwise, ..
            } => {
                let mut succs: Vec<usize> = targets.iter().map(|(_, b)| b.index()).collect();
                succs.push(otherwise.index());
                succs
            }
            Terminator::Call { target, .. } => vec![target.index()],
        })
        .collect()
}

// ── State Struct Construction ───────────────────────────────────────────

/// Builds the fields for the state struct type.
///
/// Fields:
/// - `__tag: Int32` — state dispatch tag
/// - `__sub_future: ptr` — currently-awaited sub-future pointer
/// - `__local_N: TypeN` — one per live-across-await local
/// - `__result: ReturnType` — where the final result is stored
fn build_state_fields(
    func: &MirFunction,
    all_live: &FxHashSet<Local>,
    table: &mut TypeTable,
) -> Vec<(Symbol, TypeId)> {
    let mut fields = Vec::new();

    // Tag field (i32).
    let i32_ty = table.intern(Ty::Int(IntWidth::I32));
    fields.push((sym("__tag"), i32_ty));

    // Sub-future pointer (i64 — opaque pointer).
    let i64_ty = table.intern(Ty::Int(IntWidth::I64));
    fields.push((sym("__sub_future"), i64_ty));

    // One field per live-across-await local, sorted by index for determinism.
    let mut live_locals: Vec<Local> = all_live.iter().copied().collect();
    live_locals.sort_by_key(|l| l.0);

    for local in &live_locals {
        let local_decl = &func.locals[local.index()];
        let field_name = sym(&format!("__local_{}", local.0));
        fields.push((field_name, local_decl.ty));
    }

    // Result field.
    fields.push((sym("__result"), func.return_type));

    fields
}

// ── Poll Function Generation ────────────────────────────────────────────

/// Generates the poll function MIR.
///
/// The poll function has signature `fn(state_ptr: ptr) -> i8` where:
/// - Returns 0 = Pending (awaited sub-future not ready)
/// - Returns 1 = Ready (function completed, result in state.__result)
///
/// The function body is a dispatch table: load `state.__tag`, switch to the
/// appropriate segment, execute code, and either return Ready or save state
/// and return Pending.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn generate_poll_function(
    original: &MirFunction,
    poll_def_id: DefId,
    state_type_id: TypeId,
    state_fields: &[(Symbol, TypeId)],
    _await_points: &[AwaitPoint],
    _live_across: &[FxHashSet<Local>],
    table: &mut TypeTable,
) -> MirFunction {
    let i8_ty = table.intern(Ty::Int(IntWidth::I8));
    let i32_ty = table.intern(Ty::Int(IntWidth::I32));
    let i64_ty = table.intern(Ty::Int(IntWidth::I64));

    // Allocate locals for the poll function.
    let mut locals = Vec::new();
    let mut next_local_id = 0u32;

    let alloc_local = |locals: &mut Vec<LocalDecl>, next: &mut u32, ty, name, span| {
        let local = Local(*next);
        locals.push(LocalDecl {
            local,
            ty,
            name,
            is_user_variable: false,
            mutable: true,
            span,
        });
        *next += 1;
        local
    };

    // Local(0) = return place (i8: 0 = Pending, 1 = Ready).
    let _return_place = alloc_local(&mut locals, &mut next_local_id, i8_ty, None, original.span);

    // Local(1) = state_ptr parameter (pointer — use i64 as opaque ptr).
    let state_ptr_local =
        alloc_local(&mut locals, &mut next_local_id, i64_ty, Some(sym("__state_ptr")), original.span);

    // Local for loaded tag.
    let tag_local = alloc_local(&mut locals, &mut next_local_id, i32_ty, Some(sym("__tag")), original.span);

    // Locals for each original function local that might need to be restored.
    // Map: original Local → poll function Local.
    let mut orig_to_poll: FxHashMap<Local, Local> = FxHashMap::default();

    // We need locals for ALL original locals (not just live-across-await ones)
    // because the segments execute the original code. But we only save/restore
    // the live-across-await ones.
    for local_decl in &original.locals {
        let poll_local = alloc_local(
            &mut locals,
            &mut next_local_id,
            local_decl.ty,
            local_decl.name,
            local_decl.span,
        );
        orig_to_poll.insert(local_decl.local, poll_local);
    }

    // Locals for try_poll result and sub-future.
    // Reserved for future segmented polling (Phase 3 v2).
    let _try_poll_result_local =
        alloc_local(&mut locals, &mut next_local_id, i64_ty, Some(sym("__poll_result")), original.span);

    // Build field name → field index map for state struct access.
    let field_index: FxHashMap<Symbol, usize> = state_fields
        .iter()
        .enumerate()
        .map(|(i, (name, _))| (*name, i))
        .collect();

    // Collect all live-across-await locals (union across all await points).
    // Re-type state_ptr to state_type_id so Field projections resolve correctly.
    // The state_ptr parameter is a pointer to the state struct; codegen will
    // treat it as a StackSlot/aggregate pointer and use Field projections to
    // access fields at their computed offsets.
    locals[state_ptr_local.index()].ty = state_type_id;

    // ── Build blocks ────────────────────────────────────────────────────
    //
    // Phase 3 v1: the poll function calls the original function synchronously
    // on first poll and immediately returns Ready. This establishes the full
    // cooperative infrastructure (state struct, poll function, spawn_coop)
    // without the complexity of segmenting the original MIR at await points.
    // Segmented suspension will be added in a follow-up.
    //
    // Structure:
    //   bb0 (entry): load tag, switchInt → [0: bb_run, else: bb_already_ready]
    //   bb1 (bb_run): load params from state, call original function → bb_store
    //   bb2 (bb_already_ready): return 1
    //   bb3 (bb_store): store result in state.__result, set tag = 1, return 1

    let mut blocks: Vec<BasicBlock> = Vec::new();

    // Block 0: entry — dispatch on tag.
    let bb_entry = BasicBlock {
        statements: vec![Statement::Assign {
            place: Place::from_local(tag_local),
            rvalue: Rvalue::Use(Operand::Copy(Place {
                local: state_ptr_local,
                projections: vec![Projection::Field(sym("__tag"))],
            })),
            span: original.span,
        }],
        terminator: Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::from_local(tag_local)),
            targets: vec![(0, BlockId(1))],
            otherwise: BlockId(2),
        },
        is_cleanup: false,
    };
    blocks.push(bb_entry);

    // Block 1 (bb_run): Load params from state, call original function, store result.
    let mut run_stmts = Vec::new();

    // Load each parameter from the state struct.
    // Parameters are locals 1..=param_count in the original function.
    // In the state struct, they're stored as __local_N fields.
    for param_idx in 1..=original.param_count {
        let orig_local = Local(param_idx);
        let field_name = sym(&format!("__local_{param_idx}"));

        if field_index.contains_key(&field_name) {
            if let Some(&poll_local) = orig_to_poll.get(&orig_local) {
                run_stmts.push(Statement::Assign {
                    place: Place::from_local(poll_local),
                    rvalue: Rvalue::Use(Operand::Copy(Place {
                        local: state_ptr_local,
                        projections: vec![Projection::Field(field_name)],
                    })),
                    span: original.span,
                });
            }
        }
    }

    // Call the original function with the loaded parameters.
    let mut call_args: Vec<Operand> = Vec::new();
    for param_idx in 1..=original.param_count {
        let orig_local = Local(param_idx);
        if let Some(&poll_local) = orig_to_poll.get(&orig_local) {
            call_args.push(Operand::Copy(Place::from_local(poll_local)));
        }
    }

    // Destination for the call result.
    let call_result_local = if let Some(&poll_local) = orig_to_poll.get(&Local::RETURN_PLACE) {
        poll_local
    } else {
        alloc_local(
            &mut locals,
            &mut next_local_id,
            original.return_type,
            None,
            original.span,
        )
    };

    // We need the original function's DefId to call it.
    let orig_fn_local = alloc_local(
        &mut locals,
        &mut next_local_id,
        i64_ty,
        Some(sym("__orig_fn")),
        original.span,
    );
    run_stmts.push(Statement::Assign {
        place: Place::from_local(orig_fn_local),
        rvalue: Rvalue::Use(Operand::Constant(Constant::FunctionRef(original.def_id))),
        span: original.span,
    });

    let bb_run = BasicBlock {
        statements: run_stmts,
        terminator: Terminator::Call {
            func: Operand::Copy(Place::from_local(orig_fn_local)),
            args: call_args,
            destination: Place::from_local(call_result_local),
            target: BlockId(3), // bb_store_result
            span: original.span,
        },
        is_cleanup: false,
    };
    blocks.push(bb_run);

    // Block 2 (bb_already_ready): Return 1 immediately.
    let bb_already_ready = BasicBlock {
        statements: vec![Statement::Assign {
            place: Place::from_local(Local::RETURN_PLACE),
            rvalue: Rvalue::Use(Operand::Constant(Constant::Int(1))),
            span: original.span,
        }],
        terminator: Terminator::Return,
        is_cleanup: false,
    };
    blocks.push(bb_already_ready);

    // Block 3 (bb_store_result): Store result in state.__result, set tag = 1, return 1.
    let mut store_stmts = Vec::new();

    // Store result: state.__result = call_result
    store_stmts.push(Statement::Assign {
        place: Place {
            local: state_ptr_local,
            projections: vec![Projection::Field(sym("__result"))],
        },
        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(call_result_local))),
        span: original.span,
    });

    // Set tag = 1 (completed).
    store_stmts.push(Statement::Assign {
        place: Place {
            local: state_ptr_local,
            projections: vec![Projection::Field(sym("__tag"))],
        },
        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(1))),
        span: original.span,
    });

    // Return 1 (Ready).
    store_stmts.push(Statement::Assign {
        place: Place::from_local(Local::RETURN_PLACE),
        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(1))),
        span: original.span,
    });

    let bb_store_result = BasicBlock {
        statements: store_stmts,
        terminator: Terminator::Return,
        is_cleanup: false,
    };
    blocks.push(bb_store_result);

    MirFunction {
        def_id: poll_def_id,
        param_count: 1, // state_ptr
        return_type: i8_ty,
        locals,
        blocks,
        captures: vec![],
        is_async: false,
        span: original.span,
    }
}

/// Computes which original blocks belong to each segment.
///
/// Segment 0: blocks from entry up to (but not including) the first await call block.
/// Segment k: blocks from `await_points[k-1].resume_block` up to `await_points[k].call_block`.
/// Last segment: blocks from last `resume_block` to the end.
///
/// Reserved for Phase 3 v2 segmented poll functions.
#[allow(dead_code)]
fn compute_segment_ranges(
    func: &MirFunction,
    await_points: &[AwaitPoint],
) -> Vec<Vec<usize>> {
    let num_blocks = func.blocks.len();
    let num_segments = await_points.len() + 1;
    let mut segments = vec![Vec::new(); num_segments];

    if await_points.is_empty() {
        segments[0] = (0..num_blocks).collect();
        return segments;
    }

    // For simplicity in v1, we don't actually use segment ranges since
    // the poll function calls the original function as a whole. This is
    // a placeholder for future segmented execution.
    for i in 0..num_blocks {
        segments[0].push(i);
    }

    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::{BasicBlock, Local, LocalDecl, Place};
    use spark_span::Span;
    use spark_types::{IntWidth, Ty};
    use std::collections::HashMap;

    fn make_simple_async_fn(
        def_id: DefId,
        await_def_id: DefId,
        table: &mut TypeTable,
    ) -> MirFunction {
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));
        let void_ty = table.intern(Ty::Void);
        let future_ty = table.intern(Ty::Future { inner: i64_ty });

        // Simple async function:
        //   _0: Int (return place)
        //   _1: Int (param x)
        //   _2: Future[Int] (temp for spawned future)
        //   _3: fn (temp for __spark_await ref)
        //   _4: Int (await result)
        //
        // bb0: _2 = ... (some future)
        //      _3 = FunctionRef(__spark_await)
        //      call _3(_2) → bb1, dest = _4
        // bb1: _0 = _4
        //      return
        MirFunction {
            def_id,
            param_count: 1,
            return_type: i64_ty,
            locals: vec![
                LocalDecl {
                    local: Local(0),
                    ty: i64_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(1),
                    ty: i64_ty,
                    name: Some(sym("x")),
                    is_user_variable: true,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(2),
                    ty: future_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(3),
                    ty: void_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(4),
                    ty: i64_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
            ],
            blocks: vec![
                BasicBlock {
                    statements: vec![
                        Statement::Assign {
                            place: Place::from_local(Local(2)),
                            rvalue: Rvalue::Use(Operand::Constant(Constant::Int(0))),
                            span: Span::DUMMY,
                        },
                        Statement::Assign {
                            place: Place::from_local(Local(3)),
                            rvalue: Rvalue::Use(Operand::Constant(Constant::FunctionRef(
                                await_def_id,
                            ))),
                            span: Span::DUMMY,
                        },
                    ],
                    terminator: Terminator::Call {
                        func: Operand::Copy(Place::from_local(Local(3))),
                        args: vec![Operand::Copy(Place::from_local(Local(2)))],
                        destination: Place::from_local(Local(4)),
                        target: BlockId(1),
                        span: Span::DUMMY,
                    },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(4)))),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
            captures: vec![],
            is_async: true,
            span: Span::DUMMY,
        }
    }

    #[test]
    fn find_await_points_detects_await() {
        let mut table = TypeTable::new();
        let mut definitions = DefinitionTable::new();

        let await_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::BuiltinFunction,
            kw::SPARK_AWAIT,
            Span::DUMMY,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));

        let func_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            sym("my_async"),
            Span::DUMMY,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));

        let func = make_simple_async_fn(func_def, await_def, &mut table);
        let points = find_await_points(&func, &definitions);

        assert_eq!(points.len(), 1, "Expected exactly one await point");
        assert_eq!(points[0].call_block_idx, 0);
        assert_eq!(points[0].resume_block, BlockId(1));
        assert_eq!(points[0].dest_local, Local(4));
        assert_eq!(points[0].future_local, Local(2));
    }

    #[test]
    fn no_awaits_no_transform() {
        let mut table = TypeTable::new();
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));
        let mut definitions = DefinitionTable::new();

        let func_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            sym("simple_async"),
            Span::DUMMY,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));

        // Async function with no awaits.
        let func = MirFunction {
            def_id: func_def,
            param_count: 0,
            return_type: i64_ty,
            locals: vec![LocalDecl {
                local: Local(0),
                ty: i64_ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            }],
            blocks: vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            is_async: true,
            span: Span::DUMMY,
        };

        let mut mir = Mir {
            functions: HashMap::new(),
        };
        mir.functions.insert(func_def, func);

        let output = transform(AsyncTransformInput {
            mir,
            table,
            definitions,
        });

        // No poll functions should be generated (no await points).
        assert!(
            output.poll_fn_map.is_empty(),
            "Expected no poll functions for async fn without awaits"
        );
        // Original function should still exist.
        assert!(output.mir.functions.contains_key(&func_def));
    }

    #[test]
    fn transform_generates_poll_function() {
        let mut table = TypeTable::new();
        let mut definitions = DefinitionTable::new();

        let await_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::BuiltinFunction,
            kw::SPARK_AWAIT,
            Span::DUMMY,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));

        let func_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            sym("my_async"),
            Span::DUMMY,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));

        let func = make_simple_async_fn(func_def, await_def, &mut table);
        let mut mir = Mir {
            functions: HashMap::new(),
        };
        mir.functions.insert(func_def, func);

        let output = transform(AsyncTransformInput {
            mir,
            table,
            definitions,
        });

        // Should have a poll function mapping.
        assert!(
            output.poll_fn_map.contains_key(&func_def),
            "Expected poll function mapping for async fn"
        );

        let poll_info = &output.poll_fn_map[&func_def];

        // Poll function should exist in the MIR.
        assert!(
            output.mir.functions.contains_key(&poll_info.poll_def_id),
            "Poll function should be in MIR"
        );

        // Original function should still exist.
        assert!(
            output.mir.functions.contains_key(&func_def),
            "Original function should be preserved"
        );

        // Poll function should have param_count = 1 (state_ptr).
        let poll_fn = &output.mir.functions[&poll_info.poll_def_id];
        assert_eq!(poll_fn.param_count, 1);
        assert!(!poll_fn.is_async);

        // State fields should include __tag, __sub_future, saved locals, __result.
        assert!(
            poll_info.state_fields.len() >= 3,
            "Expected at least tag + sub_future + result fields, got {}",
            poll_info.state_fields.len()
        );
        assert_eq!(poll_info.state_fields[0].0, sym("__tag"));
        assert_eq!(
            poll_info.state_fields.last().unwrap().0,
            sym("__result")
        );
    }

    #[test]
    fn liveness_analysis_basic() {
        let mut table = TypeTable::new();
        let mut definitions = DefinitionTable::new();

        let await_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::BuiltinFunction,
            kw::SPARK_AWAIT,
            Span::DUMMY,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));

        let func_def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            sym("my_async"),
            Span::DUMMY,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));

        let func = make_simple_async_fn(func_def, await_def, &mut table);
        let await_points = find_await_points(&func, &definitions);
        let live_across = compute_live_across_awaits(&func, &await_points);

        assert_eq!(live_across.len(), 1, "Expected one live-across set");
        // Local(4) is the await dest — should NOT be in live-across.
        assert!(
            !live_across[0].contains(&Local(4)),
            "Await destination should not be live-across"
        );
        // Local(0) (return place) should NOT be in live-across.
        assert!(
            !live_across[0].contains(&Local(0)),
            "Return place should not be live-across"
        );
    }
}
