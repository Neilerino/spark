// TAG [SPARK-114]

use super::*;
use crate::provenance::{
    address::{Dependency, OpaqueSite},
    Point, Slot,
};
use spark_mir::{BlockId, Local};

#[test]
fn receiving_an_older_receipt_does_not_extend_its_snapshot() {
    let output = lower_source(
        "fn sample(input: Channel[Ref[String]], relay: Channel[Ref[String]]): Void {}",
    );
    assert!(!output.diagnostics.has_errors());
    let function = output.mir.functions.values().next().unwrap();
    let mut flow = Flow::new(function, &output.table, None);
    let mut state = flow.initial_state();
    let reference =
        crate::provenance::child_type(function.locals[1].ty, &Slot::Element, &output.table)
            .unwrap();
    let first = flow.receive_payload(
        &state.value(Local(1)),
        reference,
        Point::new(BlockId::ENTRY, 0),
        &state,
    );
    let receipt = first
        .leaves
        .values()
        .flat_map(|leaf| &leaf.dependencies)
        .find_map(|dependency| match dependency {
            Dependency::InputValue(_, Some(receive)) => Some(receive.clone()),
            _ => None,
        })
        .unwrap();
    assert!(flow.receive_snapshots[&receipt].is_empty());

    let relay = state.value(Local(2));
    let target = relay.root_queue_handles().next().unwrap().target.clone();
    state
        .queues
        .insert((target.clone(), reference), first.clone());
    let event = OpaqueSite {
        function: function.def_id,
        point: Point::new(BlockId::ENTRY, 1),
    };
    state
        .publications
        .queues
        .insert((event, target, reference), first);
    let received = flow.receive_payload(&relay, reference, Point::new(BlockId::ENTRY, 2), &state);
    assert!(!received.is_empty());
    assert_eq!(flow.receive_snapshots.len(), 2);
    assert!(flow.receive_snapshots[&receipt].is_empty());
    assert_eq!(
        flow.receive_snapshots
            .values()
            .filter(|snapshot| !snapshot.is_empty())
            .count(),
        1
    );
}

#[test]
fn one_original_receive_site_keeps_distinct_queue_bindings_separate() {
    let output = lower_source(
        "fn sample(first: Channel[Ref[String]], second: Channel[Ref[String]]): Void {}",
    );
    let function = output.mir.functions.values().next().unwrap();
    let mut flow = Flow::new(function, &output.table, None);
    let mut state = flow.initial_state();
    let reference =
        crate::provenance::child_type(function.locals[1].ty, &Slot::Element, &output.table)
            .unwrap();
    let point = Point::new(BlockId::ENTRY, 0);
    let first = flow.receive_payload(&state.value(Local(1)), reference, point, &state);
    let receipt = first
        .leaves
        .values()
        .flat_map(|leaf| &leaf.dependencies)
        .find_map(|dependency| match dependency {
            Dependency::InputValue(_, Some(receive)) => Some(receive.clone()),
            _ => None,
        })
        .unwrap();
    let second = state.value(Local(2));
    let target = second.root_queue_handles().next().unwrap().target.clone();
    state.publications.queues.insert(
        (
            OpaqueSite {
                function: function.def_id,
                point,
            },
            target,
            reference,
        ),
        first,
    );
    flow.receive_payload(&second, reference, point, &state);
    assert_eq!(flow.receive_snapshots.len(), 2);
    assert!(flow.receive_snapshots[&receipt].is_empty());
    assert!(flow
        .receive_snapshots
        .keys()
        .all(|key| key.site == receipt.site));
}
