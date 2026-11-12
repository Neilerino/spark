// TAG [SPARK-114]

use super::*;
use crate::{
    facts::Point,
    provenance::{Flow, State},
};
use spark_mir::{
    AggregateKind, BlockId, LocalDecl, MirFunction, Operand, Place, Rvalue, Statement,
};
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::{IntWidth, Ty, TypeTable};

fn with_channels(run: impl FnOnce(&mut Flow<'_>, State)) {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let channel = table.intern(Ty::Channel { inner: int });
    let callback = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let future = table.intern(Ty::Future { inner: int });
    let function = MirFunction {
        def_id: DefId::new(9988),
        param_count: 0,
        return_type: int,
        locals: [
            int, channel, channel, channel, channel, callback, callback, future, future,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, ty)| LocalDecl {
            local: Local(index as u32),
            ty,
            name: None,
            is_user_variable: false,
            mutable: true,
            span: Span::DUMMY,
        })
        .collect(),
        blocks: Vec::new(),
        captures: Vec::new(),
        is_async: false,
        span: Span::DUMMY,
    };
    run(&mut Flow::new(&function, &table, None), State::default());
}

fn assign(flow: &mut Flow<'_>, state: &mut State, destination: Local, rvalue: Rvalue, step: u32) {
    flow.transfer_statement(
        &Statement::Assign {
            place: Place::from_local(destination),
            rvalue,
            span: Span::DUMMY,
        },
        Point::new(BlockId::ENTRY, step),
        state,
    );
}

fn fresh(flow: &mut Flow<'_>, state: &mut State, local: Local) {
    assign(
        flow,
        state,
        local,
        Rvalue::Aggregate(AggregateKind::Channel, Vec::new()),
        1,
    );
}

fn copy(flow: &mut Flow<'_>, state: &mut State, destination: Local, source: Local, step: u32) {
    assign(
        flow,
        state,
        destination,
        Rvalue::Use(Operand::Copy(Place::from_local(source))),
        step,
    );
}

#[test]
fn actual_channel_moves_do_not_equate_repeated_constructor_labels() {
    with_channels(|flow, mut state| {
        fresh(flow, &mut state, Local(1));
        assert!(state.channel_copies.witness(&Role::Local(Local(1))).fresh);
        copy(flow, &mut state, Local(2), Local(1), 2);
        assert_eq!(
            state.channel_copies.witness(&Role::Local(Local(1))).peers,
            FxHashSet::from_iter([Role::Local(Local(1)), Role::Local(Local(2))])
        );
        fresh(flow, &mut state, Local(1));
        assert_eq!(
            state.value(Local(1)).queues,
            state.value(Local(2)).queues,
            "the repeated constructor label is intentionally indistinguishable in MAY data"
        );
        assert_eq!(
            state.channel_copies.witness(&Role::Local(Local(1))).peers,
            FxHashSet::from_iter([Role::Local(Local(1))])
        );
        assign(
            flow,
            &mut state,
            Local(3),
            Rvalue::Use(Operand::Move(Place::from_local(Local(2)))),
            3,
        );
        assert!(!state.live.contains(&Local(2)));
        assert!(!state.channel_copies.witness(&Role::Local(Local(2))).fresh);
        assert!(state.channel_copies.witness(&Role::Local(Local(3))).fresh);
        copy(flow, &mut state, Local(4), Local(3), 4);
        assert_eq!(
            state.channel_copies.witness(&Role::Local(Local(3))).peers,
            FxHashSet::from_iter([Role::Local(Local(3)), Role::Local(Local(4))])
        );
        assert_eq!(
            state.future_copies.peers(Local(3)),
            FxHashSet::from_iter([Local(3)])
        );
    });
}

#[test]
fn channel_equality_changes_wake_the_flow_join_when_may_values_are_unchanged() {
    with_channels(|flow, mut right| {
        fresh(flow, &mut right, Local(1));
        copy(flow, &mut right, Local(2), Local(1), 2);
        fresh(flow, &mut right, Local(1));
        let mut left = right.clone();
        copy(flow, &mut left, Local(2), Local(1), 3);
        assert_eq!(left.values, right.values);
        assert_eq!(left.cells, right.cells);
        assert!(left.join(&right));
        assert_eq!(
            left.channel_copies.witness(&Role::Local(Local(1))).peers,
            FxHashSet::from_iter([Role::Local(Local(1))])
        );
        assert!(!left.join(&right));
    });
}

