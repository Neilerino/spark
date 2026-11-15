// TAG [SPARK-114]

use super::*;
use crate::provenance::{
    channels::{
        identity::Role as ChannelRole,
        messages::{Contribution, Slot},
    },
    demand::Storage,
    tasks::{Live, Role},
    Value,
};
use spark_mir::Place;

#[test]
fn message_roles_use_typed_backing_without_treating_queue_coordinates_as_locals() {
    with_fresh_producer(|flow, before, child, birth| {
        let (index, block) = flow.function.blocks.iter().enumerate().find(|(_, block)| matches!(&block.terminator,
            Terminator::Call { args, .. } if matches!(args.get(1), Some(Operand::Copy(place) | Operand::Move(place)) if place.local == child))).unwrap();
        let Terminator::Call { args, .. } = &block.terminator else {
            unreachable!()
        };
        let (Operand::Copy(receiver) | Operand::Move(receiver)) = &args[0] else {
            unreachable!()
        };
        let receiver = receiver.local;
        let point = Point::new(BlockId(index as u32), block.statements.len() as u32);
        check_root_cell_sources(flow, &before, child, birth, point);
        let slot = Slot {
            queue: ChannelRole::Local(receiver),
            site: crate::provenance::address::OpaqueSite {
                function: flow.function.def_id,
                point,
            },
            ty: birth.future_ty,
        };
        let target = before
            .value(receiver)
            .root_queue_handles()
            .next()
            .unwrap()
            .target
            .clone();
        let role = Role::message(slot.clone());
        let mut state = before;
        state.queue_current.insert(
            slot.clone(),
            Contribution {
                targets: FxHashSet::from_iter([target.clone()]),
                value: state.value(child),
                births: FxHashSet::from_iter([birth]),
            },
        );
        assert!(
            !state.future_owners.known_role(&role),
            "typed queue backing alone never grants ownership"
        );
        assert!(!state
            .future_copies
            .role_peers(&role)
            .contains(&Role::root(child)));

        // This fixture supplies an exact copy relation; actual sends still need a closed-access proof.
        state.future_copies.assign_role(
            role.clone(),
            state.future_copies.witness(child),
            &Live::new(&state.live, &state.queue_current),
        );
        state
            .future_owners
            .grant_role(role.clone(), &state.future_copies);
        assert!(state.role_live(&role));
        assert_eq!(
            flow.current_role_value(&role, &state).unwrap().origins(),
            state.value(child).origins()
        );
        let mut missing = state.clone();
        missing.queue_current.remove(&slot);
        assert!(!missing.role_live(&role));
        assert!(flow.current_role_value(&role, &missing).is_none());

        let mut escaped = state.clone();
        Flow::lose_owning_operand(&Operand::Copy(Place::from_local(child)), &mut escaped);
        assert!(
            !escaped.future_owners.known_role(&role),
            "local escape revokes the entire copy class"
        );
        escaped.consume(child);
        assert!(!escaped.future_owners.known_role(&role));

        let mut alternative = state.clone();
        alternative.future_copies.forget_role(&role);
        alternative.future_owners.forget_role(&role);
        alternative.queue_current.remove(&slot);
        let mut joined = state.clone();
        assert!(joined.join(&alternative));
        assert!(joined.queue_current.contains_key(&slot));
        assert!(!joined.future_owners.known(child));
        assert!(!joined.future_owners.known_role(&role));

        state
            .queues
            .insert((target.clone(), birth.future_ty), state.value(child));
        let mut completed = state.clone();
        flow.transfers.remove(&point);
        flow.ensure_transfer(point, &completed);
        flow.complete_future(child, &Value::default(), point, &mut completed);
        assert!(completed.queue_current[&slot]
            .value
            .futures
            .values()
            .flatten()
            .all(|handle| !handle.pending));
        assert!(
            completed.queues[&(target, birth.future_ty)]
                .futures
                .values()
                .flatten()
                .any(|handle| handle.pending),
            "completion cannot sweep same-target weak history"
        );
        assert!(state.queue_current[&slot]
            .value
            .futures
            .values()
            .flatten()
            .any(|handle| handle.pending));

        flow.transfers.remove(&point);
        flow.ensure_transfer(point, &state);
        flow.preserve_writer_loss(&Place::from_local(child), point, &mut state);
        state.consume(child);
        state.consume(receiver);
        assert!(
            state.role_live(&role),
            "a message's actual backing outlives its queue template local"
        );
        assert_eq!(
            state.future_copies.role_witness(&role).peers,
            FxHashSet::from_iter([role.clone()])
        );
        assert!(state.future_owners.known_role(&role));
        assert!(
            state.residuals.is_empty(),
            "a represented surviving copy is not lost activity"
        );
        assert!(!state.queue_current[&slot].value.origins().is_empty());
        assert!(state
            .rooted_activity()
            .into_carriers()
            .any(
                |(storage, value)| storage == Storage::QueueCurrent(slot.clone())
                    && !value.origins().is_empty()
            ));
        flow.complete_owned_role(&role, None, point, &mut state);
        assert!(state.queue_current[&slot]
            .value
            .futures
            .values()
            .flatten()
            .all(|handle| !handle.pending));
    });
}

fn check_root_cell_sources(
    flow: &mut Flow<'_>,
    before: &State,
    child: Local,
    birth: Birth,
    point: Point,
) {
    use crate::provenance::demand::{Atom, Coordinate, Demand};
    let peer = flow
        .function
        .locals
        .iter()
        .find(|decl| {
            decl.local != child
                && crate::provenance::value::strip_mutable(decl.ty, flow.table) == birth.future_ty
        })
        .unwrap()
        .local;
    let mut state = before.clone();
    state.set(peer, state.value(child));
    flow.transfers.remove(&point);
    flow.ensure_transfer(point, &state);
    let copied = state.value(child).sourced(&Storage::Logical(child));
    assert!(flow.store_role_value(&Role::root(peer), copied, point, &mut state));
    let cell = state.cell(peer);
    let (path, origin, capture) = cell
        .leaves
        .keys()
        .next()
        .expect("actual callback borrow lineage");
    let atom = |storage| Atom {
        coordinate: Coordinate {
            storage,
            path: path.clone(),
        },
        origin: origin.clone(),
        capture: *capture,
    };
    assert_eq!(
        flow.transfers[&point].before(&Demand::from_iter([atom(Storage::Cell(peer))])),
        Demand::from_iter([atom(Storage::Logical(child))]),
        "reads through a surviving copy's cell must follow the newly stored source"
    );
}
