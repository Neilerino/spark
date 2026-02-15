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

    // Ensure ALL params have state fields (for segment 0 restoration and
    // cooperative spawn parameter injection), even if they're not live-across-await.
    for param_idx in 1..=func.param_count {
        let local = Local(param_idx);
        if !all_live.contains(&local) {
            let local_decl = &func.locals[local.index()];
            let field_name = sym(&format!("__local_{}", local.0));
            fields.push((field_name, local_decl.ty));
        }
    }

    // Result field.
    fields.push((sym("__result"), func.return_type));

    fields
}

// ── Poll Function Generation ────────────────────────────────────────────

/// Generates the poll function MIR (segmented state machine).
///
/// The poll function has signature `fn(state_ptr: ptr) -> i8` where:
/// - Returns 0 = Pending (awaited sub-future not ready)
/// - Returns 1 = Ready (function completed, result in state.__result)
///
/// The function body is a segmented dispatch table:
/// 1. Load `state.__tag`, dispatch to the correct segment
/// 2. Segment 0: restore params from state, execute original code up to first await
/// 3. At await: save live locals, store sub-future, set tag = next segment, return 0
/// 4. Segment K (K>0): call `__spark_try_poll(sub_future)`, if null return 0 (Pending),
///    else restore live locals, load result, continue execution
/// 5. On function return: store result in `state.__result`, return 1 (Ready)
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn generate_poll_function(
    original: &MirFunction,
    poll_def_id: DefId,
    state_type_id: TypeId,
    state_fields: &[(Symbol, TypeId)],
    await_points: &[AwaitPoint],
    live_across: &[FxHashSet<Local>],
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

    // Map: original Local → poll function Local.
    let mut orig_to_poll: FxHashMap<Local, Local> = FxHashMap::default();

    // We need locals for ALL original locals because segments execute cloned
    // original code. We only save/restore the live-across-await ones.
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

    // Local for try_poll result pointer.
    let try_poll_result_local =
        alloc_local(&mut locals, &mut next_local_id, i64_ty, Some(sym("__poll_result")), original.span);

    // Build field name → field index map for state struct access.
    let field_index: FxHashMap<Symbol, usize> = state_fields
        .iter()
        .enumerate()
        .map(|(i, (name, _))| (*name, i))
        .collect();

    // Re-type state_ptr to state_type_id so Field projections resolve correctly.
    locals[state_ptr_local.index()].ty = state_type_id;

    // Compute segment ranges via BFS from entry/resume points.
    let segments = compute_segment_ranges(original, await_points);

    // ── Build blocks ────────────────────────────────────────────────────
    //
    // Phase 3 v2: Segmented state machine.
    //
    // Structure:
    //   bb0 (entry): load tag, switchInt → [0: seg0_entry, 1: seg1_entry, ..., else: already_ready]
    //   seg0_entry: restore params from state → jump to cloned segment 0 blocks
    //     ... cloned blocks from segment 0 ...
    //     await_call_block (rewritten): save live locals, set tag, return 0
    //   seg1_entry: try_poll(sub_future) → if null return 0, else restore + jump
    //     ... cloned blocks from segment 1 ...
    //   [more segments...]
    //   already_ready: return 1

    let mut blocks: Vec<BasicBlock> = Vec::new();
    let num_segments = segments.len();

    // We'll build segment entry block indices as we go.
    // First, reserve block 0 (entry) — we'll fill it later when we know all targets.
    blocks.push(BasicBlock {
        statements: vec![],
        terminator: Terminator::Unreachable, // placeholder
        is_cleanup: false,
    });

    // Build a block index for the "already_ready" block — placed after all segments.
    // We'll track its final index.
    let mut segment_entry_indices: Vec<usize> = Vec::with_capacity(num_segments);

    // For each segment, build: entry block + cloned blocks from original.
    for seg_idx in 0..num_segments {
        let seg_entry_block_idx = blocks.len();
        segment_entry_indices.push(seg_entry_block_idx);

        let mut entry_stmts = Vec::new();

        if seg_idx == 0 {
            // Segment 0: restore params from state.
            for param_idx in 1..=original.param_count {
                let orig_local = Local(param_idx);
                let field_name = sym(&format!("__local_{param_idx}"));

                if field_index.contains_key(&field_name) {
                    if let Some(&poll_local) = orig_to_poll.get(&orig_local) {
                        entry_stmts.push(Statement::Assign {
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
        } else {
            // Segment K>0: poll the sub-future, check result.
            // Load sub_future from state.__sub_future
            let sub_future_local = alloc_local(
                &mut locals,
                &mut next_local_id,
                i64_ty,
                None,
                original.span,
            );
            entry_stmts.push(Statement::Assign {
                place: Place::from_local(sub_future_local),
                rvalue: Rvalue::Use(Operand::Copy(Place {
                    local: state_ptr_local,
                    projections: vec![Projection::Field(sym("__sub_future"))],
                })),
                span: original.span,
            });

            // Allocate a local for the __spark_try_poll function ref.
            let try_poll_fn_local = alloc_local(
                &mut locals,
                &mut next_local_id,
                i64_ty,
                Some(sym("__try_poll_fn")),
                original.span,
            );

            // Find try_poll DefId — we'll use kw::TRY_POLL as a FunctionRef constant.
            // Since the resolver registers it, we search definitions at runtime.
            // But in MIR we don't have the DefId directly. We'll use a sentinel
            // approach: emit FunctionRef with a placeholder DefId that codegen will
            // resolve by name. Actually, since try_poll is a builtin and codegen
            // intercepts it by name, we need its DefId. We'll store the FunctionRef
            // constant and let codegen intercept.
            //
            // For now, emit as a call terminator to a try_poll FunctionRef.
            // We need to know the try_poll DefId. Since we're in the MIR transform
            // and have access to definitions via the outer function, we'll need it
            // passed in. Add a parameter for try_poll_def_id.
            //
            // However, to avoid changing the function signature too much, we'll
            // search for it lazily. Actually the simpler approach: the poll function
            // just uses Operand::Constant(Constant::FunctionRef(try_poll_def_id)).
            // We need the caller to pass it. Let's use a helper.
            //
            // Actually, the cleanest approach is: store the try_poll FunctionRef
            // in a local and use Call terminator. The try_poll_def_id will be
            // passed into generate_poll_function.
            //
            // For now, we'll add try_poll_def_id as a field that we stash.
            // This will be resolved after we update the call site below.
            //
            // PLAN: We'll emit the try_poll call as a Call terminator. The
            // entry block ends with a Call to try_poll, and the continuation
            // block checks the result.

            // We'll split: entry_stmts sets up sub_future + try_poll fn ref,
            // then a Call terminator calls try_poll, then a continuation block
            // checks the result pointer (null = pending, non-null = ready).

            // First, assign the try_poll function ref.
            // We use TRY_POLL_DEF_ID placeholder — will be set by caller.
            entry_stmts.push(Statement::Assign {
                place: Place::from_local(try_poll_fn_local),
                rvalue: Rvalue::Use(Operand::Constant(Constant::Null)),
                span: original.span,
            });

            // Push the entry block with the Call terminator.
            let poll_cont_block_idx = blocks.len() + 1;
            blocks.push(BasicBlock {
                statements: entry_stmts,
                terminator: Terminator::Call {
                    func: Operand::Copy(Place::from_local(try_poll_fn_local)),
                    args: vec![Operand::Copy(Place::from_local(sub_future_local))],
                    destination: Place::from_local(try_poll_result_local),
                    target: BlockId(poll_cont_block_idx as u32),
                    span: original.span,
                },
                is_cleanup: false,
            });

            // Continuation block: check if result_ptr is null.
            // If null → return 0 (Pending).
            // If non-null → restore locals, load await result, jump to cloned resume.

            let mut cont_stmts = Vec::new();

            // Compare result_ptr == 0
            let cmp_local = alloc_local(
                &mut locals,
                &mut next_local_id,
                table.intern(Ty::Bool),
                None,
                original.span,
            );
            cont_stmts.push(Statement::Assign {
                place: Place::from_local(cmp_local),
                rvalue: Rvalue::BinaryOp(
                    spark_ast::BinaryOp::Eq,
                    Operand::Copy(Place::from_local(try_poll_result_local)),
                    Operand::Constant(Constant::Int(0)),
                ),
                span: original.span,
            });

            // We need two targets: pending_block and ready_block.
            let pending_block_idx = blocks.len() + 1;
            let ready_block_idx = blocks.len() + 2;

            blocks.push(BasicBlock {
                statements: cont_stmts,
                terminator: Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::from_local(cmp_local)),
                    targets: vec![(1, BlockId(pending_block_idx as u32))], // null → pending
                    otherwise: BlockId(ready_block_idx as u32),             // non-null → ready
                },
                is_cleanup: false,
            });

            // Pending block: return 0.
            blocks.push(BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local::RETURN_PLACE),
                    rvalue: Rvalue::Use(Operand::Constant(Constant::Int(0))),
                    span: original.span,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            });

            // Ready block: load result from try_poll, restore live locals, jump to cloned resume.
            let mut ready_stmts = Vec::new();

            // Load await result: the try_poll returns a pointer to the result buffer.
            // We need to load the result value. In MIR we represent this as
            // assigning from the result pointer. However, the await dest local
            // receives the concrete type. In the poll function, we'll use a
            // Deref projection to load through the pointer.
            //
            // Actually, the simplest approach for MIR: the try_poll_result_local
            // holds a pointer. We need to load the concrete value from it.
            // MIR doesn't have a "load from raw pointer" operation directly.
            // We use Projection::Deref on a Ref-typed local.
            //
            // Simpler: store try_poll result as an Int (raw ptr), then codegen
            // will handle loading the actual value from the pointer based on the
            // dest type. We'll just do a Copy of try_poll_result_local into the
            // await dest local — codegen will need to load from the pointer.
            //
            // For now, store the pointer in the dest local. Codegen intercepts
            // try_poll calls and loads the result. (Step 6 handles this.)
            let prev_await = &await_points[seg_idx - 1];
            if let Some(&poll_dest_local) = orig_to_poll.get(&prev_await.dest_local) {
                ready_stmts.push(Statement::Assign {
                    place: Place::from_local(poll_dest_local),
                    rvalue: Rvalue::Use(Operand::Copy(Place::from_local(try_poll_result_local))),
                    span: original.span,
                });
            }

            // Restore live-across-await locals from state.
            let live_set = &live_across[seg_idx - 1];
            for &live_local in live_set {
                let field_name = sym(&format!("__local_{}", live_local.0));
                if field_index.contains_key(&field_name) {
                    if let Some(&poll_local) = orig_to_poll.get(&live_local) {
                        ready_stmts.push(Statement::Assign {
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

            // The ready block will jump to the first cloned block of this segment.
            // We'll set the terminator after we know the cloned block index.
            let ready_block_actual_idx = blocks.len();
            blocks.push(BasicBlock {
                statements: ready_stmts,
                terminator: Terminator::Unreachable, // placeholder — fixed below
                is_cleanup: false,
            });

            // Now clone the segment's blocks.
            let seg_blocks = &segments[seg_idx];
            if seg_blocks.is_empty() {
                // Edge case: empty segment — ready block goes to already_ready.
                // Fix the terminator below after we know already_ready index.
                // For now, leave as Unreachable — will be patched.
            } else {
                let first_cloned_block_idx = blocks.len();
                // Fix ready block terminator to jump to first cloned block.
                blocks[ready_block_actual_idx].terminator =
                    Terminator::Goto { target: BlockId(first_cloned_block_idx as u32) };

                // Build the block remapping for this segment.
                let remap = build_block_remap(seg_blocks, blocks.len());

                for &orig_block_idx in seg_blocks {
                    let cloned = clone_block_with_remap(
                        &original.blocks[orig_block_idx],
                        &orig_to_poll,
                        &remap,
                        original.span,
                    );
                    blocks.push(cloned);
                }

                // Now rewrite: if this segment contains an await point (i.e., not the
                // last segment), rewrite the await call block's terminator.
                if seg_idx < await_points.len() {
                    let ap = &await_points[seg_idx];
                    // Find which cloned block corresponds to ap.call_block_idx.
                    if let Some(pos) = seg_blocks.iter().position(|&b| b == ap.call_block_idx) {
                        let cloned_block_idx = first_cloned_block_idx + pos;
                        rewrite_await_block(
                            &mut blocks[cloned_block_idx],
                            &orig_to_poll,
                            &live_across[seg_idx],
                            &field_index,
                            state_ptr_local,
                            ap,
                            (seg_idx + 1) as i64,
                            original.span,
                        );
                    }
                }

                // Rewrite Return terminators in cloned blocks → store result + return 1.
                for block_idx in first_cloned_block_idx..blocks.len() {
                    if matches!(blocks[block_idx].terminator, Terminator::Return) {
                        rewrite_return_block(
                            &mut blocks[block_idx],
                            &orig_to_poll,
                            state_ptr_local,
                            original.span,
                        );
                    }
                }
            }

            continue; // Skip the segment 0 path below
        }

        // ── Segment 0 path ──────────────────────────────────────────────
        // The entry block for segment 0 restores params, then jumps to
        // the first cloned block of segment 0.

        let seg_blocks = &segments[seg_idx];
        if seg_blocks.is_empty() {
            // Empty segment 0 — just push the entry block going to already_ready.
            blocks.push(BasicBlock {
                statements: entry_stmts,
                terminator: Terminator::Unreachable, // patched below
                is_cleanup: false,
            });
            continue;
        }

        let first_cloned_block_idx = blocks.len() + 1;
        blocks.push(BasicBlock {
            statements: entry_stmts,
            terminator: Terminator::Goto { target: BlockId(first_cloned_block_idx as u32) },
            is_cleanup: false,
        });

        // Build the block remapping for segment 0.
        let remap = build_block_remap(seg_blocks, blocks.len());

        for &orig_block_idx in seg_blocks {
            let cloned = clone_block_with_remap(
                &original.blocks[orig_block_idx],
                &orig_to_poll,
                &remap,
                original.span,
            );
            blocks.push(cloned);
        }

        let first_cloned = first_cloned_block_idx;

        // If segment 0 has an await point, rewrite the await call block.
        if !await_points.is_empty() {
            let ap = &await_points[0];
            if let Some(pos) = seg_blocks.iter().position(|&b| b == ap.call_block_idx) {
                let cloned_block_idx = first_cloned + pos;
                rewrite_await_block(
                    &mut blocks[cloned_block_idx],
                    &orig_to_poll,
                    &live_across[0],
                    &field_index,
                    state_ptr_local,
                    ap,
                    1, // next segment tag
                    original.span,
                );
            }
        }

        // Rewrite Return terminators in cloned blocks.
        for block_idx in first_cloned..blocks.len() {
            if matches!(blocks[block_idx].terminator, Terminator::Return) {
                rewrite_return_block(
                    &mut blocks[block_idx],
                    &orig_to_poll,
                    state_ptr_local,
                    original.span,
                );
            }
        }
    }

    // Already-ready block: return 1.
    let already_ready_idx = blocks.len();
    blocks.push(BasicBlock {
        statements: vec![Statement::Assign {
            place: Place::from_local(Local::RETURN_PLACE),
            rvalue: Rvalue::Use(Operand::Constant(Constant::Int(1))),
            span: original.span,
        }],
        terminator: Terminator::Return,
        is_cleanup: false,
    });

    // Fix the entry block (bb0): dispatch on tag.
    let mut switch_targets: Vec<(i128, BlockId)> = Vec::with_capacity(num_segments);
    for (seg_idx, &entry_idx) in segment_entry_indices.iter().enumerate() {
        switch_targets.push((seg_idx as i128, BlockId(entry_idx as u32)));
    }

    blocks[0] = BasicBlock {
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
            targets: switch_targets,
            otherwise: BlockId(already_ready_idx as u32),
        },
        is_cleanup: false,
    };

    // Patch any remaining Unreachable terminators (from empty segments) to go to already_ready.
    for block in &mut blocks {
        if matches!(block.terminator, Terminator::Unreachable) {
            block.terminator = Terminator::Goto { target: BlockId(already_ready_idx as u32) };
        }
    }

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

/// Computes which original blocks belong to each segment via BFS.
///
/// Segment 0: blocks reachable from entry, stopping at first await call block.
/// Segment K: blocks reachable from `await_points[K-1].resume_block`, stopping at
///            `await_points[K].call_block` (if K < await_points.len()).
/// Last segment: blocks reachable from last resume block, no barrier.
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

    let successors = build_successor_map(func);

    for seg_idx in 0..num_segments {
        let entry = if seg_idx == 0 {
            0 // entry block
        } else {
            await_points[seg_idx - 1].resume_block.index()
        };

        // Barrier: the await call block for this segment's await point (if any).
        let barrier = if seg_idx < await_points.len() {
            Some(await_points[seg_idx].call_block_idx)
        } else {
            None
        };

        // BFS from entry, including the barrier block itself but not going past it.
        let mut visited = FxHashSet::default();
        let mut queue = vec![entry];
        while let Some(idx) = queue.pop() {
            if !visited.insert(idx) {
                continue;
            }
            // If this is the barrier block, include it but don't explore successors.
            if barrier == Some(idx) {
                continue;
            }
            for &succ in &successors[idx] {
                if !visited.contains(&succ) {
                    queue.push(succ);
                }
            }
        }

        let mut seg: Vec<usize> = visited.into_iter().collect();
        seg.sort_unstable();
        segments[seg_idx] = seg;
    }

    segments
}

/// Builds a mapping from original block index → cloned block index.
fn build_block_remap(
    seg_blocks: &[usize],
    cloned_start: usize,
) -> FxHashMap<usize, usize> {
    seg_blocks
        .iter()
        .enumerate()
        .map(|(i, &orig_idx)| (orig_idx, cloned_start + i))
        .collect()
}

/// Clones a block with local and block ID remapping.
fn clone_block_with_remap(
    block: &BasicBlock,
    local_remap: &FxHashMap<Local, Local>,
    block_remap: &FxHashMap<usize, usize>,
    _span: spark_span::Span,
) -> BasicBlock {
    let statements = block
        .statements
        .iter()
        .map(|stmt| remap_statement(stmt, local_remap))
        .collect();

    let terminator = remap_terminator(&block.terminator, local_remap, block_remap);

    BasicBlock {
        statements,
        terminator,
        is_cleanup: block.is_cleanup,
    }
}

/// Remaps a statement's locals.
fn remap_statement(
    stmt: &Statement,
    local_remap: &FxHashMap<Local, Local>,
) -> Statement {
    match stmt {
        Statement::Assign { place, rvalue, span } => Statement::Assign {
            place: remap_place(place, local_remap),
            rvalue: remap_rvalue(rvalue, local_remap),
            span: *span,
        },
        Statement::Drop { place, span } => Statement::Drop {
            place: remap_place(place, local_remap),
            span: *span,
        },
        Statement::Nop => Statement::Nop,
    }
}

/// Remaps a place's local.
fn remap_place(place: &Place, local_remap: &FxHashMap<Local, Local>) -> Place {
    Place {
        local: local_remap.get(&place.local).copied().unwrap_or(place.local),
        projections: place.projections.iter().map(|p| remap_projection(p, local_remap)).collect(),
    }
}

/// Remaps a projection's index local (if any).
fn remap_projection(proj: &Projection, local_remap: &FxHashMap<Local, Local>) -> Projection {
    match proj {
        Projection::Index(idx_local) => {
            Projection::Index(local_remap.get(idx_local).copied().unwrap_or(*idx_local))
        }
        other => other.clone(),
    }
}

/// Remaps an operand's locals.
fn remap_operand(op: &Operand, local_remap: &FxHashMap<Local, Local>) -> Operand {
    match op {
        Operand::Copy(place) => Operand::Copy(remap_place(place, local_remap)),
        Operand::Move(place) => Operand::Move(remap_place(place, local_remap)),
        Operand::Constant(c) => Operand::Constant(c.clone()),
    }
}

/// Remaps an rvalue's locals.
fn remap_rvalue(rvalue: &Rvalue, local_remap: &FxHashMap<Local, Local>) -> Rvalue {
    match rvalue {
        Rvalue::Use(op) => Rvalue::Use(remap_operand(op, local_remap)),
        Rvalue::BinaryOp(op, a, b) => Rvalue::BinaryOp(
            *op,
            remap_operand(a, local_remap),
            remap_operand(b, local_remap),
        ),
        Rvalue::UnaryOp(op, a) => Rvalue::UnaryOp(*op, remap_operand(a, local_remap)),
        Rvalue::Ref { place, mutable } => Rvalue::Ref {
            place: remap_place(place, local_remap),
            mutable: *mutable,
        },
        Rvalue::Aggregate(kind, ops) => Rvalue::Aggregate(
            kind.clone(),
            ops.iter().map(|o| remap_operand(o, local_remap)).collect(),
        ),
        Rvalue::Discriminant(place) => Rvalue::Discriminant(remap_place(place, local_remap)),
        Rvalue::Cast { operand, target_ty } => Rvalue::Cast {
            operand: remap_operand(operand, local_remap),
            target_ty: *target_ty,
        },
        Rvalue::StringInterp(ops) => Rvalue::StringInterp(
            ops.iter().map(|o| remap_operand(o, local_remap)).collect(),
        ),
    }
}

/// Remaps a terminator's locals and block IDs.
fn remap_terminator(
    term: &Terminator,
    local_remap: &FxHashMap<Local, Local>,
    block_remap: &FxHashMap<usize, usize>,
) -> Terminator {
    let remap_block = |bid: &BlockId| -> BlockId {
        block_remap
            .get(&bid.index())
            .map_or(*bid, |&new_idx| BlockId(new_idx as u32))
    };

    match term {
        Terminator::Goto { target } => Terminator::Goto {
            target: remap_block(target),
        },
        Terminator::Return => Terminator::Return,
        Terminator::Unreachable => Terminator::Unreachable,
        Terminator::SwitchInt {
            discriminant,
            targets,
            otherwise,
        } => Terminator::SwitchInt {
            discriminant: remap_operand(discriminant, local_remap),
            targets: targets
                .iter()
                .map(|(val, bid)| (*val, remap_block(bid)))
                .collect(),
            otherwise: remap_block(otherwise),
        },
        Terminator::Call {
            func,
            args,
            destination,
            target,
            span,
        } => Terminator::Call {
            func: remap_operand(func, local_remap),
            args: args.iter().map(|a| remap_operand(a, local_remap)).collect(),
            destination: remap_place(destination, local_remap),
            target: remap_block(target),
            span: *span,
        },
    }
}

/// Rewrites an await call block to save state and return Pending.
///
/// Replaces the `Call { __spark_await }` terminator with:
/// 1. Save each live-across-await local → `state.__local_N`
/// 2. Store future pointer → `state.__sub_future`
/// 3. Set `state.__tag` = next_tag
/// 4. Return 0 (Pending)
#[allow(clippy::too_many_arguments)]
fn rewrite_await_block(
    block: &mut BasicBlock,
    orig_to_poll: &FxHashMap<Local, Local>,
    live_set: &FxHashSet<Local>,
    field_index: &FxHashMap<Symbol, usize>,
    state_ptr_local: Local,
    await_point: &AwaitPoint,
    next_tag: i64,
    span: spark_span::Span,
) {
    // The block currently ends with a Call terminator to __spark_await.
    // Extract the future argument local from the call.
    let future_poll_local = orig_to_poll
        .get(&await_point.future_local)
        .copied()
        .unwrap_or(await_point.future_local);

    let mut save_stmts = Vec::new();

    // Save each live-across-await local to the state struct.
    for &live_local in live_set {
        let field_name = sym(&format!("__local_{}", live_local.0));
        if field_index.contains_key(&field_name) {
            let poll_local = orig_to_poll
                .get(&live_local)
                .copied()
                .unwrap_or(live_local);
            save_stmts.push(Statement::Assign {
                place: Place {
                    local: state_ptr_local,
                    projections: vec![Projection::Field(field_name)],
                },
                rvalue: Rvalue::Use(Operand::Copy(Place::from_local(poll_local))),
                span,
            });
        }
    }

    // Store sub_future pointer to state.__sub_future.
    save_stmts.push(Statement::Assign {
        place: Place {
            local: state_ptr_local,
            projections: vec![Projection::Field(sym("__sub_future"))],
        },
        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(future_poll_local))),
        span,
    });

    // Set state.__tag = next_tag.
    save_stmts.push(Statement::Assign {
        place: Place {
            local: state_ptr_local,
            projections: vec![Projection::Field(sym("__tag"))],
        },
        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(next_tag))),
        span,
    });

    // Return 0 (Pending).
    save_stmts.push(Statement::Assign {
        place: Place::from_local(Local::RETURN_PLACE),
        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(0))),
        span,
    });

    // Replace the block's statements (keep everything before the call, add saves).
    block.statements.append(&mut save_stmts);
    block.terminator = Terminator::Return;
}

