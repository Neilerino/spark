// TAG [SPARK-114]

use super::*;
use crate::provenance::{Flow, Origin, State};
use spark_mir::{BlockId, LocalDecl, MirFunction};
use spark_span::Span;
use spark_types::{IntWidth, LifetimeId};

#[test]
fn task_only_metadata_grows_joins_and_survives_bounded_capture_paths() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let callable = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let (_, mut value) = birth(DefId::new(9901), Point::new(BlockId::ENTRY, 1), future);
    for _ in 0..32 {
        value = value.prefixed(
            Slot::Capture(DefId::new(9902), 0, callable),
            callable,
            &table,
        );
    }
    let mut joined = Value::default();
    assert!(joined.join(&value));
    assert!(!joined.is_empty());
    assert!(!joined.join(&value));
    assert!(joined.futures.keys().all(|path| path.len() <= 2));
    let selected = joined.capture(DefId::new(9902), 0);
    assert_eq!(selected.futures.values().flatten().count(), 1);
}

#[test]
fn completing_a_current_copy_preserves_result_borrows_and_unproved_other_instances() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let future = table.intern(Ty::Future { inner: reference });
    let function = MirFunction {
        def_id: DefId::new(9910),
        param_count: 0,
        return_type: int,
        locals: [int, future, future, future]
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
        blocks: Vec::new(),
        captures: Vec::new(),
        is_async: false,
        span: Span::DUMMY,
    };
    let mut flow = Flow::new(&function, &table, None);
    let (_, mut pending) = birth(function.def_id, Point::new(BlockId::ENTRY, 1), future);
    pending.join(
        &Value::reference(Origin::Loan(Point::new(BlockId::ENTRY, 2))).prefixed(
            Slot::Tail,
            future,
            &table,
        ),
    );
    let result = Value::reference(Origin::Loan(Point::new(BlockId::ENTRY, 3)));
    let mut state = State::default();
    for index in 1..=3 {
        state.set(Local(index), pending.clone());
    }
    state
        .future_copies
        .assign(Local(2), state.future_copies.witness(Local(1)), &state.live);
    flow.complete_future(Local(2), &result, Point::new(BlockId::ENTRY, 4), &mut state);
    for index in [1, 2] {
        let completed = state.value(Local(index));
        assert!(completed
            .futures
            .values()
            .flatten()
            .all(|handle| !handle.pending));
        assert_eq!(
            completed.project(&Slot::Element).origins(),
            result.origins()
        );
        assert!(!completed
            .leaves
            .keys()
            .any(|(path, ..)| *path == [Slot::Tail]));
    }
    assert!(state
        .value(Local(3))
        .futures
        .values()
        .flatten()
        .all(|handle| handle.pending));
}

#[test]
fn future_result_keeps_result_borrows_without_pending_capture_tails() {
    let capture = Origin::Loan(Point::new(BlockId::ENTRY, 10));
    let result = Origin::Loan(Point::new(BlockId::ENTRY, 11));
    let mut value = Value::default();
    value.insert(&[Slot::Tail], FxHashSet::from_iter([capture]));
    value.insert(&[Slot::Element], FxHashSet::from_iter([result.clone()]));
    assert_eq!(
        value.future_result().origins(),
        FxHashSet::from_iter([result])
    );
}