#[test]
fn physical_channel_storage_clobber_detaches_only_the_overwritten_handle() {
    with_channels(|flow, mut state| {
        fresh(flow, &mut state, Local(1));
        copy(flow, &mut state, Local(2), Local(1), 2);
        flow.clobber_input_owners(Local(1), &mut state);
        assert!(!state.channel_copies.witness(&Role::Local(Local(1))).fresh);
        let survivor = state.channel_copies.witness(&Role::Local(Local(2)));
        assert!(survivor.fresh);
        assert_eq!(
            survivor.peers,
            FxHashSet::from_iter([Role::Local(Local(2))])
        );
    });
}

#[test]
fn channel_captures_follow_closure_moves_and_current_execution_backing() {
    with_channels(|flow, mut state| {
        fresh(flow, &mut state, Local(1));
        let owner = DefId::new(9989);
        let ty = flow.function.locals[1].ty;
        assign(
            flow,
            &mut state,
            Local(5),
            Rvalue::Aggregate(
                AggregateKind::Closure { func_def_id: owner },
                vec![Operand::Copy(Place::from_local(Local(1)))],
            ),
            2,
        );
        let capture = crate::provenance::channels::Capture {
            owner,
            index: 0,
            ty,
        };
        let role = Role::Capture {
            local: Local(5),
            capture,
            callback_ty: None,
        };
        assert!(state.channel_copies.witness(&role).fresh);
        assign(
            flow,
            &mut state,
            Local(6),
            Rvalue::Use(Operand::Move(Place::from_local(Local(5)))),
            3,
        );
        assert!(!state.channel_copies.witness(&role).fresh);
        assert!(state.channel_copies.witness(&role.reanchor(Local(6))).fresh);
        let args = [Operand::Move(Place::from_local(Local(6)))];
        let transfer = flow.spawn_owner_transfer(&args, false, &state).unwrap();
        let point = Point::new(BlockId::ENTRY, 4);
        flow.ensure_transfer(point, &state);
        transfer.detach(flow, point, &mut state);
        state.consume(Local(6));
        let (_, future) = crate::provenance::tasks::birth(
            flow.function.def_id,
            point,
            flow.function.locals[7].ty,
        );
        state.set(Local(7), future);
        transfer.install(flow, Local(7), point, &mut state);
        let callback_ty = flow.function.locals[5].ty;
        let execution_role = role.in_execution(Local(7), callback_ty).unwrap();
        let witness = state.channel_copies.witness(&execution_role);
        assert!(witness.fresh);
        assert_eq!(
            witness.peers,
            FxHashSet::from_iter([Role::Local(Local(1)), execution_role.clone()])
        );
        assert!(state.current_execution_record(Local(7)).is_some());
        flow.complete_future(
            Local(7),
            &crate::provenance::Value::default(),
            Point::new(BlockId::ENTRY, 5),
            &mut state,
        );
        assert!(!state.channel_copies.witness(&execution_role).fresh);
        assert_eq!(
            state.channel_copies.witness(&Role::Local(Local(1))).peers,
            FxHashSet::from_iter([Role::Local(Local(1))])
        );
    });
}

