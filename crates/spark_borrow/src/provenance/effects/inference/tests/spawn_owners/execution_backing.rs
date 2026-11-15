// TAG [SPARK-114]

use super::*;

#[test]
fn current_execution_backing_keeps_child_completion_without_reusing_the_spawn_snapshot() {
    with_keeper(KEEPER, |flow, mut state, parent, child, captured, _, _| {
        flow.complete_future(
            child,
            &Value::default(),
            Point::new(BlockId::ENTRY, 100),
            &mut state,
        );
        let current = state
            .current_execution_record(parent)
            .expect("actual adoption provides complete current backing");
        let parent_value = state
            .value(parent)
            .sourced(&crate::provenance::demand::Storage::Logical(parent));
        let handle = parent_value.futures[&Vec::new()]
            .iter()
            .find(|handle| handle.pending)
            .unwrap();
        let (records, unknown) = crate::provenance::tasks::views::pending_records_at(
            &parent_value,
            &[],
            handle,
            &state,
            &crate::provenance::demand::Storage::Logical(parent),
        );
        assert!(!unknown);
        assert!(
            records.iter().all(|record| record
                .callback
                .futures
                .values()
                .flatten()
                .all(|handle| !handle.pending)),
            "pending queries must use the covered current callback, not the old snapshot"
        );
        assert!(current
            .callback
            .futures
            .values()
            .flatten()
            .all(|handle| !handle.pending));
        assert!(
            state
                .value(parent)
                .task_views
                .values()
                .flat_map(|subjects| subjects.values())
                .flat_map(|view| view.callbacks.values())
                .any(|callback| callback
                    .value()
                    .futures
                    .values()
                    .flatten()
                    .any(|handle| handle.pending)),
            "the spawn snapshot deliberately retains the old child state"
        );
        assert!(state
            .executions
            .contains_key(&(parent, captured.callback_type().unwrap())));
    });
}

fn pending_callback_children(state: &State, parent: Local) -> bool {
    use crate::provenance::{demand::Storage, tasks::views};
    let value = state.value(parent).sourced(&Storage::Logical(parent));
    let handle = value.futures[&Vec::new()]
        .iter()
        .find(|handle| handle.pending)
        .unwrap();
    let (records, _) =
        views::pending_records_at(&value, &[], handle, state, &Storage::Logical(parent));
    records.iter().any(|record| {
        record
            .callback
            .futures
            .values()
            .flatten()
            .any(|handle| handle.pending)
    })
}

#[test]
fn current_execution_backing_meets_missing_and_may_alternatives_in_both_orders() {
    with_keeper(
        KEEPER,
        |flow, mut adopted, parent, child, captured, _, _| {
            let unadopted_value = adopted.value(parent);
            flow.complete_future(
                child,
                &Value::default(),
                Point::new(BlockId::ENTRY, 100),
                &mut adopted,
            );
            assert!(adopted.current_execution_record(parent).is_some());
            let mut missing = State::default();
            missing.set(parent, unadopted_value);
            let mut may = adopted.clone();
            let ty = captured.callback_type().unwrap();
            flow.ensure_transfer(Point::new(BlockId::ENTRY, 101), &may);
            flow.install_callback_outputs(
                FxHashMap::from_iter([(
                    ty,
                    crate::provenance::tasks::outputs::CallbackOutput {
                        callback: adopted.executions[&(parent, ty)].clone(),
                        ..crate::provenance::tasks::outputs::CallbackOutput::default()
                    },
                )]),
                &Place::from_local(parent),
                Point::new(BlockId::ENTRY, 101),
                &mut may,
            );
            assert!(may.current_execution_record(parent).is_none());
            for other in [missing, may] {
                for (mut first, second) in [
                    (adopted.clone(), other.clone()),
                    (other.clone(), adopted.clone()),
                ] {
                    first.join(&second);
                    assert!(first.executions.contains_key(&(parent, ty)));
                    assert!(
                        first.current_execution_record(parent).is_none(),
                        "MAY backing cannot become complete through a join"
                    );
                    assert!(
                        pending_callback_children(&first, parent),
                        "the unrepresented resident alternative must remain reachable"
                    );
                }
            }
        },
    );
}