/// Rewrites a Return terminator in a cloned block to store the result in
/// `state.__result`, set `state.__tag` to a completed value, and return 1 (Ready).
fn rewrite_return_block(
    block: &mut BasicBlock,
    orig_to_poll: &FxHashMap<Local, Local>,
    state_ptr_local: Local,
    span: spark_span::Span,
) {
    // The original function stores its result in Local(0) (return place).
    // We need to copy that to state.__result.
    let poll_return_local = orig_to_poll
        .get(&Local::RETURN_PLACE)
        .copied()
        .unwrap_or(Local::RETURN_PLACE);

    let mut store_stmts = Vec::new();

    // Store result: state.__result = poll_return_local (which mirrors original Local(0))
    store_stmts.push(Statement::Assign {
        place: Place {
            local: state_ptr_local,
            projections: vec![Projection::Field(sym("__result"))],
        },
        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(poll_return_local))),
        span,
    });

    // Set completed tag.
    store_stmts.push(Statement::Assign {
        place: Place {
            local: state_ptr_local,
            projections: vec![Projection::Field(sym("__tag"))],
        },
        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(-1))),
        span,
    });

    // Return 1 (Ready).
    store_stmts.push(Statement::Assign {
        place: Place::from_local(Local::RETURN_PLACE),
        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(1))),
        span,
    });

    block.statements.append(&mut store_stmts);
    // Terminator stays as Return.
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

    #[test]
    fn segmented_poll_has_dispatch_entry() {
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

        let poll_info = &output.poll_fn_map[&func_def];
        let poll_fn = &output.mir.functions[&poll_info.poll_def_id];

        // Entry block (bb0) should be a SwitchInt on the tag.
        assert!(
            matches!(poll_fn.blocks[0].terminator, Terminator::SwitchInt { .. }),
            "Poll function entry should dispatch via SwitchInt on tag"
        );

        // Should have at least 2 segments (segment 0 + segment 1) + entry + already_ready.
        assert!(
            poll_fn.blocks.len() >= 4,
            "Expected at least 4 blocks (entry + 2 segments + already_ready), got {}",
            poll_fn.blocks.len()
        );

        // At least one block should return 0 (Pending).
        let has_pending_return = poll_fn.blocks.iter().any(|b| {
            b.statements.iter().any(|s| {
                matches!(s,
                    Statement::Assign { rvalue: Rvalue::Use(Operand::Constant(Constant::Int(0))), .. }
                )
            }) && matches!(b.terminator, Terminator::Return)
        });
        assert!(has_pending_return, "Expected a block that returns 0 (Pending)");

        // At least one block should return 1 (Ready).
        let has_ready_return = poll_fn.blocks.iter().any(|b| {
            b.statements.iter().any(|s| {
                matches!(s,
                    Statement::Assign { rvalue: Rvalue::Use(Operand::Constant(Constant::Int(1))), .. }
                )
            }) && matches!(b.terminator, Terminator::Return)
        });
        assert!(has_ready_return, "Expected a block that returns 1 (Ready)");
    }

    #[test]
    fn multi_await_creates_multiple_segments() {
        let mut table = TypeTable::new();
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));
        let void_ty = table.intern(Ty::Void);
        let future_ty = table.intern(Ty::Future { inner: i64_ty });
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
            sym("pipeline"),
            Span::DUMMY,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));

        // Async function with 2 awaits:
        //   _0: Int (return)
        //   _1: Future[Int] (future 1)
        //   _2: fn (await ref)
        //   _3: Int (await result 1)
        //   _4: Future[Int] (future 2)
        //   _5: fn (await ref 2)
        //   _6: Int (await result 2)
        //
        // bb0: _1 = 0; _2 = FunctionRef(__spark_await); call _2(_1) → bb1, dest = _3
        // bb1: _4 = 0; _5 = FunctionRef(__spark_await); call _5(_4) → bb2, dest = _6
        // bb2: _0 = _3 + _6; return
        let func = MirFunction {
            def_id: func_def,
            param_count: 0,
            return_type: i64_ty,
            locals: vec![
                LocalDecl { local: Local(0), ty: i64_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(1), ty: future_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(2), ty: void_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(3), ty: i64_ty, name: Some(sym("a")), is_user_variable: true, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(4), ty: future_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(5), ty: void_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(6), ty: i64_ty, name: Some(sym("b")), is_user_variable: true, mutable: false, span: Span::DUMMY },
            ],
            blocks: vec![
                BasicBlock {
                    statements: vec![
                        Statement::Assign { place: Place::from_local(Local(1)), rvalue: Rvalue::Use(Operand::Constant(Constant::Int(0))), span: Span::DUMMY },
                        Statement::Assign { place: Place::from_local(Local(2)), rvalue: Rvalue::Use(Operand::Constant(Constant::FunctionRef(await_def))), span: Span::DUMMY },
                    ],
                    terminator: Terminator::Call {
                        func: Operand::Copy(Place::from_local(Local(2))),
                        args: vec![Operand::Copy(Place::from_local(Local(1)))],
                        destination: Place::from_local(Local(3)),
                        target: BlockId(1),
                        span: Span::DUMMY,
                    },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![
                        Statement::Assign { place: Place::from_local(Local(4)), rvalue: Rvalue::Use(Operand::Constant(Constant::Int(0))), span: Span::DUMMY },
                        Statement::Assign { place: Place::from_local(Local(5)), rvalue: Rvalue::Use(Operand::Constant(Constant::FunctionRef(await_def))), span: Span::DUMMY },
                    ],
                    terminator: Terminator::Call {
                        func: Operand::Copy(Place::from_local(Local(5))),
                        args: vec![Operand::Copy(Place::from_local(Local(4)))],
                        destination: Place::from_local(Local(6)),
                        target: BlockId(2),
                        span: Span::DUMMY,
                    },
                    is_cleanup: false,
                },
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::BinaryOp(
                            spark_ast::BinaryOp::Add,
                            Operand::Copy(Place::from_local(Local(3))),
                            Operand::Copy(Place::from_local(Local(6))),
                        ),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
            captures: vec![],
            is_async: true,
            span: Span::DUMMY,
        };

        let mut mir = Mir { functions: HashMap::new() };
        mir.functions.insert(func_def, func);

        let output = transform(AsyncTransformInput {
            mir,
            table,
            definitions,
        });

        assert!(output.poll_fn_map.contains_key(&func_def));
        let poll_info = &output.poll_fn_map[&func_def];
        let poll_fn = &output.mir.functions[&poll_info.poll_def_id];

        // Entry block should dispatch to 3 segments (0, 1, 2).
        if let Terminator::SwitchInt { targets, .. } = &poll_fn.blocks[0].terminator {
            assert_eq!(
                targets.len(), 3,
                "Expected 3 segment targets (for 2 awaits), got {}",
                targets.len()
            );
        } else {
            panic!("Entry block should be SwitchInt");
        }

        // Local(3) = `a` is used in bb2 (after both awaits), so it should be
        // live-across the second await → has a state field.
        let has_local_3_field = poll_info
            .state_fields
            .iter()
            .any(|(name, _)| name.as_str() == "__local_3");
        assert!(has_local_3_field, "Local(3) 'a' should be in state fields (live-across-await)");
    }

    #[test]
    fn params_always_in_state_fields() {
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

        // In make_simple_async_fn, Local(1) is param x but is never used
        // in the body. With the fix, it should still be in state fields
        // so that cooperative spawn can inject the param value.
        let mut mir = Mir { functions: HashMap::new() };
        mir.functions.insert(func_def, func);

        let output = transform(AsyncTransformInput {
            mir,
            table,
            definitions,
        });

        let poll_info = &output.poll_fn_map[&func_def];

        // Param Local(1) should have a __local_1 field even though it's not live-across-await.
        let has_param_field = poll_info
            .state_fields
            .iter()
            .any(|(name, _)| name.as_str() == "__local_1");
        assert!(
            has_param_field,
            "Param Local(1) should always be in state fields for coop spawn"
        );

        // __result should still be the last field.
        assert_eq!(
            poll_info.state_fields.last().unwrap().0,
            sym("__result"),
            "__result should be the last state field"
        );
    }
}
