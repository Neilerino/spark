// TAG [SPARK-114]

use super::*;
use crate::provenance::{tasks, tasks::proofs::CallProof, value::Value};
use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::{BlockId, LocalDecl};
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::{IntWidth, Ty, TypeTable};

fn fixture() -> (TypeTable, MirFunction) {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let function = MirFunction {
        def_id: DefId::new(9965),
        param_count: 1,
        return_type: int,
        locals: [int, future, future]
            .into_iter()
            .enumerate()
            .map(|(index, ty)| LocalDecl {
                local: Local(index as u32),
                ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            })
            .collect(),
        blocks: vec![],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    (table, function)
}

fn pending(flow: &Flow<'_>, point: Point) -> State {
    let mut state = flow.initial_state();
    let (_, task) = tasks::birth(flow.function.def_id, point, flow.function.locals[1].ty);
    state.set(Local(1), task);
    state.future_owners.grant(Local(1), &state.future_copies);
    state
}

fn outcome(state: &State, released_index: usize) -> CallOutcome {
    CallOutcome {
        returned: Value::default(),
        tasks: vec![],
        completed_task: None,
        clobbered_copies: vec![],
        future_proof: CallProof {
            released_owners: FxHashMap::from_iter([(
                released_index,
                state.future_copies.witness(Local(1)),
            )]),
            ..CallProof::default()
        },
        returned_prefix: Default::default(),
        publications: Default::default(),
        place_writes: vec![],
        memory: vec![],
        async_memory: vec![],
        actual_definitions: Vec::new(),
        ordered_definitions: Vec::new(),
        fresh_outputs: Vec::new(),
        spawn_outputs: Vec::new(),
        producer_events: Vec::new(),
        queue_accesses: crate::provenance::channels::access::Accesses::default(),
        queue_send_counts: crate::provenance::channels::counts::Sends::default(),
        receipt: None,
        direct_queue_send: None,
        spawned_memory: None,
        spawn_return: None,
        fresh_leaf_task: false,
        spawn_owners: None,
        returned_owner_tags: crate::provenance::tasks::lifecycle::Tags::default(),
        retained_captures: Vec::new(),
        returned_callbacks: crate::provenance::tasks::outputs::CallbackOutputs::default(),
        residuals: vec![],
        prefix_memory: vec![],
        prefix_residuals: vec![],
        queues: vec![],
        queue_contents: vec![],
    }
}

fn copied() -> Operand {
    Operand::Copy(Place::from_local(Local(1)))
}
fn moved() -> Operand {
    Operand::Move(Place::from_local(Local(1)))
}

#[test]
fn releasing_a_call_copy_preserves_the_original_pending_owner() {
    let (table, function) = fixture();
    let mut flow = Flow::new(&function, &table, None);
    let point = Point::new(BlockId::ENTRY, 1);
    let mut state = pending(&flow, point);
    let mut proof = outcome(&state, 0);
    flow.ensure_transfer(point, &state);
    flow.consume_call_arguments(&[copied()], &mut proof, &[], point, &mut state);
    assert!(state.future_owners.known(Local(1)));
    assert!(state.live.contains(&Local(1)));
    assert!(state.joined_inputs.is_empty());
    assert!(state
        .value(Local(1))
        .futures
        .values()
        .flatten()
        .all(|handle| handle.pending));
    assert!(!state.input_owners.released_provided(Local(1)));
}

#[test]
fn releasing_the_last_covered_moved_owner_joins_it() {
    let (table, function) = fixture();
    let mut flow = Flow::new(&function, &table, None);
    let point = Point::new(BlockId::ENTRY, 1);
    let mut state = pending(&flow, point);
    let mut proof = outcome(&state, 0);
    flow.ensure_transfer(point, &state);
    flow.consume_call_arguments(&[moved()], &mut proof, &[], point, &mut state);
    assert!(!state.live.contains(&Local(1)));
    assert!(state.joined_inputs.contains(&Local(1)));
    assert!(state.input_owners.released_provided(Local(1)));
    assert!(state.input_owners.retains_no_owner(Local(1)));
}

#[test]
fn aliased_argument_escape_is_recorded_before_any_certified_release() {
    for args in [vec![moved(), copied()], vec![copied(), moved()]] {
        let (table, function) = fixture();
        let mut flow = Flow::new(&function, &table, None);
        let point = Point::new(BlockId::ENTRY, 1);
        let mut state = pending(&flow, point);
        let released = usize::from(matches!(args[0], Operand::Copy(_)));
        let mut proof = outcome(&state, released);
        flow.ensure_transfer(point, &state);
        flow.consume_call_arguments(&args, &mut proof, &[], point, &mut state);
        assert!(
            state.joined_inputs.is_empty(),
            "a retained aliased argument prevents final cleanup"
        );
        assert!(state.input_owners.released_provided(Local(1)));
        assert!(
            !state.input_owners.retains_no_owner(Local(1)),
            "the uncertified Copy may escape"
        );
    }
}

#[test]
fn certified_release_preserves_an_older_escaped_copy() {
    let (table, function) = fixture();
    let mut flow = Flow::new(&function, &table, None);
    let point = Point::new(BlockId::ENTRY, 1);
    let mut state = pending(&flow, point);
    flow.escape_input_owner_operand(&copied(), &mut state);
    Flow::lose_owning_operand(&copied(), &mut state);
    let mut proof = outcome(&state, 0);
    flow.ensure_transfer(point, &state);
    flow.consume_call_arguments(&[moved()], &mut proof, &[], point, &mut state);
    assert!(state.joined_inputs.is_empty());
    assert!(state.input_owners.released_provided(Local(1)));
    assert!(!state.input_owners.retains_no_owner(Local(1)));
}

#[test]
fn a_clobbered_owner_witness_cannot_discharge_the_argument() {
    let (table, function) = fixture();
    let mut flow = Flow::new(&function, &table, None);
    let point = Point::new(BlockId::ENTRY, 1);
    let mut state = pending(&flow, point);
    let mut proof = outcome(&state, 0);
    proof.clobbered_copies.push(Local(1));
    flow.ensure_transfer(point, &state);
    flow.consume_call_arguments(&[moved()], &mut proof, &[], point, &mut state);
    assert!(state.joined_inputs.is_empty());
    assert!(!state.input_owners.released_provided(Local(1)));
}

#[test]
fn releasing_two_aliased_move_roles_joins_only_after_both_are_consumed() {
    let (table, function) = fixture();
    let mut flow = Flow::new(&function, &table, None);
    let point = Point::new(BlockId::ENTRY, 1);
    let mut state = pending(&flow, point);
    flow.ensure_transfer(point, &state);
    flow.transfer_statement(
        &spark_mir::Statement::Assign {
            place: Place::from_local(Local(2)),
            rvalue: spark_mir::Rvalue::Use(copied()),
            span: Span::DUMMY,
        },
        point,
        &mut state,
    );
    let mut proof = outcome(&state, 0);
    proof
        .future_proof
        .released_owners
        .insert(1, state.future_copies.witness(Local(2)));
    flow.consume_call_arguments(
        &[moved(), Operand::Move(Place::from_local(Local(2)))],
        &mut proof,
        &[],
        point,
        &mut state,
    );
    assert!(state.joined_inputs.contains(&Local(1)));
    assert!(!state.live.contains(&Local(1)));
    assert!(!state.live.contains(&Local(2)));
    assert!(state.input_owners.retains_no_owner(Local(1)));
}

#[test]
fn releasing_one_of_two_covered_owners_keeps_the_other_pending() {
    let (table, function) = fixture();
    let mut flow = Flow::new(&function, &table, None);
    let point = Point::new(BlockId::ENTRY, 1);
    let mut state = pending(&flow, point);
    flow.ensure_transfer(point, &state);
    flow.transfer_statement(
        &spark_mir::Statement::Assign {
            place: Place::from_local(Local(2)),
            rvalue: spark_mir::Rvalue::Use(copied()),
            span: Span::DUMMY,
        },
        point,
        &mut state,
    );
    let mut proof = outcome(&state, 0);
    flow.consume_call_arguments(&[moved()], &mut proof, &[], point, &mut state);
    assert!(state.joined_inputs.is_empty());
    assert!(state.future_owners.known(Local(2)));
    assert!(state
        .value(Local(2))
        .futures
        .values()
        .flatten()
        .all(|handle| handle.pending));
    assert!(state.input_owners.released_provided(Local(1)));
    assert!(!state.input_owners.retains_no_owner(Local(1)));
}

#[test]
fn a_returned_capture_reserves_ownership_across_all_released_alias_arguments() {
    use crate::provenance::{
        tasks::{lifecycle::Token, outputs::CallbackRecipe},
        Slot,
    };
    for retained_index in [0, 1] {
        let (mut table, function) = fixture();
        let callback_ty = table.intern(Ty::Function {
            params: vec![],
            ret: function.return_type,
            is_async: false,
        });
        let mut flow = Flow::new(&function, &table, None);
        let point = Point::new(BlockId::ENTRY, 1);
        let mut state = pending(&flow, point);
        flow.ensure_transfer(point, &state);
        flow.transfer_statement(
            &spark_mir::Statement::Assign {
                place: Place::from_local(Local(2)),
                rvalue: spark_mir::Rvalue::Use(copied()),
                span: Span::DUMMY,
            },
            point,
            &mut state,
        );
        let args = if retained_index == 0 {
            vec![moved(), Operand::Move(Place::from_local(Local(2)))]
        } else {
            vec![Operand::Move(Place::from_local(Local(2))), moved()]
        };
        let mut proof = outcome(&state, 1 - retained_index);
        let callback = DefId::new(9968);
        let recipe = CallbackRecipe {
            parent_ty: function.locals[1].ty,
            callback_ty,
            callback,
            slots: FxHashMap::from_iter([(
                vec![Slot::Capture(callback, 0, function.locals[1].ty)],
                Token::Derived(Local(retained_index as u32 + 1)),
            )]),
        };
        proof.future_proof.output_owners = flow.bind_output_recipe(&recipe, &args, &state);
        assert!(proof.future_proof.output_owners.is_some());
        flow.consume_call_arguments(&args, &mut proof, &[], point, &mut state);
        assert!(
            state.joined_inputs.is_empty(),
            "the returned parent retains an owner in either argument order"
        );
    }
}

#[test]
fn a_forwarded_root_reserves_the_parent_across_all_released_alias_arguments() {
    for retained_index in [0, 1] {
        let (table, mut function) = fixture();
        function.return_type = function.locals[1].ty;
        function.locals[0].ty = function.return_type;
        let mut flow = Flow::new(&function, &table, None);
        let point = Point::new(BlockId::ENTRY, 1);
        let mut state = pending(&flow, point);
        flow.ensure_transfer(point, &state);
        flow.transfer_statement(
            &spark_mir::Statement::Assign {
                place: Place::from_local(Local(2)),
                rvalue: spark_mir::Rvalue::Use(copied()),
                span: Span::DUMMY,
            },
            point,
            &mut state,
        );
        let args = if retained_index == 0 {
            vec![moved(), Operand::Move(Place::from_local(Local(2)))]
        } else {
            vec![Operand::Move(Place::from_local(Local(2))), moved()]
        };
        let mut call = outcome(&state, 1 - retained_index);
        call.future_proof.returned = Some(state.future_copies.witness(Local(1)));
        call.future_proof
            .forwarded_provided
            .insert(retained_index, state.future_copies.witness(Local(1)));
        call.prepare_owner_transfers(
            &mut flow,
            &args,
            &Place::from_local(Local(0)),
            point,
            &mut state,
        );
        assert!(call.future_proof.forwarded.is_some());
        flow.consume_call_arguments(&args, &mut call, &[], point, &mut state);
        assert!(
            state.joined_inputs.is_empty(),
            "forwarded parent is not a final release in either order"
        );
    }
}

#[test]
fn returned_callback_imports_cannot_restore_invalidated_identity_or_owner_coverage() {
    use crate::provenance::{
        address::OpaqueSite,
        demand::Storage,
        observations::{FrameScope, Memory, Sample},
        tasks::{
            lifecycle::Token,
            memory::Lane,
            outputs::{CallbackOutput, CallbackRecipe},
            views::{Subject, View},
            Role,
        },
        Slot,
    };
    for target in [Some(Local(1)), Some(Local(2)), None, Some(Local(3))] {
        let (mut table, mut function) = fixture();
        let future = function.locals[1].ty;
        function.locals.push(LocalDecl {
            local: Local(3),
            ty: future,
            name: None,
            is_user_variable: false,
            mutable: false,
            span: Span::DUMMY,
        });
        let callback_ty = table.intern(Ty::Function {
            params: Vec::new(),
            ret: function.return_type,
            is_async: false,
        });
        let mut flow = Flow::new(&function, &table, None);
        let point = Point::new(BlockId::ENTRY, 100);
        let mut state = pending(&flow, point);
        state.set(
            Local(3),
            tasks::birth(function.def_id, Point::new(BlockId::ENTRY, 99), future).1,
        );
        let callback_def = DefId::new(9969);
        let path = vec![Slot::Capture(callback_def, 0, future)];
        let recipe = CallbackRecipe {
            parent_ty: future,
            callback_ty,
            callback: callback_def,
            slots: FxHashMap::from_iter([(path.clone(), Token::Derived(Local(1)))]),
        };
        let bound = flow
            .bind_output_recipe(&recipe, &[copied()], &state)
            .unwrap();
        let payload = state.value(Local(1)).sourced(&Storage::Logical(Local(1)));
        let mut callback = Value::function(callback_def);
        callback.join(&payload.prefixed_path(&path, callback_ty, &table));
        let actor = Subject::from(callback.futures[&path].iter().next().unwrap());
        let site = OpaqueSite {
            function: function.def_id,
            point,
        };
        let memory = target.map_or(Memory::Unavailable(site), |local| Memory::Frame {
            scope: FrameScope::Current,
            function: function.def_id,
            local,
            ty: future,
        });
        let mut view = View::unknown();
        view.memory.import(
            std::sync::Arc::new(Lane {
                execution: crate::provenance::effects::Execution::Current,
                event: site,
                actor: actor.clone(),
                location: Sample {
                    memory,
                    path: Vec::new(),
                    ty: future,
                },
            }),
            tasks::birth(function.def_id, Point::new(BlockId::ENTRY, 150), future).1,
        );
        callback
            .task_views
            .entry(path.clone())
            .or_default()
            .insert(actor, view);
        state.set(Local(2), tasks::birth(function.def_id, point, future).1);
        flow.ensure_transfer(point, &state);
        flow.install_callback_outputs(
            FxHashMap::from_iter([(
                callback_ty,
                CallbackOutput {
                    callback,
                    lineage: FxHashMap::from_iter([(
                        path,
                        FxHashSet::from_iter([Token::Derived(Local(1))]),
                    )]),
                },
            )]),
            &Place::from_local(Local(2)),
            point,
            &mut state,
        );
        assert!(state
            .invalidated_copies
            .contains(&target.unwrap_or(Local(2))));
        bound.install(Local(2), &mut state);
        let role = Role::capture(Local(2), callback_def, 0, future)
            .in_execution(Local(2), callback_ty)
            .unwrap();
        let valid = target == Some(Local(3));
        assert_eq!(
            state.future_owners.known_role(&role),
            valid,
            "only an unrelated imported write may preserve captured owner coverage"
        );
        assert_eq!(
            state.future_owners.known(Local(1)),
            valid,
            "an untracked returned capture must revoke source owner closure"
        );
        assert_eq!(
            state.future_copies.witness(Local(1)).peers.contains(&role),
            valid,
            "invalidated entry witnesses cannot be reconnected"
        );
        flow.transfer_statement(
            &spark_mir::Statement::Nop,
            Point::new(BlockId::ENTRY, 101),
            &mut state,
        );
        assert_eq!(
            state.current_execution_record(Local(2)).is_some(),
            valid,
            "MUST backing may survive only unrelated import-time invalidation"
        );
    }
}

mod settled_memory;
