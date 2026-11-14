// TAG [SPARK-114]

use super::*;
use crate::facts::Point;
use crate::provenance::{
    demand::{Atom, Demand},
    Origin,
};
use spark_mir::{BlockId, Local, LocalDecl, MirFunction};
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::{IntWidth, LifetimeId, Ty, TypeTable};

#[test]
fn a_completed_parent_does_not_keep_its_catalog_capture_active() {
    use crate::provenance::tasks::{birth, Record};
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let callback_ty = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let function = MirFunction {
        def_id: DefId::new(9960),
        param_count: 0,
        return_type: int,
        locals: [int, future]
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
    let (parent, mut completed) = birth(function.def_id, Point::new(BlockId::ENTRY, 1), future);
    let (_, child) = birth(function.def_id, Point::new(BlockId::ENTRY, 0), future);
    let callback = child.prefixed(
        Slot::Capture(DefId::new(9961), 0, future),
        callback_ty,
        &table,
    );
    completed.mark_future_completed();
    let mut state = State::default();
    state.set(Local(1), completed);
    state.tasks.insert(
        parent,
        Record {
            callback,
            callback_ty,
        },
    );
    let flow = Flow::new(&function, &table, None);
    assert!(
        !flow.task_carriers(&state).iter().any(|(_, _, value)| {
            value
                .futures
                .values()
                .flatten()
                .any(|handle| handle.pending)
        }),
        "a recipe catalog is not an independent execution owner"
    );
}

#[test]
fn an_unadopted_pending_parent_keeps_its_callback_child_reachable() {
    use crate::provenance::tasks::{birth, views::View};
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let callback_ty = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let function = activity_function(int, future);
    let (parent, mut pending) = birth(function.def_id, Point::new(BlockId::ENTRY, 1), future);
    let (child_target, child) = birth(function.def_id, Point::new(BlockId::ENTRY, 0), future);
    let callback = child.prefixed(
        Slot::Capture(DefId::new(9961), 0, future),
        callback_ty,
        &table,
    );
    pending.task_views.entry(Vec::new()).or_default().insert(
        Subject {
            target: parent,
            capture: None,
        },
        View::pending(callback, callback_ty),
    );
    let mut state = State::default();
    state.set(Local(1), pending);
    assert!(state.executions.is_empty());
    assert!(state.tasks.is_empty());
    let flow = Flow::new(&function, &table, None);
    assert!(flow.task_carriers(&state).iter().any(|(_, _, value)| value
        .futures
        .values()
        .flatten()
        .any(|handle| handle.pending && handle.target == child_target)));
}

#[test]
fn a_pending_residual_keeps_its_bound_callback_child_reachable() {
    use crate::provenance::{
        address::OpaqueSite,
        tasks::{birth, residual},
    };
    use std::sync::Arc;
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let callback_ty = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let function = activity_function(int, future);
    let point = Point::new(BlockId::ENTRY, 1);
    let (parent, pending) = birth(function.def_id, point, future);
    let (child_target, child) = birth(function.def_id, Point::new(BlockId::ENTRY, 0), future);
    let callback = child.prefixed(
        Slot::Capture(DefId::new(9961), 0, future),
        callback_ty,
        &table,
    );
    let key = Arc::new(residual::Key {
        site: OpaqueSite {
            function: function.def_id,
            point,
        },
        role: Vec::new(),
        ty: future,
        actor: Subject {
            target: parent,
            capture: None,
        },
    });
    let mut state = State::default();
    state.residuals.insert(key.clone(), pending);
    state
        .residual_callbacks
        .insert((key, callback_ty), callback);
    assert!(state.tasks.is_empty());
    let flow = Flow::new(&function, &table, None);
    assert!(flow.task_carriers(&state).iter().any(|(_, _, value)| value
        .futures
        .values()
        .flatten()
        .any(|handle| handle.pending && handle.target == child_target)));
}

fn activity_function(int: TypeId, future: TypeId) -> MirFunction {
    MirFunction {
        def_id: DefId::new(9960),
        param_count: 0,
        return_type: int,
        locals: [int, future]
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
    }
}

#[test]
fn cached_result_source_annotations_do_not_count_as_persistent_data_growth() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let future = table.intern(Ty::Future { inner: reference });
    let function = MirFunction {
        def_id: DefId::new(9972),
        param_count: 0,
        return_type: int,
        locals: [int, reference, future]
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
    state.set(Local(1), Value::reference(origin.clone()));
    state.set(
        Local(2),
        crate::provenance::tasks::birth(function.def_id, point, future).1,
    );
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    let result = flow.freeze_sources(
        point,
        state.value(Local(1)).sourced(&Storage::Logical(Local(1))),
    );
    assert!(flow.cache_task_result(
        &Storage::Logical(Local(2)),
        future,
        Vec::new(),
        &result,
        point,
        &mut state
    ));
    let stable = flow.transfers[&point].clone();
    assert!(!flow.cache_task_result(
        &Storage::Logical(Local(2)),
        future,
        Vec::new(),
        &result,
        point,
        &mut state
    ));
    assert_eq!(flow.transfers[&point], stable);
    assert!(state.value(Local(2)).sources.is_empty());
    let after = Atom {
        coordinate: Coordinate {
            storage: Storage::Logical(Local(2)),
            path: vec![Slot::Element],
        },
        origin: origin.clone(),
        capture: None,
    };
    let before = Atom {
        coordinate: Coordinate {
            storage: Storage::Logical(Local(1)),
            path: Vec::new(),
        },
        origin,
        capture: None,
    };
    assert_eq!(
        flow.transfers[&point].before(&Demand::from_iter([after])),
        Demand::from_iter([before])
    );
}

fn readonly_parent_with_borrowed_child() -> (TypeTable, MirFunction, State, Target, Origin) {
    use crate::provenance::tasks::birth;
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let callback_ty = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let function = activity_function(int, future);
    let (child_target, mut child) = birth(function.def_id, Point::new(BlockId::ENTRY, 0), future);
    let origin = Origin::Loan(Point::new(BlockId::ENTRY, 0));
    let mut borrowed = Value::function(DefId::new(9962));
    borrowed.join(&Value::reference(origin.clone()).task_lineage());
    child.join(&borrowed);
    child.task_views.entry(Vec::new()).or_default().insert(
        Subject {
            target: child_target.clone(),
            capture: None,
        },
        views::View::pending(borrowed, callback_ty),
    );
    let mut callback = Value::function(DefId::new(9961));
    callback.join(&child.prefixed(
        Slot::Capture(DefId::new(9961), 0, future),
        callback_ty,
        &table,
    ));
    let (parent, mut value) = birth(function.def_id, Point::new(BlockId::ENTRY, 1), future);
    value.join(&callback.task_lineage());
    value.task_views.entry(Vec::new()).or_default().insert(
        Subject {
            target: parent,
            capture: None,
        },
        views::View::pending(callback, callback_ty),
    );
    let mut state = State::default();
    state.set(Local(1), value);
    (table, function, state, child_target, origin)
}

#[test]
fn losing_a_readonly_parent_preserves_its_pending_callback_child_without_a_catalog() {
    let (table, function, mut state, child, origin) = readonly_parent_with_borrowed_child();
    let mut flow = Flow::new(&function, &table, None);
    flow.transfer_statement(
        &spark_mir::Statement::Drop {
            place: spark_mir::Place::from_local(Local(1)),
            span: Span::DUMMY,
        },
        Point::new(BlockId::ENTRY, 2),
        &mut state,
    );
    assert!(state.tasks.is_empty());
    assert!(
        flow.task_carriers(&state).iter().any(|(_, _, value)| value
            .futures
            .values()
            .flatten()
            .any(|handle| handle.pending && handle.target == child)
            && value.origins().contains(&origin)),
        "readonly parents must conserve child activity and borrowed source support on loss"
    );
}

#[test]
fn completing_an_unadopted_parent_preserves_children_before_clearing_its_view() {
    let (table, function, mut state, child, origin) = readonly_parent_with_borrowed_child();
    let point = Point::new(BlockId::ENTRY, 2);
    let mut flow = Flow::new(&function, &table, None);
    flow.complete_future(Local(1), &Value::default(), point, &mut state);
    assert!(state.value(Local(1)).futures[&Vec::new()]
        .iter()
        .all(|handle| !handle.pending));
    let carriers = flow.task_carriers(&state);
    let (storage, _, value) = carriers
        .iter()
        .find(|(_, _, value)| {
            value
                .futures
                .values()
                .flatten()
                .any(|handle| handle.pending && handle.target == child)
                && value.origins().contains(&origin)
        })
        .expect("parent completion does not prove child completion");
    let path = value
        .leaves
        .keys()
        .find(|(_, candidate, _)| *candidate == origin)
        .unwrap()
        .0
        .clone();
    let prior = flow.transfers[&point].before(&Demand::from_iter([Atom {
        coordinate: Coordinate {
            storage: storage.clone(),
            path,
        },
        origin,
        capture: None,
    }]));
    assert!(
        prior.iter().any(|atom| matches!(&atom.coordinate.storage,
        Storage::CallbackNode(location) if location.root() == &Storage::Logical(Local(1)))),
        "descendant sources must freeze before the completed parent view is cleared"
    );
}

#[test]
fn memory_owned_parents_keep_full_callback_progress_on_read_and_commit() {
    use crate::provenance::{
        address::OpaqueSite,
        observations::{FrameScope, Memory, Sample},
        tasks::{birth, memory::Lane},
    };
    let (table, mut function, mut state, child, _) = readonly_parent_with_borrowed_child();
    let future = function.locals[1].ty;
    function.locals.push(LocalDecl {
        local: Local(2),
        ty: future,
        name: None,
        is_user_variable: false,
        mutable: true,
        span: Span::DUMMY,
    });
    let parent = state.value(Local(1));
    let callback_ty = parent
        .task_views
        .values()
        .flat_map(|subjects| subjects.values())
        .flat_map(|view| view.callbacks.keys())
        .copied()
        .next()
        .unwrap();
    let point = Point::new(BlockId::ENTRY, 5);
    let (writer, mut writing) = birth(function.def_id, point, future);
    let actor = Subject {
        target: writer,
        capture: None,
    };
    let sample = Sample {
        memory: Memory::Frame {
            scope: FrameScope::Current,
            function: function.def_id,
            local: Local(2),
            ty: future,
        },
        path: Vec::new(),
        ty: future,
    };
    let lane = std::sync::Arc::new(Lane {
        execution: crate::provenance::effects::Execution::Current,
        event: OpaqueSite {
            function: function.def_id,
            point,
        },
        actor: actor.clone(),
        location: sample,
    });
    let mut view = views::View::pending(Value::function(DefId::new(9963)), callback_ty);
    view.memory.import(lane.clone(), parent.clone());
    writing
        .task_views
        .entry(Vec::new())
        .or_default()
        .insert(actor, view);
    state.set(Local(1), writing);
    state.pending_memory.insert(lane.clone(), parent);
    let mut flow = Flow::new(&function, &table, None);
    assert!(flow.task_carriers(&state).iter().any(|(_, _, value)| value
        .futures
        .values()
        .flatten()
        .any(|handle| handle.pending && handle.target == child)));
    let read = flow.async_memory(&Storage::Cell(Local(2)), future, &state);
    assert!(
        read.task_views
            .values()
            .flat_map(|subjects| subjects.values())
            .flat_map(|view| view.callbacks.values())
            .any(|callback| callback
                .value()
                .futures
                .values()
                .flatten()
                .any(|handle| handle.pending && handle.target == child)),
        "reading a sampled Future must keep its full callback graph"
    );
    flow.complete_future(
        Local(1),
        &Value::default(),
        Point::new(BlockId::ENTRY, 6),
        &mut state,
    );
    assert!(flow.task_carriers(&state).iter().any(|(storage, _, value)| matches!(storage, Storage::CallbackNode(location) if matches!(location.root(), Storage::SettledMemory(_)))
        && value.futures.values().flatten().any(|handle| handle.pending && handle.target == child)),
        "committed Future payloads remain owning activity roots after their writer completes");
    let mut inactive = State::default();
    inactive.pending_memory = state.pending_memory.clone();
    assert!(
        inactive.rooted_activity().into_carriers().next().is_none(),
        "stale pending lanes are readable backing, not autonomous activity"
    );
}

#[test]
fn active_memory_samples_do_not_revive_older_same_lane_futures() {
    use crate::provenance::{
        address::OpaqueSite,
        observations::{FrameScope, Memory, Sample},
        tasks::{birth, memory::Lane},
    };
    let (table, function, mut state, child, _) = readonly_parent_with_borrowed_child();
    let future = function.locals[1].ty;
    let parent = state.value(Local(1));
    let callback_ty = *parent
        .task_views
        .values()
        .flat_map(|subjects| subjects.values())
        .flat_map(|view| view.callbacks.keys())
        .next()
        .unwrap();
    let point = Point::new(BlockId::ENTRY, 8);
    let (writer, mut writing) = birth(function.def_id, point, future);
    let actor = Subject {
        target: writer,
        capture: None,
    };
    let lane = std::sync::Arc::new(Lane {
        execution: crate::provenance::effects::Execution::Current,
        event: OpaqueSite {
            function: function.def_id,
            point,
        },
        actor: actor.clone(),
        location: Sample {
            memory: Memory::Frame {
                scope: FrameScope::Current,
                function: function.def_id,
                local: Local(1),
                ty: future,
            },
            path: Vec::new(),
            ty: future,
        },
    });
    let mut view = views::View::pending(Value::function(DefId::new(9963)), callback_ty);
    view.memory.import(lane.clone(), parent.clone());
    writing
        .task_views
        .entry(Vec::new())
        .or_default()
        .insert(actor, view);
    state.set(Local(1), writing);
    let (old, older) = birth(function.def_id, Point::new(BlockId::ENTRY, 9), future);
    let mut backing = parent;
    backing.join(&older);
    state.pending_memory.insert(lane, backing);
    let flow = Flow::new(&function, &table, None);
    let active: Vec<_> = state.rooted_activity().into_carriers().collect();
    assert!(active.iter().any(|(_, value)| value
        .futures
        .values()
        .flatten()
        .any(|handle| handle.pending && handle.target == child)));
    assert!(
        !active.iter().any(|(_, value)| value
            .futures
            .values()
            .flatten()
            .any(|handle| handle.pending && handle.target == old)),
        "a precise writer sample cannot revive another occurrence from its lane backing"
    );
    let read = flow.async_memory(&Storage::Cell(Local(1)), future, &state);
    assert!(!read
        .futures
        .values()
        .flatten()
        .any(|handle| handle.target == old));
}

#[test]
fn completing_a_capture_only_parent_preserves_its_unadopted_grandchild() {
    use crate::provenance::tasks::{birth, Capture, Role};
    let (table, mut function, mut state, grandchild, origin) =
        readonly_parent_with_borrowed_child();
    let future = function.locals[1].ty;
    function.locals.push(LocalDecl {
        local: Local(2),
        ty: future,
        name: None,
        is_user_variable: false,
        mutable: false,
        span: Span::DUMMY,
    });
    let child = state.value(Local(1));
    let callback_ty = *child
        .task_views
        .values()
        .flat_map(|subjects| subjects.values())
        .flat_map(|view| view.callbacks.keys())
        .next()
        .unwrap();
    let point = Point::new(BlockId::ENTRY, 12);
    let (_, keeper) = birth(function.def_id, point, future);
    state.set(Local(2), child.clone());
    state.set(Local(1), keeper);
    let capture = Capture {
        owner: DefId::new(9964),
        index: 0,
        ty: future,
    };
    let path = vec![Slot::Capture(capture.owner, capture.index, future)];
    let role = Role::capture(Local(1), capture.owner, capture.index, future)
        .in_execution(Local(1), callback_ty)
        .unwrap();
    state.future_copies.assign_role(
        role.clone(),
        state.future_copies.witness(Local(2)),
        &state.live,
    );
    state.executions.insert(
        (Local(1), callback_ty),
        child
            .with_future_capture(capture)
            .prefixed_path(&path, callback_ty, &table),
    );
    state.consume(Local(2));
    let witness = state.future_copies.role_witness(&role);
    assert!(witness.locals().next().is_none());
    let mut flow = Flow::new(&function, &table, None);
    flow.complete_future_witness(witness, None, point, &mut state);
    assert!(
        flow.current_role_value(&role, &state).unwrap().futures[&Vec::new()]
            .iter()
            .all(|handle| !handle.pending)
    );
    assert!(
        flow.task_carriers(&state)
            .iter()
            .any(|(storage, _, value)| matches!(
                storage,
                Storage::Residual(_) | Storage::ResidualCallback(_, _)
            ) && value
                .futures
                .values()
                .flatten()
                .any(|handle| handle.pending && handle.target == grandchild)
                && value.origins().contains(&origin)),
        "a capture-only completion must conserve the child's still-pending descendants"
    );
}

#[test]
fn fallback_imports_rebuild_activity_after_leafless_memory_growth() {
    use crate::provenance::{
        address::OpaqueSite,
        observations::{FrameScope, Memory, Sample},
        tasks::{birth, memory::Lane, Record},
    };
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let callback_ty = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let function = activity_function(int, future);
    let point = Point::new(BlockId::ENTRY, 15);
    let (child, payload) = birth(function.def_id, Point::new(BlockId::ENTRY, 16), future);
    let (writer, mut callback) = birth(function.def_id, Point::new(BlockId::ENTRY, 17), future);
    let actor = Subject {
        target: writer,
        capture: None,
    };
    let lane = std::sync::Arc::new(Lane {
        execution: crate::provenance::effects::Execution::Current,
        event: OpaqueSite {
            function: function.def_id,
            point,
        },
        actor: actor.clone(),
        location: Sample {
            memory: Memory::Frame {
                scope: FrameScope::Current,
                function: function.def_id,
                local: Local(1),
                ty: future,
            },
            path: Vec::new(),
            ty: future,
        },
    });
    let mut view = views::View::pending(Value::function(DefId::new(9965)), callback_ty);
    view.memory.import(lane, payload);
    callback
        .task_views
        .entry(Vec::new())
        .or_default()
        .insert(actor, view);
    let (parent, mut root) = birth(function.def_id, point, future);
    let mut resident = callback.clone();
    resident.clear_task_imports();
    let mut view = views::View::pending(resident, callback_ty);
    view.fallback = true;
    root.task_views.entry(Vec::new()).or_default().insert(
        Subject {
            target: parent.clone(),
            capture: None,
        },
        view,
    );
    let mut state = State::default();
    state.set(Local(1), root);
    state.tasks.insert(
        parent,
        Record {
            callback,
            callback_ty,
        },
    );
    assert!(state.pending_memory.is_empty());
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    let activity = flow.prepare_task_activity(point, &mut state);
    assert!(!state.pending_memory.is_empty());
    assert!(
        activity.into_carriers().any(|(storage, value)| matches!(
            storage,
            Storage::PendingWrite(_)
        ) && value
            .futures
            .values()
            .flatten()
            .any(|handle| handle.pending && handle.target == child)),
        "memory imports without reference leaves still change the rooted closure"
    );
}
