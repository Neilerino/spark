//! Async state machine transform for cooperative scheduling.
//!
//! Transforms async functions that contain `await` points into poll-based
//! state machines. A poll function preserves the original control-flow graph,
//! dispatches to saved await points, and returns Pending (0) or Ready (1).
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

mod forwarding;
mod promotion;
pub use forwarding::{SpawnAdapter, SpawnArgument};

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
    /// Mapping from original async function `DefId` → poll function `DefId`.
    pub poll_fn_map: FxHashMap<DefId, PollFnInfo>,
    /// Diagnostics accumulated during the transform.
    pub diagnostics: Diagnostics,
}

/// Information about a generated poll function.
#[derive(Debug, Clone)]
pub struct PollFnInfo {
    /// The poll function's `DefId`.
    pub poll_def_id: DefId,
    /// The state struct type (`Ty::AsyncState`).
    pub state_type_id: TypeId,
    /// Field names and types in the state struct, in order.
    pub state_fields: Vec<(Symbol, TypeId)>,
    /// Certified callback bindings, keyed by the original callback definition.
    pub spawn_adapters: FxHashMap<DefId, SpawnAdapter>,
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

    let mut diagnostics = Diagnostics::new();
    let try_poll = definitions
        .iter()
        .find(|(_, info)| info.name == kw::TRY_POLL)
        .map(|(id, _)| id);
    let mut poll_fn_map = FxHashMap::default();

    // Phase 1: Collect async functions with await points.
    let async_def_ids: Vec<DefId> = mir
        .functions
        .iter()
        .filter(|(_, f)| f.is_async)
        .map(|(def_id, _)| *def_id)
        .collect();