#[test]
fn duplicate_channel_captures_keep_one_class_after_the_root_dies_and_execution_moves() {
    with_channels(|flow, mut state| {
        fresh(flow, &mut state, Local(1));
        let owner = DefId::new(9989);
        let ty = flow.function.locals[1].ty;
        assign(
            flow,
            &mut state,
            Local(5),
            Rvalue::Aggregate(
                AggregateKind::Closure { func_def_id: owner },
                vec![Operand::Copy(Place::from_local(Local(1))); 2],
            ),
            2,
        );
        flow.transfer_statement(
            &Statement::Drop {
                place: Place::from_local(Local(1)),
                span: Span::DUMMY,
            },
            Point::new(BlockId::ENTRY, 3),
            &mut state,
        );
        assign(
            flow,
            &mut state,
            Local(6),
            Rvalue::Use(Operand::Move(Place::from_local(Local(5)))),
            4,
        );
        let role = |local, index, callback_ty| Role::Capture {
            local,
            capture: crate::provenance::channels::Capture { owner, index, ty },
            callback_ty,
        };
        assert_eq!(
            state.channel_copies.witness(&role(Local(6), 0, None)).peers,
            FxHashSet::from_iter([role(Local(6), 0, None), role(Local(6), 1, None)])
        );
        let transfer = flow
            .spawn_owner_transfer(&[Operand::Move(Place::from_local(Local(6)))], false, &state)
            .unwrap();
        let point = Point::new(BlockId::ENTRY, 5);
        flow.ensure_transfer(point, &state);
        transfer.detach(flow, point, &mut state);
        state.consume(Local(6));
        state.set(
            Local(7),
            crate::provenance::tasks::birth(
                flow.function.def_id,
                point,
                flow.function.locals[7].ty,
            )
            .1,
        );
        transfer.install(flow, Local(7), point, &mut state);
        let callback_ty = Some(flow.function.locals[5].ty);
        let witness = state
            .channel_copies
            .witness(&role(Local(7), 0, callback_ty));
        assert!(witness.fresh);
        assert_eq!(
            witness.peers,
            FxHashSet::from_iter([
                role(Local(7), 0, callback_ty),
                role(Local(7), 1, callback_ty)
            ])
        );
    });
}

#[test]
fn overwriting_a_closure_does_not_reuse_old_capture_coordinates_as_new_peers() {
    with_channels(|flow, mut state| {
        fresh(flow, &mut state, Local(1));
        copy(flow, &mut state, Local(2), Local(1), 2);
        fresh(flow, &mut state, Local(1));
        assert_eq!(state.value(Local(1)).queues, state.value(Local(2)).queues);
        let owner = DefId::new(9989);
        for (step, sources) in [(3, [Local(2), Local(1)]), (4, [Local(1), Local(2)])] {
            assign(
                flow,
                &mut state,
                Local(5),
                Rvalue::Aggregate(
                    AggregateKind::Closure { func_def_id: owner },
                    sources
                        .into_iter()
                        .map(|local| Operand::Copy(Place::from_local(local)))
                        .collect(),
                ),
                step,
            );
        }
        let role = |index| Role::Capture {
            local: Local(5),
            capture: crate::provenance::channels::Capture {
                owner,
                index,
                ty: flow.function.locals[1].ty,
            },
            callback_ty: None,
        };
        let first = state.channel_copies.witness(&role(0));
        assert!(first.peers.contains(&Role::Local(Local(1))));
        assert!(!first.peers.contains(&role(1)),
            "a frozen old capture peer is not the field installed at its coordinate after overwrite");
        assert_eq!(
            first.peers,
            FxHashSet::from_iter([Role::Local(Local(1)), role(0)])
        );
        assert_eq!(
            state.channel_copies.witness(&role(1)).peers,
            FxHashSet::from_iter([Role::Local(Local(2)), role(1)])
        );
    });
}

