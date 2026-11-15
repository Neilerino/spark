// TAG [SPARK-114]

use super::*;
use crate::provenance::{
    address::OpaqueSite,
    observations::{FrameScope, Memory, Sample},
    tasks::{self, memory::Lane, views::View},
    Flow, Origin, Point, Slot, State, Value,
};
use spark_mir::{BlockId, Local, LocalDecl, MirFunction};
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::{IntWidth, LifetimeId, Ty, TypeTable};
use std::sync::Arc;

#[test]
fn storing_a_callback_node_keeps_its_borrow_source_after_rhs_consumption() {
    use crate::provenance::demand::{Atom, Coordinate, Demand};
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let future = table.intern(Ty::Future { inner: int });
    let callback_ty = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let function = MirFunction {
        def_id: DefId::new(9952),
        param_count: 0,
        return_type: int,
        locals: [int, future, reference, future]
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
    let point = Point::new(BlockId::ENTRY, 1);
    let origin = Origin::Loan(Point::new(BlockId::ENTRY, 0));
    let mut state = State::default();
    state.set(Local(2), Value::reference(origin.clone()));
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    let path = vec![Slot::Capture(DefId::new(9953), 0, reference)];
    let callback = state
        .value(Local(2))
        .sourced(&Storage::Logical(Local(2)))
        .prefixed_path(&path, callback_ty, &table);
    let (parent, mut value) = tasks::birth(function.def_id, point, future);
    value.join(&callback.task_lineage());
    let actor = Subject {
        target: parent,
        capture: None,
    };
    value
        .task_views
        .entry(Vec::new())
        .or_default()
        .insert(actor.clone(), View::pending(callback, callback_ty));
    flow.record_store(point, &spark_mir::Place::from_local(Local(1)), &value);
    state.set(Local(1), value);
    state.consume(Local(2));
    flow.record_consumed(point, Local(2));
    let storage = Storage::CallbackNode(Box::new(Location::new(
        &Storage::Logical(Local(1)),
        Vec::new(),
        actor.clone(),
        callback_ty,
    )));
    assert!(state
        .stored_value(&storage)
        .unwrap()
        .origins()
        .contains(&origin));
    let before = flow.transfers[&point].before(&Demand::from_iter([Atom {
        coordinate: Coordinate {
            storage: storage.clone(),
            path: path.clone(),
        },
        origin: origin.clone(),
        capture: None,
    }]));
    assert!(before.contains(&Atom {
        coordinate: Coordinate {
            storage: Storage::Logical(Local(2)),
            path: Vec::new()
        },
        origin: origin.clone(),
        capture: None,
    }));

    let moved_at = Point::new(BlockId::ENTRY, 2);
    flow.ensure_transfer(moved_at, &state);
    let moved = state.value(Local(1)).sourced(&Storage::Logical(Local(1)));
    flow.record_store(moved_at, &spark_mir::Place::from_local(Local(3)), &moved);
    state.set(Local(3), moved);
    state.consume(Local(1));
    flow.record_consumed(moved_at, Local(1));
    assert!(state.stored_value(&storage).is_none());
    let moved_storage = Storage::CallbackNode(Box::new(Location::new(
        &Storage::Logical(Local(3)),
        Vec::new(),
        actor,
        callback_ty,
    )));
    let moved_atom = Atom {
        coordinate: Coordinate {
            storage: moved_storage.clone(),
            path: path.clone(),
        },
        origin: origin.clone(),
        capture: None,
    };
    let prior = flow.transfers[&moved_at].before(&Demand::from_iter([moved_atom.clone()]));
    assert!(prior.contains(&Atom {
        coordinate: Coordinate { storage, path },
        origin: origin.clone(),
        capture: None
    }));
    assert!(flow.transfers[&point].before(&prior).contains(&Atom {
        coordinate: Coordinate {
            storage: Storage::Logical(Local(2)),
            path: Vec::new()
        },
        origin,
        capture: None,
    }));
    let replaced_at = Point::new(BlockId::ENTRY, 3);
    flow.ensure_transfer(replaced_at, &state);
    let replacement = tasks::birth(function.def_id, replaced_at, future).1;
    flow.record_store(
        replaced_at,
        &spark_mir::Place::from_local(Local(3)),
        &replacement,
    );
    state.set(Local(3), replacement);
    assert!(state.stored_value(&moved_storage).is_none());
    assert!(
        flow.transfers[&replaced_at]
            .before(&Demand::from_iter([moved_atom]))
            .is_empty(),
        "a fresh root must retire its old callback source coordinates"
    );
}

#[test]
fn callback_node_memory_updates_the_actual_view_without_a_catalog_row() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let future = table.intern(Ty::Future { inner: int });
    let callback_ty = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let function = MirFunction {
        def_id: DefId::new(9950),
        param_count: 0,
        return_type: int,
        locals: [int, future, reference, future]
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
    let point = Point::new(BlockId::ENTRY, 1);
    let (child_target, child) =
        tasks::birth(function.def_id, Point::new(BlockId::ENTRY, 0), future);
    let child_actor = Subject {
        target: child_target,
        capture: None,
    };
    let path = vec![Slot::Capture(DefId::new(9951), 0, future)];
    let callback = child.prefixed_path(&path, callback_ty, &table);
    let (parent, mut value) = tasks::birth(function.def_id, point, future);
    let parent_actor = Subject {
        target: parent,
        capture: None,
    };
    value
        .task_views
        .entry(Vec::new())
        .or_default()
        .insert(parent_actor.clone(), View::pending(callback, callback_ty));
    let location = Location::new(
        &Storage::Logical(Local(1)),
        Vec::new(),
        parent_actor.clone(),
        callback_ty,
    );
    let storage = Storage::CallbackNode(Box::new(location));
    let mut state = State::default();
    state.set(Local(1), value);
    state.set(Local(3), state.value(Local(1)));
    state
        .future_copies
        .assign(Local(3), state.future_copies.witness(Local(1)), &state.live);
    let mut flow = Flow::new(&function, &table, None);
    assert_eq!(flow.storage_type(&storage), callback_ty);
    assert!(state.stored_value(&storage).unwrap().futures[&path]
        .iter()
        .any(|handle| handle.pending));
    flow.ensure_transfer(point, &state);
    let lane = Arc::new(Lane {
        execution: crate::provenance::effects::Execution::Current,
        event: OpaqueSite {
            function: function.def_id,
            point,
        },
        actor: child_actor.clone(),
        location: Sample {
            memory: Memory::Frame {
                scope: FrameScope::Current,
                function: function.def_id,
                local: Local(2),
                ty: reference,
            },
            path: Vec::new(),
            ty: reference,
        },
    });
    let origin = Origin::Loan(point);
    assert!(flow.cache_pending_memory(
        &storage,
        path.clone(),
        child_actor.clone(),
        (vec![(lane, Value::reference(origin.clone()))], true, false),
        point,
        &mut state
    ));
    let stored = state.stored_value(&storage).unwrap();
    assert!(stored.task_views[&path][&child_actor]
        .memory
        .samples()
        .any(|(_, value)| value.origins().contains(&origin)));
    assert!(state.tasks.is_empty());
    let alias_storage = Storage::CallbackNode(Box::new(Location::new(
        &Storage::Logical(Local(3)),
        Vec::new(),
        parent_actor,
        callback_ty,
    )));
    assert!(!state
        .stored_value(&alias_storage)
        .unwrap()
        .task_views
        .get(&path)
        .is_some_and(|views| views
            .get(&child_actor)
            .is_some_and(|view| view.memory.may_write)));
    flow.transfer_statement(
        &spark_mir::Statement::Drop {
            place: spark_mir::Place::from_local(Local(1)),
            span: Span::DUMMY,
        },
        Point::new(BlockId::ENTRY, 2),
        &mut state,
    );
    assert!(state.stored_value(&storage).is_none());
    assert!(
        state.stored_value(&alias_storage).unwrap().task_views[&path][&child_actor]
            .memory
            .samples()
            .any(|(_, value)| value.origins().contains(&origin)),
        "dropping one exact alias must transfer its child cache to the surviving alias"
    );
    let mut current = storage;
    for _ in 0..32 {
        let nested = Location::new(&current, path.clone(), child_actor.clone(), callback_ty);
        assert_eq!(nested.root(), &Storage::Logical(Local(1)));
        current = Storage::CallbackNode(Box::new(nested));
    }
}
