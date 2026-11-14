// TAG [SPARK-114]

use super::*;
use crate::provenance::{
    effects::inference::test_support::{infer_summaries, lower_source},
    observations::{FrameScope, Memory},
    Slot,
};

#[test]
fn a_released_leaf_move_uses_its_settled_batch_instead_of_querying_again() {
    leaf_move_case(true);
}

#[test]
fn an_unselected_leaf_release_still_discovers_its_live_completion() {
    leaf_move_case(false);
}

fn leaf_move_case(selected: bool) {
    let source = r#"
struct Text { value: Ref[String] }
struct Bucket { values: List[Ref[String]]! }
struct Target { bucket: Ref[Bucket]! }
struct Holder { target: Ref[Target]! }
async fn install(text: Text, holder: Holder): Int {
    target: Ref[Target]! = holder.target
    bucket: Ref[Bucket]! = target.bucket
    values: List[Ref[String]] = [text.value]
    bucket.values = values
    return 0
}
fn join(task: Future[Int]): Int { return await task }
fn main(): Int {
    text: String = " borrowed ".trim()
    borrowed: Ref[String] = text
    shared: Text = Text { value: borrowed }
    empty: List[Ref[String]] = []
    bucket: Bucket! = Bucket { values: empty }
    bucketRef: Ref[Bucket]! = bucket
    target: Target! = Target { bucket: bucketRef }
    targetRef: Ref[Target]! = target
    holder: Holder = Holder { target: targetRef }
    otherBucket: Bucket! = Bucket { values: empty }
    otherRef: Ref[Bucket]! = otherBucket
    result: Int = join(spawn { install(shared, holder) })
    return result
}
"#;
    let source = if selected {
        source.to_owned()
    } else {
        source.replace("return await task", "return 0")
    };
    let output = lower_source(&source);
    assert!(!output.diagnostics.has_errors());
    let facts = output
        .mir
        .functions
        .iter()
        .map(|(id, function)| (*id, crate::facts::extract_facts(function)))
        .collect();
    let (summaries, ports, proofs) = infer_summaries(&output, &facts);
    let context = crate::provenance::effects::Context {
        mir: &output.mir,
        summaries: &summaries,
        ports: &ports,
        definitions: Some(&output.definitions),
        proofs: Some(&proofs),
    };
    let function = output
        .mir
        .functions
        .values()
        .find(|function| {
            output
                .definitions
                .get(function.def_id)
                .unwrap()
                .name
                .as_str()
                == "main"
        })
        .unwrap();
    let (index, body) = function
        .blocks
        .iter()
        .enumerate()
        .find(|(_, body)| {
            let spark_mir::Terminator::Call { func, .. } = &body.terminator else {
                return false;
            };
            function
                .resolve_callee_def_id(func)
                .and_then(|def| output.definitions.get(def))
                .is_some_and(|def| def.name.as_str() == "join")
        })
        .unwrap();
    let spark_mir::Terminator::Call {
        func,
        args,
        destination,
        ..
    } = &body.terminator
    else {
        unreachable!()
    };
    let Operand::Move(place) = &args[0] else {
        panic!("anonymous Future must move")
    };
    let mut flow = Flow::new(function, &output.table, Some(context));
    flow.solve(&facts[&function.def_id]);
    let mut state = flow.entries[index].clone().unwrap();
    for (offset, statement) in body.statements.iter().enumerate() {
        assert!(flow.transfer_statement(
            statement,
            Point::new(BlockId(index as u32), offset as u32),
            &mut state
        ));
    }
    let point = Point::new(BlockId(index as u32), body.statements.len() as u32);
    flow.transfers.remove(&point);
    flow.ensure_transfer(point, &state);
    assert!(flow.current_leaf_birth(place.local, &state).is_some());
    assert!(state.current_execution_record(place.local).is_none());
    assert_eq!(state.future_copies.witness(place.local).peers.len(), 1);
    assert!(state.future_owners.known(place.local));
    let mut dispatch = flow.call_result(
        func,
        args,
        function.locals[destination.local.index()].ty,
        point,
        &state,
    );
    assert!(dispatch.diverging.is_empty());
    assert_eq!(dispatch.returning.len(), 1);
    let mut outcome = dispatch.returning.pop().unwrap();
    assert_eq!(
        outcome
            .future_proof
            .completed
            .iter()
            .any(|witness| witness.contains_local(place.local)),
        selected
    );
    assert!(outcome.future_proof.released_owners[&0].contains_local(place.local));
    outcome.freeze_sources(&flow, point);
    outcome.apply_queue_accesses(&mut flow, point, &mut state);
    outcome.apply_prefix(&mut flow, point, &mut state);
    let requests = outcome.completions(&state);
    let prepared = flow.prepare_task_memory(requests, &outcome.clobbered_copies, point, &mut state);
    let settled = flow.commit_task_memory(prepared, point, &mut state);
    assert_eq!(
        settled
            .iter()
            .any(|completion| completion.contains_local(&flow, point, place.local)),
        selected
    );
    outcome.prepare_owner_transfers(&mut flow, args, destination, point, &mut state);
    let named = |name: &str| {
        function
            .locals
            .iter()
            .find(|local| local.name.is_some_and(|symbol| symbol.as_str() == name))
            .unwrap()
            .local
    };
    let original = named("bucket");
    let other = named("otherBucket");
    let target = named("targetRef");
    let other_ref = named("otherRef");
    let target_value = flow.read(&Place::from_local(target), &state);
    let target_ty = crate::provenance::value::child_type(
        function.locals[target.index()].ty,
        &Slot::Deref,
        &output.table,
    )
    .unwrap();
    let metadata = flow.project_reference(
        &target_value,
        target_ty,
        &vec![Slot::Field(spark_intern::sym("bucket"))],
    );
    let replacement = state
        .value(other_ref)
        .sourced(&crate::provenance::demand::Storage::Logical(other_ref));
    flow.store_references(
        &metadata,
        &replacement,
        function.locals[other_ref.index()].ty,
        point,
        &mut state,
    );
    flow.consume_call_arguments(args, &mut outcome, &settled, point, &mut state);
    assert!(!state.live.contains(&place.local));
    for completion in settled {
        completion.finish(&mut flow, None, point, &mut state);
    }
    let at = |memory: &Memory, local| {
        matches!(memory,
        Memory::Frame { scope: FrameScope::Current, local: actual, .. } if *actual == local)
    };
    assert!(state
        .settled_memory
        .keys()
        .any(|location| at(&location.memory, original)));
    assert!(
        state
            .settled_memory
            .keys()
            .any(|location| at(&location.memory, other))
            == !selected,
        "released Move must reuse selected settlement and discover unselected cleanup"
    );
}
