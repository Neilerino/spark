// TAG [SPARK-114]

use super::keeper_activity::with_source_flow;
use super::*;

#[derive(Clone, Copy, Debug)]
enum Fault {
    None,
    Pending,
    Weak,
    MissingBirth,
    MissingOwner,
    UnknownAccess,
    Clobber(Local),
    Imported(Option<Local>),
}

#[test]
fn receipt_revalidates_present_identity_and_late_imports_without_losing_borrowed_data() {
    use crate::provenance::{
        address::OpaqueSite,
        observations::{FrameScope, Memory, Sample},
        tasks::memory::Lane,
    };
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_retained_child_memory_scoped.spark"
    ));
    with_source_flow(source, |flow| {
        let named = |name: &str| {
            flow.function
                .locals
                .iter()
                .find(|decl| decl.name.is_some_and(|symbol| symbol.as_str() == name))
                .unwrap()
                .local
        };
        let retained = named("retained");
        let unrelated = named("joined");
        let (index, block) = flow.function.blocks.iter().enumerate().find(|(_, block)| {
            matches!(&block.terminator, Terminator::Call {func,args,..}
                if flow.function.resolve_callee_def_id(func).and_then(|id| flow.program.unwrap().definitions.unwrap().get(id)).is_some_and(|info| info.name == spark_intern::kw::RECV)
                && matches!(args.first(), Some(Operand::Copy(place)) if place.local == retained))
        }).unwrap();
        let block = block.clone();
        let mut before = flow.entries[index].clone().unwrap();
        for (step, statement) in block.statements.iter().enumerate() {
            flow.transfer_statement(
                statement,
                Point::new(BlockId(index as u32), step as u32),
                &mut before,
            );
        }
        let Terminator::Call {
            func,
            args,
            destination,
            ..
        } = &block.terminator
        else {
            unreachable!()
        };
        let point = Point::new(BlockId(index as u32), block.statements.len() as u32);
        let (slot, promise) = before.queue_promises.iter().next().unwrap();
        let slot = slot.clone();
        let queue = promise.queue.clone();
        let birth = promise.recipe.birth;
        let original = before.queue_current[&slot].value.clone();
        assert!(promise.present);
        assert!(!original.origins().is_empty());
        for fault in [
            Fault::None,
            Fault::Pending,
            Fault::Weak,
            Fault::MissingBirth,
            Fault::MissingOwner,
            Fault::UnknownAccess,
            Fault::Clobber(retained),
            Fault::Clobber(destination.local),
            Fault::Clobber(unrelated),
            Fault::Imported(Some(retained)),
            Fault::Imported(Some(destination.local)),
            Fault::Imported(Some(unrelated)),
            Fault::Imported(None),
        ] {
            let mut state = before.clone();
            let role = Role::message(slot.clone());
            match fault {
                Fault::Pending => state.queue_promises.get_mut(&slot).unwrap().present = false,
                Fault::Weak => {
                    state
                        .queues
                        .insert((queue.clone(), slot.ty), original.clone());
                }
                Fault::MissingBirth => state.future_copies.forget_role(&role),
                Fault::MissingOwner => state.future_owners.forget_role(&role),
                _ => {}
            }
            flow.transfers.remove(&point);
            flow.ensure_transfer(point, &state);
            let mut dispatch =
                flow.call_result(func, args, flow.place_type(destination), point, &state);
            assert_eq!(dispatch.returning.len(), 1);
            let mut outcome = dispatch.returning.pop().unwrap();
            match fault {
                Fault::UnknownAccess => outcome.queue_accesses.unknown = true,
                Fault::Clobber(local) => outcome.clobbered_copies.push(local),
                Fault::Imported(overwritten) => {
                    let local = overwritten.unwrap_or(destination.local);
                    let ty = flow.function.locals[local.index()].ty;
                    let payload = if local == retained {
                        crate::provenance::channels::allocation(flow.function.def_id, point, ty)
                    } else if local == destination.local {
                        crate::provenance::tasks::birth(flow.function.def_id, point, ty).1
                    } else {
                        Value::default()
                    };
                    let value = &mut outcome.receipt.as_mut().unwrap().value;
                    let (actor, view) = value
                        .task_views
                        .get_mut(&Vec::new())
                        .unwrap()
                        .iter_mut()
                        .next()
                        .unwrap();
                    view.memory.import(
                        std::sync::Arc::new(Lane {
                            event: OpaqueSite {
                                function: flow.function.def_id,
                                point,
                            },
                            actor: actor.clone(),
                            location: Sample {
                                memory: overwritten.map_or(
                                    Memory::Unavailable(OpaqueSite {
                                        function: flow.function.def_id,
                                        point,
                                    }),
                                    |local| Memory::Frame {
                                        scope: FrameScope::Current,
                                        function: flow.function.def_id,
                                        local,
                                        ty,
                                    },
                                ),
                                path: Vec::new(),
                                ty,
                            },
                        }),
                        payload,
                    );
                }
                _ => {}
            }
            flow.apply_call_outcomes(args, destination, vec![outcome], point, &mut state);
            let accepted = matches!(fault, Fault::None)
                || matches!(fault, Fault::Clobber(local) | Fault::Imported(Some(local)) if local == unrelated);
            assert_eq!(
                state.future_owners.known(destination.local),
                accepted,
                "{fault:?}"
            );
            assert_eq!(
                state
                    .future_copies
                    .witness(destination.local)
                    .birth
                    .is_some(),
                accepted,
                "{fault:?}"
            );
            assert_eq!(
                state.current_execution_record(destination.local).is_some(),
                accepted,
                "{fault:?}"
            );
            if let Fault::Imported(overwritten) = fault {
                assert!(
                    !state.invalidated_copies.is_empty(),
                    "{fault:?} actually imports a physical clobber"
                );
                if let Some(local) = overwritten {
                    assert!(state.invalidated_copies.contains(&local));
                }
            }
            let retained_origins: Origins = state
                .values
                .values()
                .chain(state.queues.values())
                .chain(state.queue_current.values().map(|message| &message.value))
                .flat_map(Value::origins)
                .collect();
            assert!(
                original.origins().is_subset(&retained_origins),
                "{fault:?} conserves all borrowed payload support"
            );
            if !accepted {
                assert!(
                    state
                        .queues
                        .values()
                        .chain(state.queue_current.values().map(|message| &message.value))
                        .any(|value| birth.record(value).is_some()),
                    "{fault:?} preserves the full pending callback record"
                );
            }
            if accepted {
                if matches!(fault, Fault::None) {
                    assert_receipt_source_frontiers(flow, &state, &slot, destination.local, point);
                }
                assert!(!state.queue_current.contains_key(&slot));
                assert!(!state.queues.contains_key(&(queue.clone(), slot.ty)));
                flow.transfers.remove(&point);
                flow.ensure_transfer(point, &state);
                let second =
                    flow.call_result(func, args, flow.place_type(destination), point, &state);
                assert!(
                    second
                        .returning
                        .iter()
                        .all(|outcome| outcome.receipt.is_none()),
                    "a second receive cannot reuse the popped message"
                );
            }
        }
    });
}