#[test]
fn imported_memory_writes_cannot_restore_channel_capture_identity_after_clobber() {
    use crate::provenance::{
        address::OpaqueSite,
        observations::{FrameScope, Memory, Sample},
        tasks::{
            birth,
            memory::Lane,
            views::{Subject, View},
        },
        Value,
    };
    for overwritten in [Some(Local(1)), Some(Local(5)), Some(Local(2)), None] {
        with_channels(|flow, mut state| {
            fresh(flow, &mut state, Local(1));
            fresh(flow, &mut state, Local(2));
            state.set(Local(5), Value::function(DefId::new(9992)));
            let point = Point::new(BlockId::ENTRY, 2);
            let site = OpaqueSite {
                function: flow.function.def_id,
                point,
            };
            let (target, mut child) = birth(
                flow.function.def_id,
                Point::new(BlockId::ENTRY, 0),
                flow.function.locals[7].ty,
            );
            let actor = Subject {
                target,
                capture: None,
            };
            let ty = flow.function.locals[overwritten.unwrap_or(Local(1)).index()].ty;
            let memory = overwritten.map_or(Memory::Unavailable(site), |local| Memory::Frame {
                scope: FrameScope::Current,
                function: flow.function.def_id,
                local,
                ty,
            });
            let payload = if overwritten == Some(Local(5)) {
                Value::function(DefId::new(9993))
            } else {
                crate::provenance::channels::allocation(
                    flow.function.def_id,
                    Point::new(BlockId::ENTRY, 10),
                    ty,
                )
            };
            let mut view = View::pending(
                Value::function(DefId::new(9994)),
                flow.function.locals[5].ty,
            );
            view.memory.import(
                std::sync::Arc::new(Lane {
                    event: site,
                    actor: actor.clone(),
                    location: Sample {
                        memory,
                        path: Vec::new(),
                        ty,
                    },
                }),
                payload,
            );
            child
                .task_views
                .entry(Vec::new())
                .or_default()
                .insert(actor, view);
            state.set(Local(7), child);
            let owner = DefId::new(9989);
            assign(
                flow,
                &mut state,
                Local(5),
                Rvalue::Aggregate(
                    AggregateKind::Closure { func_def_id: owner },
                    vec![
                        Operand::Copy(Place::from_local(Local(1))),
                        Operand::Copy(Place::from_local(Local(7))),
                    ],
                ),
                2,
            );
            assert!(
                state
                    .invalidated_copies
                    .contains(&overwritten.unwrap_or(Local(1))),
                "the imported memory lane must really clobber the chosen storage"
            );
            let role = Role::Capture {
                local: Local(5),
                capture: crate::provenance::channels::Capture {
                    owner,
                    index: 0,
                    ty: flow.function.locals[1].ty,
                },
                callback_ty: None,
            };
            assert_eq!(state.channel_copies.witness(&role).fresh, overwritten == Some(Local(2)),
                "channel identities are installed only after revalidating imported clobbers: {overwritten:?}");
        });
    }
}

#[test]
fn execution_imports_revalidate_channel_captures_at_the_final_installation() {
    use crate::provenance::{
        address::OpaqueSite,
        observations::{FrameScope, Memory, Sample},
        tasks::{
            birth,
            memory::Lane,
            views::{Subject, View},
        },
        Slot, Value,
    };
    for overwritten in [Some(Local(8)), Some(Local(5)), Some(Local(2)), None] {
        with_channels(|flow, mut state| {
            fresh(flow, &mut state, Local(1));
            fresh(flow, &mut state, Local(2));
            let future_ty = flow.function.locals[7].ty;
            let callback_ty = flow.function.locals[5].ty;
            let (target, mut child) = birth(
                flow.function.def_id,
                Point::new(BlockId::ENTRY, 0),
                future_ty,
            );
            child.task_views.entry(Vec::new()).or_default().insert(
                Subject {
                    target,
                    capture: None,
                },
                View::pending(Value::function(DefId::new(9994)), callback_ty),
            );
            state.set(Local(7), child);
            let owner = DefId::new(9989);
            assign(
                flow,
                &mut state,
                Local(5),
                Rvalue::Aggregate(
                    AggregateKind::Closure { func_def_id: owner },
                    vec![
                        Operand::Copy(Place::from_local(Local(1))),
                        Operand::Copy(Place::from_local(Local(7))),
                    ],
                ),
                2,
            );
            let point = Point::new(BlockId::ENTRY, 3);
            flow.ensure_transfer(point, &state);
            let mut transfer = flow
                .spawn_owner_transfer(&[Operand::Move(Place::from_local(Local(5)))], false, &state)
                .unwrap();
            let site = OpaqueSite {
                function: flow.function.def_id,
                point,
            };
            let ty = flow.function.locals[overwritten.unwrap_or(Local(8)).index()].ty;
            let memory = overwritten.map_or(Memory::Unavailable(site), |local| Memory::Frame {
                scope: FrameScope::Current,
                function: flow.function.def_id,
                local,
                ty,
            });
            let payload = match overwritten {
                Some(Local(5)) => Value::function(DefId::new(9993)),
                Some(Local(2)) => crate::provenance::channels::allocation(
                    flow.function.def_id,
                    Point::new(BlockId::ENTRY, 10),
                    ty,
                ),
                _ => birth(flow.function.def_id, Point::new(BlockId::ENTRY, 10), ty).1,
            };
            transfer.visit_callback(&mut |callback| {
                let (actor, view) = callback
                    .task_views
                    .get_mut(&vec![Slot::Capture(owner, 1, future_ty)])
                    .unwrap()
                    .iter_mut()
                    .next()
                    .unwrap();
                view.memory.import(
                    std::sync::Arc::new(Lane {
                        event: site,
                        actor: actor.clone(),
                        location: Sample {
                            memory: memory.clone(),
                            path: Vec::new(),
                            ty,
                        },
                    }),
                    payload.clone(),
                );
            });
            transfer.detach(flow, point, &mut state);
            state.consume(Local(5));
            state.set(Local(8), birth(flow.function.def_id, point, future_ty).1);
            transfer.install(flow, Local(8), point, &mut state);
            assert!(
                state
                    .invalidated_copies
                    .contains(&overwritten.unwrap_or(Local(8))),
                "the post-snapshot import must really clobber its source or destination"
            );
            let role = Role::Capture {
                local: Local(8),
                capture: crate::provenance::channels::Capture {
                    owner,
                    index: 0,
                    ty: flow.function.locals[1].ty,
                },
                callback_ty: Some(callback_ty),
            };
            assert_eq!(
                state.channel_copies.witness(&role).fresh,
                overwritten == Some(Local(2))
            );
            assert_eq!(
                state.current_execution_record(Local(8)).is_some(),
                overwritten == Some(Local(2))
            );
        });
    }
}

