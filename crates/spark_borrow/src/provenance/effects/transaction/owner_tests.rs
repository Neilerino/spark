// TAG [SPARK-114]

use super::*;
use crate::provenance::{tasks, value::Value};
use rustc_hash::FxHashMap;
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
        fresh_leaf_task: false,
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
    let proof = outcome(&state, 0);
    flow.ensure_transfer(point, &state);
    flow.consume_call_arguments(&[copied()], &proof, point, &mut state);
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
    let proof = outcome(&state, 0);
    flow.ensure_transfer(point, &state);
    flow.consume_call_arguments(&[moved()], &proof, point, &mut state);
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
        let proof = outcome(&state, released);
        flow.ensure_transfer(point, &state);
        flow.consume_call_arguments(&args, &proof, point, &mut state);
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
    let proof = outcome(&state, 0);
    flow.ensure_transfer(point, &state);
    flow.consume_call_arguments(&[moved()], &proof, point, &mut state);
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
    flow.consume_call_arguments(&[moved()], &proof, point, &mut state);
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
        &proof,
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
    let proof = outcome(&state, 0);
    flow.consume_call_arguments(&[moved()], &proof, point, &mut state);
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