#[test]
fn try_receive_keeps_optional_message_data_without_receipt_authority() {
    let source = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spark_driver/tests/run-pass/async/future_task_retained_child_memory_scoped.spark"
    ));
    let source = source.replace(
        "child: Future[Int] = retained.recv()\n    childJoined: Int = await child",
        "child: Future[Int]? = retained.tryRecv()",
    );
    with_source_flow(&source, |flow| {
        let (index, block) = flow.function.blocks.iter().enumerate().find(|(_, block)| {
            matches!(&block.terminator, Terminator::Call {func,..} if flow.function.resolve_callee_def_id(func).and_then(|id| flow.program.unwrap().definitions.unwrap().get(id)).is_some_and(|info| info.name == spark_intern::kw::TRY_RECV))
        }).unwrap();
        let block = block.clone();
        let mut state = flow.entries[index].clone().unwrap();
        for (step, statement) in block.statements.iter().enumerate() {
            flow.transfer_statement(
                statement,
                Point::new(BlockId(index as u32), step as u32),
                &mut state,
            );
        }
        assert_eq!(state.queue_promises.len(), 1);
        assert!(state.queue_promises.values().all(|promise| promise.present));
        let Terminator::Call {
            func,
            args,
            destination,
            ..
        } = &block.terminator
        else {
            unreachable!()
        };
        let point = Point::new(BlockId(index as u32), block.statements.len() as u32);
        flow.ensure_transfer(point, &state);
        let dispatch = flow.call_result(func, args, flow.place_type(destination), point, &state);
        assert!(!dispatch.returning.is_empty());
        assert!(dispatch
            .returning
            .iter()
            .all(|outcome| outcome.receipt.is_none()));
        flow.apply_call_outcomes(args, destination, dispatch.returning, point, &mut state);
        assert!(!state.future_owners.known(destination.local));
        assert!(state
            .future_copies
            .witness(destination.local)
            .birth
            .is_none());
        assert!(state.queues.values().any(|value| value
            .futures
            .values()
            .flatten()
            .any(|handle| handle.pending)));
    });
}

fn assert_receipt_source_frontiers(
    flow: &Flow<'_>,
    state: &State,
    slot: &crate::provenance::channels::messages::Slot,
    destination: Local,
    point: Point,
) {
    use crate::provenance::demand::{Atom, Coordinate, Demand, Storage};
    use crate::provenance::Origin;
    let record = state.current_execution_record(destination).unwrap();
    for (storage, value) in [
        (Storage::Cell(destination), state.cell(destination)),
        (
            Storage::ExecutionCallback(destination, record.callback_ty),
            record.callback,
        ),
    ] {
        let (path, origin, capture) = value
            .leaves
            .keys()
            .find(|(_, origin, _)| matches!(origin, Origin::Loan(_)))
            .expect("actual borrowed callback support");
        let demand = Atom {
            coordinate: Coordinate {
                storage: storage.clone(),
                path: path.clone(),
            },
            origin: origin.clone(),
            capture: *capture,
        };
        let before = flow.transfers[&point].before(&Demand::from_iter([demand]));
        assert!(
            before.iter().any(|atom| {
                let root = match &atom.coordinate.storage {
                    Storage::CallbackNode(location) => location.root(),
                    storage => storage,
                };
                *root == Storage::QueueCurrent(slot.clone()) && atom.origin == *origin
            }),
            "{storage:?} must reach the entry message after the strong pop: {before:?}"
        );
    }
}
