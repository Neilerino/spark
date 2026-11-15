// TAG [SPARK-114]

use super::*;
use crate::provenance::{
    channels::identity::Role as ChannelRole,
    tasks::{lifecycle::Coverage, Role},
};
use spark_mir::Place;

fn send_call(flow: &Flow<'_>, child: Local) -> (Operand, Vec<Operand>, Place, Point) {
    flow.function.blocks.iter().enumerate().find_map(|(index, block)| {
        let Terminator::Call { func, args, destination, .. } = &block.terminator else { return None };
        matches!(args.get(1), Some(Operand::Copy(place) | Operand::Move(place)) if place.local == child)
            .then(|| (func.clone(), args.clone(), destination.clone(), Point::new(BlockId(index as u32), block.statements.len() as u32)))
    }).unwrap()
}

fn send(flow: &mut Flow<'_>, state: &mut State, call: &(Operand, Vec<Operand>, Place, Point)) {
    let (func, args, destination, point) = call;
    flow.transfers.remove(point);
    flow.ensure_transfer(*point, state);
    let dispatch = flow.call_result(func, args, flow.place_type(destination), *point, state);
    assert_eq!(dispatch.returning.len(), 1);
    flow.apply_call_outcomes(args, destination, dispatch.returning, *point, state);
}

#[test]
fn actual_send_transports_a_conditional_copy_without_preserving_closed_rights() {
    with_fresh_producer(|flow, before, child, birth| {
        let original = send_call(flow, child);
        let (Operand::Copy(receiver) | Operand::Move(receiver)) = &original.1[0] else {
            unreachable!()
        };
        let receiver = receiver.local;
        let premise = before
            .channel_copies
            .witness(&ChannelRole::Local(receiver))
            .input
            .unwrap();
        for moved in [false, true] {
            let mut call = original.clone();
            call.1[1] = if moved {
                Operand::Move(Place::from_local(child))
            } else {
                Operand::Copy(Place::from_local(child))
            };
            let mut state = before.clone();
            assert!(state.future_owners.known(child));
            send(flow, &mut state, &call);
            let slot = state.queue_current.keys().next().unwrap().clone();
            let role = Role::message(slot.clone());
            assert_eq!(
                state.future_owners.coverage(&role),
                Some(Coverage::ThroughQueue(premise.clone()))
            );
            assert!(!state.future_owners.known_role(&role));
            assert!(!state.future_owners.known(child));
            assert_eq!(state.live.contains(&child), !moved);
            assert_eq!(state.future_copies.role_witness(&role).birth, Some(birth));
            assert_eq!(
                state
                    .future_copies
                    .role_peers(&role)
                    .contains(&Role::root(child)),
                !moved
            );
            assert!(!state.queue_current[&slot].value.origins().is_empty());
            let mut missing = state.clone();
            missing.queue_current.remove(&slot);
            missing.future_copies.forget_role(&role);
            missing.future_owners.forget_role(&role);
            assert!(state.join(&missing));
            assert!(state.queue_current.contains_key(&slot));
            assert!(state.future_owners.coverage(&role).is_none());
        }
        let mut repeated = before.clone();
        let mut copy_call = original.clone();
        copy_call.1[1] = Operand::Copy(Place::from_local(child));
        send(flow, &mut repeated, &copy_call);
        let slot = repeated.queue_current.keys().next().unwrap().clone();
        let message = Role::message(slot.clone());
        assert!(repeated.future_owners.coverage(&message).is_some());
        send(flow, &mut repeated, &copy_call);
        assert!(!repeated.queues.is_empty());
        assert!(repeated
            .queues
            .values()
            .any(|value| !value.origins().is_empty()));
        assert!(repeated.future_owners.coverage(&message).is_none());
        assert!(
            !repeated
                .future_copies
                .role_peers(&message)
                .contains(&Role::root(child)),
            "old-slot archival cannot leave a stale copy bridge"
        );

        let mut moved_receiver = original.clone();
        moved_receiver.1[0] = Operand::Move(Place::from_local(receiver));
        let mut state = before.clone();
        send(flow, &mut state, &moved_receiver);
        assert!(state.queue_current.is_empty());
        assert!(
            !state.queues.is_empty(),
            "failed final reservation preserves weak activity"
        );
        assert!(!state.future_owners.known(child));

        let mut forged = before.clone();
        forged.channel_copies.forget(receiver);
        assert_eq!(forged.value(receiver), before.value(receiver));
        send(flow, &mut forged, &original);
        assert!(
            forged.queue_current.keys().all(|slot| forged
                .future_owners
                .coverage(&Role::message(slot.clone()))
                .is_none()),
            "a static input Target without entry-object lineage grants no conditional theorem"
        );

        let mut alternative = before.clone();
        let role = ChannelRole::Local(receiver);
        let mut witness = alternative.channel_copies.witness(&role);
        witness.input = None;
        alternative
            .channel_copies
            .assign_role(role.clone(), witness, &alternative.live);
        assert_eq!(alternative.values, before.values);
        let mut joined = before;
        assert!(
            joined.join(&alternative),
            "entry-lineage-only disagreement must wake Flow"
        );
        assert!(joined.channel_copies.witness(&role).input.is_none());
    });
}

#[test]
fn conditional_coverage_meets_and_exposure_never_promote_it_to_closed() {
    with_fresh_producer(|flow, mut state, child, _| {
        let call = send_call(flow, child);
        send(flow, &mut state, &call);
        let slot = state.queue_current.keys().next().unwrap().clone();
        let message = Role::message(slot);
        let condition = state.future_owners.coverage(&message).unwrap();
        assert!(matches!(condition, Coverage::ThroughQueue(_)));
        let other_queue = flow
            .function
            .locals
            .iter()
            .find(|decl| decl.name.is_some_and(|name| name.as_str() == "stop"))
            .unwrap()
            .local;
        let other_premise = flow
            .initial_state()
            .channel_copies
            .witness(&ChannelRole::Local(other_queue))
            .input
            .unwrap();
        assert!(condition
            .clone()
            .meet(Coverage::ThroughQueue(other_premise))
            .is_none());
        assert_eq!(
            condition.clone().meet(condition.clone()),
            Some(condition.clone())
        );
        assert_eq!(
            Coverage::Closed.meet(condition.clone()),
            Some(condition.clone())
        );
        let mut changed = state.clone();
        changed
            .future_owners
            .grant_role(message.clone(), &changed.future_copies);
        assert!(state.activity_basis() != changed.activity_basis());
        assert!(changed.join(&state));
        assert_eq!(changed.future_owners.coverage(&message), Some(condition));
        assert!(!changed.future_owners.known_role(&message));
        Flow::lose_owning_operand(&Operand::Copy(Place::from_local(child)), &mut changed);
        assert!(changed.future_owners.coverage(&message).is_none());
        changed.consume(child);
        assert!(changed.future_owners.coverage(&message).is_none());
    });
}