#[test]
fn capturing_a_current_future_preserves_its_closed_owner_group_and_late_sources() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: int,
        is_async: false,
    });
    let callback = DefId::new(9921);
    let function = MirFunction {
        def_id: DefId::new(9920),
        param_count: 0,
        return_type: int,
        locals: [int, future, callable, callable, future]
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
    let mut flow = Flow::new(&function, &table, None);
    let (_, mut child) = birth(function.def_id, Point::new(BlockId::ENTRY, 1), future);
    let late = Origin::Loan(Point::new(BlockId::ENTRY, 2));
    child.join(&Value::reference(late.clone()).prefixed(Slot::Tail, future, &table));
    let mut state = State::default();
    state.set(Local(1), child);
    state.future_owners.grant(Local(1), &state.future_copies);
    let mut moved_state = state.clone();
    let point = Point::new(BlockId::ENTRY, 3);
    flow.transfer_statement(
        &spark_mir::Statement::Assign {
            place: spark_mir::Place::from_local(Local(2)),
            rvalue: spark_mir::Rvalue::Aggregate(
                spark_mir::AggregateKind::Closure {
                    func_def_id: callback,
                },
                vec![spark_mir::Operand::Copy(spark_mir::Place::from_local(
                    Local(1),
                ))],
            ),
            span: Span::DUMMY,
        },
        point,
        &mut state,
    );
    let captured = state.value(Local(2)).capture(callback, 0);
    assert!(
        captured.origins().contains(&late),
        "capture must retain the original late-write source"
    );
    assert!(!captured.futures.is_empty());
    assert!(captured
        .futures
        .values()
        .flatten()
        .all(|handle| handle.pending));
    assert!(state
        .value(Local(1))
        .futures
        .values()
        .flatten()
        .all(|handle| handle.pending));
    assert!(
        state.future_owners.known(Local(1)),
        "a typed by-value capture is another covered owner, not an opaque escape"
    );
    let capture = Role::capture(Local(2), callback, 0, future);
    assert_eq!(state.future_copies.witness(Local(1)).peers.len(), 2);
    assert!(state.future_owners.known_role(&capture));

    let mut relocated = state.clone();
    flow.transfer_statement(
        &spark_mir::Statement::Assign {
            place: spark_mir::Place::from_local(Local(3)),
            rvalue: spark_mir::Rvalue::Use(spark_mir::Operand::Move(spark_mir::Place::from_local(
                Local(2),
            ))),
            span: Span::DUMMY,
        },
        Point::new(BlockId::ENTRY, 9),
        &mut relocated,
    );
    let relocated_capture = Role::capture(Local(3), callback, 0, future);
    assert!(relocated
        .value(Local(3))
        .capture(callback, 0)
        .origins()
        .contains(&late));
    assert!(!relocated.live.contains(&Local(2)));
    assert!(
        relocated.future_owners.known_role(&relocated_capture),
        "moving an environment transfers its existing capture owner"
    );
    assert_eq!(
        relocated.future_copies.witness(Local(1)).peers,
        FxHashSet::from_iter([Role::root(Local(1)), relocated_capture])
    );

    let mut opaque = state.clone();
    flow.transfer_statement(
        &spark_mir::Statement::Assign {
            place: spark_mir::Place::from_local(Local(3)),
            rvalue: spark_mir::Rvalue::Use(spark_mir::Operand::Copy(spark_mir::Place::from_local(
                Local(2),
            ))),
            span: Span::DUMMY,
        },
        Point::new(BlockId::ENTRY, 4),
        &mut opaque,
    );
    assert!(
        !opaque.future_owners.known(Local(1)),
        "an untracked environment copy revokes group coverage"
    );
    assert!(!opaque.future_owners.known_role(&capture));

    flow.transfer_statement(
        &spark_mir::Statement::Drop {
            place: spark_mir::Place::from_local(Local(1)),
            span: Span::DUMMY,
        },
        Point::new(BlockId::ENTRY, 5),
        &mut state,
    );
    assert!(!state.live.contains(&Local(1)));
    assert!(state.future_owners.known_role(&capture));
    let captured = state.value(Local(2)).capture(callback, 0);
    assert!(captured.origins().contains(&late));
    assert!(
        captured
            .futures
            .values()
            .flatten()
            .all(|handle| handle.pending),
        "dropping one of two owners cannot complete the captured execution"
    );

    let (_, replacement) = birth(function.def_id, Point::new(BlockId::ENTRY, 1), future);
    state.set(Local(4), replacement);
    state.future_owners.grant(Local(4), &state.future_copies);
    flow.transfer_statement(
        &spark_mir::Statement::Assign {
            place: spark_mir::Place::from_local(Local(1)),
            rvalue: spark_mir::Rvalue::Use(spark_mir::Operand::Move(spark_mir::Place::from_local(
                Local(4),
            ))),
            span: Span::DUMMY,
        },
        Point::new(BlockId::ENTRY, 6),
        &mut state,
    );
    assert!(
        !state
            .future_copies
            .witness(Local(1))
            .peers
            .contains(&capture),
        "a repeated birth site does not identify the captured old execution"
    );
    flow.complete_future(
        Local(1),
        &Value::default(),
        Point::new(BlockId::ENTRY, 7),
        &mut state,
    );
    let captured = state.value(Local(2)).capture(callback, 0);
    assert!(captured.origins().contains(&late));
    assert!(captured
        .futures
        .values()
        .flatten()
        .all(|handle| handle.pending));

    flow.transfer_statement(
        &spark_mir::Statement::Assign {
            place: spark_mir::Place::from_local(Local(2)),
            rvalue: spark_mir::Rvalue::Aggregate(
                spark_mir::AggregateKind::Closure {
                    func_def_id: callback,
                },
                vec![spark_mir::Operand::Move(spark_mir::Place::from_local(
                    Local(1),
                ))],
            ),
            span: Span::DUMMY,
        },
        Point::new(BlockId::ENTRY, 8),
        &mut moved_state,
    );
    assert!(!moved_state.live.contains(&Local(1)));
    assert!(moved_state.future_owners.known_role(&capture));
    assert_eq!(
        moved_state.future_copies.role_peers(&capture),
        FxHashSet::from_iter([capture])
    );
    assert!(moved_state
        .value(Local(2))
        .capture(callback, 0)
        .origins()
        .contains(&late));
}