#[test]
fn current_execution_backing_joins_complete_alternatives_without_completing_their_children() {
    with_keeper(KEEPER, |flow, pending, parent, child, _, _, _| {
        let mut completed = pending.clone();
        flow.complete_future(
            child,
            &Value::default(),
            Point::new(BlockId::ENTRY, 100),
            &mut completed,
        );
        for (mut first, second) in [(pending.clone(), completed.clone()), (completed, pending)] {
            first.join(&second);
            assert!(first.current_execution_record(parent).is_some());
            assert!(
                pending_callback_children(&first, parent),
                "one complete callback still has a pending child alternative"
            );
        }
    });
}

#[test]
fn current_execution_backing_requires_actual_copy_identity_and_survives_owner_escape() {
    with_keeper(KEEPER, |flow, mut state, parent, child, _, extras, _| {
        let [alias, unrelated, _] = extras[..] else {
            unreachable!()
        };
        state.set(unrelated, state.value(parent));
        assert!(
            state.current_execution_record(unrelated).is_none(),
            "the same static target is not a copy witness"
        );
        assign(flow, &mut state, alias, copied(parent), 100);
        assert!(state.current_execution_record(alias).is_some());
        state
            .future_owners
            .revoke(&state.future_copies.witness(child), &state.future_copies);
        assert!(!state.future_owners.known(child));
        assert!(
            state.current_execution_record(alias).is_some(),
            "owner coverage and callback completeness are independent"
        );
        flow.clobber_input_owners(alias, &mut state);
        flow.complete_future(
            child,
            &Value::default(),
            Point::new(BlockId::ENTRY, 102),
            &mut state,
        );
        flow.transfer_statement(&Statement::Nop, Point::new(BlockId::ENTRY, 101), &mut state);
        assert!(
            state.current_execution_record(parent).is_none(),
            "a clobbered copy group must lose authority after the per-point invalidation clears"
        );
    });
}

#[test]
fn current_execution_backing_does_not_select_one_of_several_callback_facets() {
    with_keeper(KEEPER, |flow, mut state, parent, _, captured, extras, _| {
        let alias = extras[0];
        assign(flow, &mut state, alias, copied(parent), 100);
        assert!(state.current_execution_record(parent).is_some());
        let ty = captured.callback_type().unwrap();
        flow.ensure_transfer(Point::new(BlockId::ENTRY, 101), &state);
        flow.install_callback_outputs(
            FxHashMap::from_iter([(
                ty,
                crate::provenance::tasks::outputs::CallbackOutput {
                    callback: state.executions[&(parent, ty)].clone(),
                    ..crate::provenance::tasks::outputs::CallbackOutput::default()
                },
            )]),
            &Place::from_local(alias),
            Point::new(BlockId::ENTRY, 101),
            &mut state,
        );
        assert_eq!(state.executions.len(), 2);
        assert!(state.current_execution_record(parent).is_none());
        assert!(state.current_execution_record(alias).is_none());
        assert!(pending_callback_children(&state, parent));
    });
}

#[test]
fn current_execution_backing_cannot_be_restored_after_imported_memory_clobbers() {
    use crate::provenance::{
        address::OpaqueSite,
        demand::Storage,
        observations::{FrameScope, Memory, Sample},
        tasks::{memory::Lane, views::Subject},
    };
    for unavailable in [false, true] {
        with_keeper(KEEPER, |flow, mut state, parent, _, captured, extras, _| {
            let destination = extras[0];
            let point = Point::new(BlockId::ENTRY, 100);
            flow.ensure_transfer(point, &state);
            let place = Place::from_local(destination);
            let mut graphs =
                flow.moving_executions(&Rvalue::Use(moved(parent)), &place, point, &state);
            let mut graph = graphs.pop().unwrap();
            assert!(graphs.is_empty());
            let mut root = state.value(parent).sourced(&Storage::Logical(parent));
            root = flow.freeze_sources(point, root);
            let witness = state.future_copies.witness(parent);
            let site = OpaqueSite {
                function: flow.function.def_id,
                point,
            };
            let ty = flow.function.locals[destination.index()].ty;
            let memory = if unavailable {
                Memory::Unavailable(site)
            } else {
                Memory::Frame {
                    scope: FrameScope::Current,
                    function: flow.function.def_id,
                    local: destination,
                    ty,
                }
            };
            let payload = crate::provenance::tasks::birth(
                flow.function.def_id,
                Point::new(BlockId::ENTRY, 150),
                ty,
            )
            .1;
            graph.visit_callback(&mut |callback| {
                let (actor, view) = callback
                    .task_views
                    .get_mut(captured.selection())
                    .unwrap()
                    .iter_mut()
                    .next()
                    .unwrap();
                let lane = std::sync::Arc::new(Lane {
                    execution: crate::provenance::effects::Execution::Current,
                    event: site,
                    actor: Subject {
                        target: actor.target.clone(),
                        capture: actor.capture,
                    },
                    location: Sample {
                        memory: memory.clone(),
                        path: Vec::new(),
                        ty,
                    },
                });
                view.memory.import(lane, payload.clone());
            });
            graph.detach(flow, point, &mut state);
            flow.record_store(point, &place, &root);
            state.consume(parent);
            state.set(destination, root);
            state
                .future_copies
                .assign(destination, witness, &state.live);
            graph.install(flow, destination, point, &mut state);
            assert!(
                state.invalidated_copies.contains(&destination),
                "the imported lane must really clobber the destination"
            );
            flow.transfer_statement(&Statement::Nop, Point::new(BlockId::ENTRY, 101), &mut state);
            assert!(state.current_execution_record(destination).is_none(),
                "final callback installation must not restore authority revoked by its own memory import");
        });
    }
}