    for def_id in &async_def_ids {
        let Some(func) = mir.functions.get(def_id) else {
            continue;
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
        let all_live: FxHashSet<Local> = live_across
            .iter()
            .flat_map(|s| s.iter().copied())
            .chain(await_points.iter().map(|point| point.future_local))
            .chain(func.address_taken_locals())
            .collect();
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

        let try_poll = try_poll.unwrap_or_else(|| {
            definitions.insert(DefinitionInfo::new(
                DefinitionKind::BuiltinFunction,
                kw::TRY_POLL,
                func.span,
                Visibility::Private,
                spark_modules::ModulePath::root(),
            ))
        });
        // Phase 3b: Generate the poll function MIR.
        let poll_fn = generate_poll_function(
            func,
            poll_def_id,
            state_type_id,
            &state_fields,
            &await_points,
            &live_across,
            try_poll,
            &mut table,
            &mut diagnostics,
        );

        poll_fn_map.insert(
            *def_id,
            PollFnInfo {
                poll_def_id,
                state_type_id,
                state_fields: state_fields.clone(),
                spawn_adapters: FxHashMap::default(),
            },
        );

        mir.functions.insert(poll_def_id, poll_fn);
    }

    forwarding::attach(&mir, &table, &definitions, &mut poll_fn_map);

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
    resume_block: BlockId,
    /// The local that receives the await result.
    dest_local: Local,
    /// The local holding the future being awaited (argument to `__spark_await`).
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
            let callee_def_id = func.resolve_callee_def_id(callee_op);
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
        if let Terminator::Call { destination, .. } = &block.terminator {
            if destination.projections.is_empty() {
                kill[idx].insert(destination.local);
            } else {
                add_place_uses(destination, &mut gen[idx]);
            }
        }
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
                Statement::IndexedAssign {
                    root,
                    indices,
                    value,
                    ..
                } => {
                    add_place_uses(root, &mut gen[idx]);
                    gen[idx].extend(indices);
                    gen[idx].insert(*value);
                }
                Statement::Drop { place, .. } => add_place_uses(place, &mut gen[idx]),
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
    await_points
        .iter()
        .map(|ap| {
            let mut live = live_out[ap.call_block_idx].clone();
            live.remove(&ap.dest_local);
            live
        })
        .collect()
}

/// Adds uses from a terminator to the gen set.
fn add_terminator_uses(term: &Terminator, gen: &mut FxHashSet<Local>) {
    match term {
        Terminator::Return => {
            gen.insert(Local::RETURN_PLACE);
        }
        Terminator::Goto { .. } | Terminator::Unreachable => {}
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
        Rvalue::ReadFutureResult { pointer } => {
            gen.insert(*pointer);
        }
        Rvalue::Use(op) | Rvalue::Cast { operand: op, .. } | Rvalue::UnaryOp(_, op) => {
            add_operand_uses(op, gen);
        }
        Rvalue::BinaryOp(_, a, b) => {
            add_operand_uses(a, gen);
            add_operand_uses(b, gen);
        }
        Rvalue::Ref { place, .. } | Rvalue::Discriminant(place) => add_place_uses(place, gen),
        Rvalue::Aggregate(_, ops) | Rvalue::StringInterp(ops) => {
            for op in ops {
                add_operand_uses(op, gen);
            }
        }
        Rvalue::ListRange {
            start, end, step, ..
        }
        | Rvalue::RangeLen {
            start, end, step, ..
        } => {
            add_operand_uses(start, gen);
            add_operand_uses(end, gen);
            add_operand_uses(step, gen);
        }
        Rvalue::CollectionGetOptional { collection, index } => {
            add_place_uses(collection, gen);
            add_place_uses(index, gen);
        }
    }
}

/// Adds uses from an operand to the gen set.
fn add_operand_uses(op: &Operand, gen: &mut FxHashSet<Local>) {
    if let Operand::Copy(place) | Operand::Move(place) = op {
        add_place_uses(place, gen);
    }
}

fn add_place_uses(place: &Place, gen: &mut FxHashSet<Local>) {
    gen.insert(place.local);
    for projection in &place.projections {
        if let Projection::Index(index) = projection {
            gen.insert(*index);
        }
    }
}

/// Builds a map from block index → successor block indices.
fn build_successor_map(func: &MirFunction) -> Vec<Vec<usize>> {
    func.blocks
        .iter()
        .map(|block| match &block.terminator {
            Terminator::Goto { target } | Terminator::Call { target, .. } => {
                vec![target.index()]
            }
            Terminator::Return | Terminator::Unreachable => vec![],
            Terminator::SwitchInt {
                targets, otherwise, ..
            } => {
                let mut succs: Vec<usize> = targets.iter().map(|(_, b)| b.index()).collect();
                succs.push(otherwise.index());
                succs
            }
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

// TAG [SPARK-117]
fn poll_local(locals: &mut Vec<LocalDecl>, ty: TypeId, span: spark_span::Span) -> Local {
    let local = Local(locals.len() as u32);
    locals.push(LocalDecl {
        local,
        ty,
        name: None,
        is_user_variable: false,
        mutable: true,
        span,
    });
    local
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn generate_poll_function(
    original: &MirFunction,
    poll_def_id: DefId,
    state_type_id: TypeId,
    state_fields: &[(Symbol, TypeId)],
    await_points: &[AwaitPoint],
    live_across: &[FxHashSet<Local>],
    try_poll: DefId,
    table: &mut TypeTable,
    diagnostics: &mut Diagnostics,
) -> MirFunction {
    let status_ty = table.intern(Ty::Bool);
    let tag_ty = table.intern(Ty::Int(IntWidth::I32));
    let pointer_ty = table.intern(Ty::CPtr);
    let state_ref_ty = table.intern(Ty::Ref {
        inner: state_type_id,
        mutable: true,
        lifetime: spark_types::ty::LifetimeId::ANONYMOUS,
    });
    let span = original.span;
    let mut locals = Vec::new();
    poll_local(&mut locals, status_ty, span);
    let state = poll_local(&mut locals, state_ref_ty, span);
    let tag = poll_local(&mut locals, tag_ty, span);
    let mut local_map = FxHashMap::default();
    for decl in &original.locals {
        let local = poll_local(&mut locals, decl.ty, decl.span);
        locals[local.index()].name = decl.name;
        local_map.insert(decl.local, local);
    }
    let persistent = original.address_taken_locals();
    let result_pointer = poll_local(&mut locals, pointer_ty, span);
    let pending = poll_local(&mut locals, status_ty, span);
    let field = |name| Place {
        local: state,
        projections: vec![Projection::Deref, Projection::Field(name)],
    };
    let assign = |place, rvalue| Statement::Assign {
        place,
        rvalue,
        span,
    };
    let set_tag = |value| {
        assign(
            field(sym("__tag")),
            Rvalue::Cast {
                operand: Operand::Constant(Constant::Int(value)),
                target_ty: tag_ty,
            },
        )
    };
    let set_status = |value| {
        assign(
            Place::from_local(Local::RETURN_PLACE),
            Rvalue::Use(Operand::Constant(Constant::Bool(value))),
        )
    };
    let block_map: FxHashMap<_, _> = (0..original.blocks.len()).map(|i| (i, i + 2)).collect();
    let mut blocks = vec![
        BasicBlock {
            statements: vec![],
            terminator: Terminator::Unreachable,
            is_cleanup: false,
        },
        BasicBlock {
            statements: vec![],
            terminator: Terminator::Goto { target: BlockId(2) },
            is_cleanup: false,
        },
    ];
    for index in 1..=original.param_count {
        let name = sym(&format!("__local_{index}"));
        if !persistent.contains(&Local(index))
            && state_fields.iter().any(|(field, _)| *field == name)
        {
            blocks[1].statements.push(assign(
                Place::from_local(local_map[&Local(index)]),
                Rvalue::Use(Operand::Copy(field(name))),
            ));
        }
    }
    blocks.extend(
        original
            .blocks
            .iter()
            .map(|block| clone_block_with_remap(block, &local_map, &block_map, span)),
    );
    // Only original source returns commit the result. Suspension exits are added below.
    for (index, source) in original.blocks.iter().enumerate() {
        if matches!(source.terminator, Terminator::Return) {
            blocks[index + 2].statements.extend([
                assign(
                    field(sym("__result")),
                    Rvalue::Use(Operand::Move(Place::from_local(
                        local_map[&Local::RETURN_PLACE],
                    ))),
                ),
                set_tag(-1),
                set_status(true),
            ]);
        }
    }
    let mut dispatch = vec![(0, BlockId(1))];
    for (index, point) in await_points.iter().enumerate() {
        let poll_block = BlockId(blocks.len() as u32);
        let check_block = BlockId(poll_block.0 + 1);
        let pending_block = BlockId(poll_block.0 + 2);
        let ready_block = BlockId(poll_block.0 + 3);
        dispatch.push(((index + 1) as i128, poll_block));
        let mut saved: Vec<_> = live_across[index].iter().copied().collect();
        saved.push(point.future_local);
        saved.sort_unstable_by_key(|local| local.0);
        saved.dedup();
        saved.retain(|local| !persistent.contains(local));
        let source = &mut blocks[point.call_block_idx + 2];
        for local in &saved {
            source.statements.push(assign(
                field(sym(&format!("__local_{}", local.0))),
                Rvalue::Use(Operand::Copy(Place::from_local(local_map[local]))),
            ));
        }
        source.statements.push(assign(
            field(sym("__sub_future")),
            Rvalue::Use(Operand::Copy(Place::from_local(
                local_map[&point.future_local],
            ))),
        ));
        source.statements.push(set_tag(
            i64::try_from(index + 1).expect("MIR await count fits i64"),
        ));
        source.terminator = Terminator::Goto { target: poll_block };
        blocks.push(BasicBlock {
            statements: vec![],
            terminator: Terminator::Call {
                func: Operand::Constant(Constant::FunctionRef(try_poll)),
                args: vec![Operand::Copy(field(sym("__sub_future")))],
                destination: Place::from_local(result_pointer),
                target: check_block,
                span,
            },
            is_cleanup: false,
        });
        blocks.push(BasicBlock {
            statements: vec![assign(
                Place::from_local(pending),
                Rvalue::BinaryOp(
                    spark_ast::BinaryOp::Eq,
                    Operand::Copy(Place::from_local(result_pointer)),
                    Operand::Constant(Constant::Null),
                ),
            )],
            terminator: Terminator::SwitchInt {
                discriminant: Operand::Copy(Place::from_local(pending)),
                targets: vec![(1, pending_block)],
                otherwise: ready_block,
            },
            is_cleanup: false,
        });
        blocks.push(BasicBlock {
            statements: vec![set_status(false)],
            terminator: Terminator::Return,
            is_cleanup: false,
        });
        let mut ready = Vec::new();
        for local in saved {
            ready.push(assign(
                Place::from_local(local_map[&local]),
                Rvalue::Use(Operand::Copy(field(sym(&format!("__local_{}", local.0))))),
            ));
        }
        ready.push(assign(
            Place::from_local(local_map[&point.dest_local]),
            Rvalue::ReadFutureResult {
                pointer: result_pointer,
            },
        ));
        blocks.push(BasicBlock {
            statements: ready,
            terminator: Terminator::Goto {
                target: BlockId((point.resume_block.index() + 2) as u32),
            },
            is_cleanup: false,
        });
    }
    let completed = BlockId(blocks.len() as u32);
    blocks.push(BasicBlock {
        statements: vec![set_status(true)],
        terminator: Terminator::Return,
        is_cleanup: false,
    });
    blocks[0] = BasicBlock {
        statements: vec![assign(
            Place::from_local(tag),
            Rvalue::Use(Operand::Copy(field(sym("__tag")))),
        )],
        terminator: Terminator::SwitchInt {
            discriminant: Operand::Copy(Place::from_local(tag)),
            targets: dispatch,
            otherwise: completed,
        },
        is_cleanup: false,
    };
    let mut function = MirFunction {
        def_id: poll_def_id,
        param_count: 1,
        return_type: status_ty,
        locals,
        blocks,
        captures: vec![],
        is_async: false,
        span,
    };
    let roots = persistent
        .into_iter()
        .map(|local| (local_map[&local], sym(&format!("__local_{}", local.0))))
        .collect();
    promotion::promote(&mut function, &roots, table, diagnostics);
    function
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
fn remap_statement(stmt: &Statement, local_remap: &FxHashMap<Local, Local>) -> Statement {
    match stmt {
        Statement::Assign {
            place,
            rvalue,
            span,
        } => Statement::Assign {
            place: remap_place(place, local_remap),
            rvalue: remap_rvalue(rvalue, local_remap),
            span: *span,
        },
        Statement::IndexedAssign {
            root,
            indices,
            value,
            span,
        } => Statement::IndexedAssign {
            root: remap_place(root, local_remap),
            indices: indices
                .iter()
                .map(|local| *local_remap.get(local).unwrap_or(local))
                .collect(),
            value: *local_remap.get(value).unwrap_or(value),
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
        local: local_remap
            .get(&place.local)
            .copied()
            .unwrap_or(place.local),
        projections: place
            .projections
            .iter()
            .map(|p| remap_projection(p, local_remap))
            .collect(),
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
        Rvalue::ReadFutureResult { pointer } => Rvalue::ReadFutureResult {
            pointer: local_remap.get(pointer).copied().unwrap_or(*pointer),
        },
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
        Rvalue::StringInterp(ops) => {
            Rvalue::StringInterp(ops.iter().map(|o| remap_operand(o, local_remap)).collect())
        }
        Rvalue::ListRange {
            start,
            end,
            step,
            inclusive,
        } => Rvalue::ListRange {
            start: remap_operand(start, local_remap),
            end: remap_operand(end, local_remap),
            step: remap_operand(step, local_remap),
            inclusive: *inclusive,
        },
        Rvalue::RangeLen {
            start,
            end,
            step,
            inclusive,
        } => Rvalue::RangeLen {
            start: remap_operand(start, local_remap),
            end: remap_operand(end, local_remap),
            step: remap_operand(step, local_remap),
            inclusive: *inclusive,
        },
        Rvalue::CollectionGetOptional { collection, index } => Rvalue::CollectionGetOptional {
            collection: remap_place(collection, local_remap),
            index: remap_place(index, local_remap),
        },
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
        assert_eq!(poll_info.state_fields.last().unwrap().0, sym("__result"));
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
                matches!(
                    s,
                    Statement::Assign {
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Bool(false))),
                        ..
                    }
                )
            }) && matches!(b.terminator, Terminator::Return)
        });
        assert!(
            has_pending_return,
            "Expected a block that returns 0 (Pending)"
        );

        // At least one block should return 1 (Ready).
        let has_ready_return = poll_fn.blocks.iter().any(|b| {
            b.statements.iter().any(|s| {
                matches!(
                    s,
                    Statement::Assign {
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Bool(true))),
                        ..
                    }
                )
            }) && matches!(b.terminator, Terminator::Return)
        });
        assert!(has_ready_return, "Expected a block that returns 1 (Ready)");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
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
                    ty: future_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(2),
                    ty: void_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(3),
                    ty: i64_ty,
                    name: Some(sym("a")),
                    is_user_variable: true,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(4),
                    ty: future_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(5),
                    ty: void_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(6),
                    ty: i64_ty,
                    name: Some(sym("b")),
                    is_user_variable: true,
                    mutable: false,
                    span: Span::DUMMY,
                },
            ],
            blocks: vec![
                BasicBlock {
                    statements: vec![
                        Statement::Assign {
                            place: Place::from_local(Local(1)),
                            rvalue: Rvalue::Use(Operand::Constant(Constant::Int(0))),
                            span: Span::DUMMY,
                        },
                        Statement::Assign {
                            place: Place::from_local(Local(2)),
                            rvalue: Rvalue::Use(Operand::Constant(Constant::FunctionRef(
                                await_def,
                            ))),
                            span: Span::DUMMY,
                        },
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
                        Statement::Assign {
                            place: Place::from_local(Local(4)),
                            rvalue: Rvalue::Use(Operand::Constant(Constant::Int(0))),
                            span: Span::DUMMY,
                        },
                        Statement::Assign {
                            place: Place::from_local(Local(5)),
                            rvalue: Rvalue::Use(Operand::Constant(Constant::FunctionRef(
                                await_def,
                            ))),
                            span: Span::DUMMY,
                        },
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

        let mut mir = Mir {
            functions: HashMap::new(),
        };
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
                targets.len(),
                3,
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
        assert!(
            has_local_3_field,
            "Local(3) 'a' should be in state fields (live-across-await)"
        );
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
    // TAG [SPARK-117]
    fn poll_contract(divergent_branch: bool) -> (AsyncTransformOutput, DefId) {
        let mut table = TypeTable::new();
        let mut definitions = DefinitionTable::new();
        let mut builtin = |name| {
            definitions.insert(DefinitionInfo::new(
                DefinitionKind::BuiltinFunction,
                name,
                Span::DUMMY,
                Visibility::Private,
                spark_modules::ModulePath::root(),
            ))
        };
        let await_def = builtin(kw::SPARK_AWAIT);
        builtin(kw::TRY_POLL);
        let def = definitions.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            sym("poll_contract"),
            Span::DUMMY,
            Visibility::Private,
            spark_modules::ModulePath::root(),
        ));
        let mut function = make_simple_async_fn(def, await_def, &mut table);
        if divergent_branch {
            function.blocks[1].terminator = Terminator::SwitchInt {
                discriminant: Operand::Constant(Constant::Bool(true)),
                targets: vec![(1, BlockId(2))],
                otherwise: BlockId(3),
            };
            function.blocks.push(BasicBlock {
                statements: vec![],
                terminator: Terminator::Unreachable,
                is_cleanup: false,
            });
            function.blocks.push(BasicBlock {
                statements: vec![],
                terminator: Terminator::Return,
                is_cleanup: false,
            });
        }
        let mir = Mir {
            functions: HashMap::from([(def, function)]),
        };
        (
            transform(AsyncTransformInput {
                mir,
                table,
                definitions,
            }),
            def,
        )
    }

    #[test]
    fn poll_contract_uses_a_reference_to_persistent_state() {
        let (output, def) = poll_contract(false);
        let info = &output.poll_fn_map[&def];
        let poll = &output.mir.functions[&info.poll_def_id];
        assert!(matches!(output.table.resolve(poll.locals[1].ty),
            Ty::Ref { inner, mutable: true, .. } if *inner == info.state_type_id));
        assert!(matches!(output.table.resolve(poll.return_type), Ty::Bool));
    }

    #[test]
    fn poll_contract_pending_exit_does_not_publish_a_result() {
        let (output, def) = poll_contract(false);
        let poll = &output.mir.functions[&output.poll_fn_map[&def].poll_def_id];
        let pending: Vec<_> = poll.blocks.iter().filter(|block| block.statements.iter().any(|stmt|
            matches!(stmt, Statement::Assign { place, rvalue: Rvalue::Use(Operand::Constant(Constant::Bool(false) | Constant::Int(0))), .. }
                if place.local == Local::RETURN_PLACE && place.projections.is_empty()))).collect();
        assert!(!pending.is_empty(), "a pending exit must remain available");
        for block in pending {
            assert!(matches!(block.terminator, Terminator::Return));
            assert!(!block.statements.iter().any(|stmt| matches!(stmt,
                Statement::Assign { place, .. } if place.projections.iter().any(|projection| matches!(projection, Projection::Field(name) if *name == sym("__result"))))),
                "Pending cannot initialize or publish the completed result");
        }
    }

    #[test]
    fn poll_contract_preserves_divergent_control_flow() {
        let (output, def) = poll_contract(true);
        let poll = &output.mir.functions[&output.poll_fn_map[&def].poll_def_id];
        assert!(
            poll.blocks
                .iter()
                .any(|block| matches!(block.terminator, Terminator::Unreachable)),
            "an original divergent edge cannot become a Ready return"
        );
    }
    #[test]
    fn liveness_does_not_save_a_value_defined_by_a_later_call() {
        let mut table = TypeTable::new();
        let mut function = make_simple_async_fn(DefId::new(0), DefId::new(1), &mut table);
        function.blocks[1] = BasicBlock {
            statements: vec![],
            terminator: Terminator::Call {
                func: Operand::Constant(Constant::FunctionRef(DefId::new(2))),
                args: vec![],
                destination: Place::from_local(Local(1)),
                target: BlockId(2),
                span: Span::DUMMY,
            },
            is_cleanup: false,
        };
        function.blocks.push(BasicBlock {
            statements: vec![Statement::Assign {
                place: Place::from_local(Local(0)),
                rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(1)))),
                span: Span::DUMMY,
            }],
            terminator: Terminator::Return,
            is_cleanup: false,
        });
        let point = AwaitPoint {
            call_block_idx: 0,
            resume_block: BlockId(1),
            dest_local: Local(4),
            future_local: Local(2),
        };
        assert!(
            !compute_live_across_awaits(&function, &[point])[0].contains(&Local(1)),
            "a later call destination has no pre-suspend value to save"
        );
    }

    #[test]
    fn liveness_preserves_an_existing_return_value_across_await() {
        let mut table = TypeTable::new();
        let mut function = make_simple_async_fn(DefId::new(0), DefId::new(1), &mut table);
        function.blocks[0].statements.insert(
            0,
            Statement::Assign {
                place: Place::from_local(Local::RETURN_PLACE),
                rvalue: Rvalue::Use(Operand::Constant(Constant::Int(17))),
                span: Span::DUMMY,
            },
        );
        function.blocks[1].statements.clear();
        let point = AwaitPoint {
            call_block_idx: 0,
            resume_block: BlockId(1),
            dest_local: Local(4),
            future_local: Local(2),
        };
        assert!(compute_live_across_awaits(&function, &[point])[0].contains(&Local::RETURN_PLACE));
    }
    #[test]
    fn liveness_preserves_index_dependencies_of_projected_accesses() {
        for reference in [false, true] {
            let mut table = TypeTable::new();
            let mut function = make_simple_async_fn(DefId::new(0), DefId::new(1), &mut table);
            let integer = function.return_type;
            let collection = table.intern(Ty::List(integer));
            let reference_ty = table.intern(Ty::Ref {
                inner: integer,
                mutable: false,
                lifetime: spark_types::ty::LifetimeId::ANONYMOUS,
            });
            for ty in [collection, reference_ty] {
                poll_local(&mut function.locals, ty, Span::DUMMY);
            }
            function.blocks[0].statements.insert(
                0,
                Statement::Assign {
                    place: Place::from_local(Local(5)),
                    rvalue: Rvalue::Aggregate(
                        crate::mir::AggregateKind::List,
                        vec![Operand::Constant(Constant::Int(1))],
                    ),
                    span: Span::DUMMY,
                },
            );
            let indexed = Place {
                local: Local(5),
                projections: vec![Projection::Index(Local(1))],
            };
            let access = if reference {
                Statement::Assign {
                    place: Place::from_local(Local(6)),
                    rvalue: Rvalue::Ref {
                        place: indexed,
                        mutable: false,
                    },
                    span: Span::DUMMY,
                }
            } else {
                Statement::Drop {
                    place: indexed,
                    span: Span::DUMMY,
                }
            };
            function.blocks[1].statements.insert(0, access);
            let point = AwaitPoint {
                call_block_idx: 0,
                resume_block: BlockId(1),
                dest_local: Local(4),
                future_local: Local(2),
            };
            assert!(compute_live_across_awaits(&function, &[point])[0].contains(&Local(1)),
                "projected access must preserve its index across suspension (reference={reference})");
        }
    }
    // TAG [SPARK-154]
    #[test]
    fn references_across_await_are_created_in_persistent_frame_storage() {
        for parameter in [true, false] {
            let mut table = TypeTable::new();
            let mut definitions = DefinitionTable::new();
            let await_def = definitions.insert(DefinitionInfo::new(
                DefinitionKind::BuiltinFunction,
                kw::SPARK_AWAIT,
                Span::DUMMY,
                Visibility::Private,
                spark_modules::ModulePath::root(),
            ));
            let def = definitions.insert(DefinitionInfo::new(
                DefinitionKind::Function,
                sym("frame_reference"),
                Span::DUMMY,
                Visibility::Private,
                spark_modules::ModulePath::root(),
            ));
            let mut function = make_simple_async_fn(def, await_def, &mut table);
            let integer = function.return_type;
            let root = if parameter {
                Local(1)
            } else {
                let local = poll_local(&mut function.locals, integer, Span::DUMMY);
                function.blocks[0].statements.insert(
                    0,
                    Statement::Assign {
                        place: Place::from_local(local),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(10))),
                        span: Span::DUMMY,
                    },
                );
                local
            };
            let reference_ty = table.intern(Ty::Ref {
                inner: integer,
                mutable: false,
                lifetime: spark_types::ty::LifetimeId::ANONYMOUS,
            });
            let reference = poll_local(&mut function.locals, reference_ty, Span::DUMMY);
            let loaded = poll_local(&mut function.locals, integer, Span::DUMMY);
            function.blocks[0].statements.push(Statement::Assign {
                place: Place::from_local(reference),
                rvalue: Rvalue::Ref {
                    place: Place::from_local(root),
                    mutable: false,
                },
                span: Span::DUMMY,
            });
            function.blocks[1].statements = vec![
                Statement::Assign {
                    place: Place::from_local(loaded),
                    rvalue: Rvalue::Use(Operand::Copy(Place {
                        local: reference,
                        projections: vec![Projection::Deref],
                    })),
                    span: Span::DUMMY,
                },
                Statement::Assign {
                    place: Place::from_local(Local::RETURN_PLACE),
                    rvalue: Rvalue::BinaryOp(
                        spark_ast::BinaryOp::Add,
                        Operand::Copy(Place::from_local(loaded)),
                        Operand::Copy(Place::from_local(Local(4))),
                    ),
                    span: Span::DUMMY,
                },
            ];
            let output = transform(AsyncTransformInput {
                mir: Mir {
                    functions: HashMap::from([(def, function)]),
                },
                table,
                definitions,
            });
            let info = &output.poll_fn_map[&def];
            let field = sym(&format!("__local_{}", root.0));
            assert!(
                info.state_fields.iter().any(|(name, _)| *name == field),
                "the referent needs its own persistent backing field (parameter={parameter})"
            );
            let poll = &output.mir.functions[&info.poll_def_id];
            let references: Vec<_> = poll
                .blocks
                .iter()
                .flat_map(|block| &block.statements)
                .filter_map(|statement| match statement {
                    Statement::Assign {
                        rvalue: Rvalue::Ref { place, .. },
                        ..
                    } => Some(place),
                    _ => None,
                })
                .collect();
            assert_eq!(references.len(), 1);
            assert_eq!(
                references[0].local,
                Local(1),
                "the address must originate in the frame, not a poll stack slot"
            );
            assert!(
                matches!(references[0].projections.as_slice(), [Projection::Deref, Projection::Field(name)] if *name == field)
            );
        }
    }
}