#[test]
fn entry_lineage_survives_supported_transfers_without_becoming_freshness() {
    with_channels(|flow, mut state| {
        let ty = flow.function.locals[1].ty;
        let input = flow.symbolic_input(ty, Local(1), ty, &[]);
        let port = input
            .root_queue_handles()
            .next()
            .unwrap()
            .target
            .port(flow.table)
            .unwrap();
        state.set(Local(1), input);
        state.channel_copies.seed_input(Local(1), port.clone());
        copy(flow, &mut state, Local(2), Local(1), 2);
        assign(
            flow,
            &mut state,
            Local(3),
            Rvalue::Use(Operand::Move(Place::from_local(Local(2)))),
            3,
        );
        let witness = state.channel_copies.witness(&Role::Local(Local(3)));
        assert_eq!(witness.input.as_deref(), Some(&port));
        assert!(!witness.fresh);
        assert!(!state.live.contains(&Local(2)));
        assign(
            flow,
            &mut state,
            Local(5),
            Rvalue::Aggregate(
                AggregateKind::Closure {
                    func_def_id: DefId::new(9990),
                },
                vec![Operand::Copy(Place::from_local(Local(3)))],
            ),
            4,
        );
        assign(
            flow,
            &mut state,
            Local(6),
            Rvalue::Use(Operand::Move(Place::from_local(Local(5)))),
            5,
        );
        let roles: Vec<_> = state.channel_copies.anchored(Local(6)).cloned().collect();
        assert_eq!(roles.len(), 1);
        assert_eq!(
            state.channel_copies.witness(&roles[0]).input.as_deref(),
            Some(&port)
        );
        let callback_ty = flow.function.locals[6].ty;
        let captures = Captured::execution(Local(6), Local(7), callback_ty, true, &state);
        state.live.insert(Local(7));
        Captured::install_all(captures, Local(7), &[Local(6)], &mut state);
        let execution = roles[0].in_execution(Local(7), callback_ty).unwrap();
        assert_eq!(
            state.channel_copies.witness(&execution).input.as_deref(),
            Some(&port)
        );
        fresh(flow, &mut state, Local(1));
        assert!(state
            .channel_copies
            .witness(&Role::Local(Local(1)))
            .input
            .is_none());
        assert_eq!(
            state
                .channel_copies
                .witness(&Role::Local(Local(3)))
                .input
                .as_deref(),
            Some(&port),
            "replacing the formal coordinate does not retarget an old equal copy"
        );
        flow.clobber_input_owners(Local(3), &mut state);
        assert!(state
            .channel_copies
            .witness(&Role::Local(Local(3)))
            .input
            .is_none());
        assert_eq!(
            state.channel_copies.witness(&execution).input.as_deref(),
            Some(&port)
        );
    });
}