#[test]
fn forwarding_cannot_restore_root_owner_coverage_after_imported_memory_clobbers() {
    use crate::provenance::{
        address::OpaqueSite,
        demand::Storage,
        observations::{FrameScope, Memory, Sample},
        tasks::{memory::Lane, proofs::CallProof},
    };
    for invalid in [true, false] {
        with_keeper(KEEPER, |flow, mut state, parent, _, captured, extras, _| {
            let [destination, unrelated, _] = extras[..] else {
                unreachable!()
            };
            let point = Point::new(BlockId::ENTRY, 100);
            let ty = flow.function.locals[destination.index()].ty;
            state.set(
                unrelated,
                crate::provenance::tasks::birth(
                    flow.function.def_id,
                    Point::new(BlockId::ENTRY, 99),
                    ty,
                )
                .1,
            );
            state.future_owners.grant(parent, &state.future_copies);
            assert!(state.future_owners.known(parent));
            let root = state.value(parent).sourced(&Storage::Logical(parent));
            flow.ensure_transfer(point, &state);
            let root = flow.freeze_sources(point, root);
            let witness = state.future_copies.witness(parent);
            let mut proof = CallProof {
                returned: Some(state.future_copies.witness(parent)),
                ..CallProof::default()
            };
            proof
                .forwarded_provided
                .insert(0, state.future_copies.witness(parent));
            let site = OpaqueSite {
                function: flow.function.def_id,
                point,
            };
            let target = if invalid { destination } else { unrelated };
            let callback = state
                .executions
                .get_mut(&(parent, captured.callback_type().unwrap()))
                .unwrap();
            let (actor, view) = callback
                .task_views
                .get_mut(captured.selection())
                .unwrap()
                .iter_mut()
                .next()
                .unwrap();
            view.memory.import(
                std::sync::Arc::new(Lane {
                    execution: crate::provenance::effects::Execution::Current,
                    event: site,
                    actor: actor.clone(),
                    location: Sample {
                        memory: Memory::Frame {
                            scope: FrameScope::Current,
                            function: flow.function.def_id,
                            local: target,
                            ty,
                        },
                        path: Vec::new(),
                        ty,
                    },
                }),
                crate::provenance::tasks::birth(
                    flow.function.def_id,
                    Point::new(BlockId::ENTRY, 150),
                    ty,
                )
                .1,
            );
            let place = Place::from_local(destination);
            let mut forwarded = flow
                .forward_input(&proof, &[moved(parent)], &place, &[], point, &state)
                .unwrap();
            forwarded.detach(flow, point, &mut state);
            flow.record_store(point, &place, &root);
            state.consume(parent);
            state.set(destination, root);
            state
                .future_copies
                .assign(destination, witness, &state.live);
            forwarded.install(flow, destination, point, &mut state);
            assert!(state.invalidated_copies.contains(&target));
            flow.transfer_statement(&Statement::Nop, Point::new(BlockId::ENTRY, 101), &mut state);
            assert_eq!(
                state.future_owners.known(destination),
                !invalid,
                "forwarding must not restore root owner coverage revoked by graph import"
            );
            assert_eq!(
                state.current_execution_record(destination).is_some(),
                !invalid
            );
        });
    }
}