#[test]
fn replacing_capture_slots_does_not_reconnect_old_entry_witnesses() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: int,
        is_async: false,
    });
    let callback = DefId::new(9931);
    let function = MirFunction {
        def_id: DefId::new(9930),
        param_count: 0,
        return_type: int,
        locals: [int, future, future, callable]
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
    let mut flow = Flow::new(&function, &table, None);
    let mut state = State::default();
    for local in [Local(1), Local(2)] {
        let (_, pending) = birth(function.def_id, Point::new(BlockId::ENTRY, local.0), future);
        state.set(local, pending);
    }
    let construct = |inputs: &[Local]| spark_mir::Statement::Assign {
        place: spark_mir::Place::from_local(Local(3)),
        rvalue: spark_mir::Rvalue::Aggregate(
            spark_mir::AggregateKind::Closure {
                func_def_id: callback,
            },
            inputs
                .iter()
                .map(|local| spark_mir::Operand::Copy(spark_mir::Place::from_local(*local)))
                .collect(),
        ),
        span: Span::DUMMY,
    };
    flow.transfer_statement(
        &construct(&[Local(1), Local(2)]),
        Point::new(BlockId::ENTRY, 3),
        &mut state,
    );
    let first_slot = Role::capture(Local(3), callback, 0, future);
    assert!(state
        .future_copies
        .witness(Local(1))
        .peers
        .contains(&first_slot));
    flow.transfer_statement(
        &construct(&[Local(2), Local(1)]),
        Point::new(BlockId::ENTRY, 4),
        &mut state,
    );
    assert_eq!(
        state.future_copies.witness(Local(1)).peers,
        FxHashSet::from_iter([
            Role::root(Local(1)),
            Role::capture(Local(3), callback, 1, future),
        ]),
        "old capture coordinates cannot name newly installed values"
    );
    assert_eq!(
        state.future_copies.witness(Local(2)).peers,
        FxHashSet::from_iter([Role::root(Local(2)), first_slot,])
    );
}

#[test]
fn moving_two_aliases_into_a_closure_keeps_both_capture_owners_related() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: int,
        is_async: false,
    });
    let callback = DefId::new(9941);
    let function = MirFunction {
        def_id: DefId::new(9940),
        param_count: 0,
        return_type: int,
        locals: [int, future, future, callable, callable]
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
    let mut flow = Flow::new(&function, &table, None);
    let mut state = State::default();
    let (_, pending) = birth(function.def_id, Point::new(BlockId::ENTRY, 1), future);
    state.set(Local(1), pending);
    state.future_owners.grant(Local(1), &state.future_copies);
    flow.transfer_statement(
        &spark_mir::Statement::Assign {
            place: spark_mir::Place::from_local(Local(2)),
            rvalue: spark_mir::Rvalue::Use(spark_mir::Operand::Copy(spark_mir::Place::from_local(
                Local(1),
            ))),
            span: Span::DUMMY,
        },
        Point::new(BlockId::ENTRY, 2),
        &mut state,
    );
    flow.transfer_statement(
        &spark_mir::Statement::Assign {
            place: spark_mir::Place::from_local(Local(3)),
            rvalue: spark_mir::Rvalue::Aggregate(
                spark_mir::AggregateKind::Closure {
                    func_def_id: callback,
                },
                [Local(1), Local(2)]
                    .into_iter()
                    .map(|local| spark_mir::Operand::Move(spark_mir::Place::from_local(local)))
                    .collect(),
            ),
            span: Span::DUMMY,
        },
        Point::new(BlockId::ENTRY, 3),
        &mut state,
    );
    let first = Role::capture(Local(3), callback, 0, future);
    let second = Role::capture(Local(3), callback, 1, future);
    assert!(!state.live.contains(&Local(1)) && !state.live.contains(&Local(2)));
    assert!(state.future_owners.known_role(&first) && state.future_owners.known_role(&second));
    assert_eq!(
        state.future_copies.role_peers(&first),
        FxHashSet::from_iter([first.clone(), second.clone()])
    );
    assert_eq!(
        state.future_copies.role_peers(&second),
        FxHashSet::from_iter([first, second])
    );
    let move_environment = |source, destination| spark_mir::Statement::Assign {
        place: spark_mir::Place::from_local(Local(destination)),
        rvalue: spark_mir::Rvalue::Use(spark_mir::Operand::Move(spark_mir::Place::from_local(
            Local(source),
        ))),
        span: Span::DUMMY,
    };
    flow.transfer_statement(
        &move_environment(3, 4),
        Point::new(BlockId::ENTRY, 4),
        &mut state,
    );
    let first = Role::capture(Local(4), callback, 0, future);
    let second = Role::capture(Local(4), callback, 1, future);
    let expected = FxHashSet::from_iter([first.clone(), second.clone()]);
    assert!(!state.live.contains(&Local(3)));
    assert_eq!(state.future_copies.role_peers(&first), expected);
    assert_eq!(state.future_copies.role_peers(&second), expected);
    assert!(state.future_owners.known_role(&first) && state.future_owners.known_role(&second));
    flow.transfer_statement(
        &move_environment(4, 4),
        Point::new(BlockId::ENTRY, 5),
        &mut state,
    );
    assert_eq!(state.future_copies.role_peers(&first), expected);
    assert_eq!(state.future_copies.role_peers(&second), expected);
    assert!(state.future_owners.known_role(&first) && state.future_owners.known_role(&second));
}
