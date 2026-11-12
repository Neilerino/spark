// TAG [SPARK-114]

use super::*;
use crate::facts::Point;
use crate::provenance::{tasks, Origin, Slot, State};
use spark_mir::BlockId;
use spark_resolve::DefId;
use spark_types::{IntWidth, Ty, TypeTable};

fn pending(table: &mut TypeTable, source: u32) -> (Target, Value, TypeId) {
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let callback = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let (target, mut value) = tasks::birth(DefId::new(9970), Point::new(BlockId::ENTRY, 0), future);
    value.task_views.entry(Vec::new()).or_default().insert(
        Subject {
            target: target.clone(),
            capture: None,
        },
        View::pending(
            Value::reference(Origin::Loan(Point::new(BlockId::ENTRY, source))),
            callback,
        ),
    );
    (target, value, callback)
}

fn completed(value: &mut Value) {
    for subjects in value.task_views.values_mut() {
        for view in subjects.values_mut() {
            *view = View::default();
        }
    }
    for handles in value.futures.values_mut() {
        *handles = handles
            .iter()
            .map(|handle| Handle {
                pending: false,
                ..handle.clone()
            })
            .collect();
    }
}

#[test]
fn a_callback_keeps_its_captured_child_view_progress() {
    let mut table = TypeTable::new();
    let (_, mut child, callback_ty) = pending(&mut table, 1);
    child
        .task_views
        .values_mut()
        .flat_map(|subjects| subjects.values_mut())
        .next()
        .unwrap()
        .memory
        .may_write = true;
    let future = child.futures[&Vec::new()]
        .iter()
        .next()
        .unwrap()
        .target
        .ty();
    let captured = child.prefixed(
        Slot::Capture(DefId::new(9968), 0, future),
        callback_ty,
        &table,
    );
    let parent = View::pending(captured.clone(), callback_ty);
    let restored = parent.callbacks[&callback_ty].value();
    assert_eq!(
        restored, captured,
        "flattening a callback must preserve its child progress and callback recipes"
    );
}

#[test]
fn recursive_callback_progress_uses_a_finite_flat_node_table() {
    let mut table = TypeTable::new();
    let (target, mut value, callback_ty) = pending(&mut table, 1);
    let future = target.ty();
    for _ in 0..32 {
        let callback = value.prefixed(
            Slot::Capture(DefId::new(9968), 0, future),
            callback_ty,
            &table,
        );
        let mut outer = tasks::birth(DefId::new(9970), Point::new(BlockId::ENTRY, 0), future).1;
        outer.task_views.entry(Vec::new()).or_default().insert(
            Subject {
                target: target.clone(),
                capture: None,
            },
            View::pending(callback, callback_ty),
        );
        value = outer;
    }
    let callback = &value.task_views[&Vec::new()]
        .values()
        .next()
        .unwrap()
        .callbacks[&callback_ty];
    assert_eq!(callback.nodes().count(), 1);
    assert_eq!(Callback::seal(callback.value()), *callback);
    let mut joined = callback.clone();
    assert!(!joined.join(callback));
}

#[test]
fn callback_node_joins_preserve_an_untracked_pending_alternative() {
    let mut table = TypeTable::new();
    let (target, known, _) = pending(&mut table, 1);
    let mut unknown = known.clone();
    unknown.task_views.clear();
    let mut done = known.clone();
    completed(&mut done);
    for (first, second) in [
        (&known, &unknown),
        (&unknown, &known),
        (&done, &unknown),
        (&unknown, &done),
    ] {
        let mut callback = Callback::seal(first.clone());
        callback.join(&Callback::seal(second.clone()));
        let merged = callback.value();
        assert!(
            merged.task_views[&Vec::new()][&Subject {
                target: target.clone(),
                capture: None
            }]
                .fallback
        );
    }
}

#[test]
fn exact_peer_progress_keeps_the_source_views_fallback() {
    let mut table = TypeTable::new();
    let (target, mut destination, _) = pending(&mut table, 1);
    let mut source = destination.clone();
    let subject = Subject {
        target,
        capture: None,
    };
    source
        .task_views
        .get_mut(&Vec::new())
        .unwrap()
        .get_mut(&subject)
        .unwrap()
        .fallback = true;
    destination.merge_task_progress(&source);
    assert!(destination.task_views[&Vec::new()][&subject].fallback);
}

#[test]
fn pending_memory_views_include_execution_backing_without_a_catalog_entry() {
    let mut table = TypeTable::new();
    let (_, mut child, callback_ty) = pending(&mut table, 1);
    child
        .task_views
        .values_mut()
        .flat_map(|views| views.values_mut())
        .next()
        .unwrap()
        .memory
        .may_write = true;
    let mut state = State::default();
    let future = child.futures[&Vec::new()]
        .iter()
        .next()
        .unwrap()
        .target
        .ty();
    state.executions.insert(
        (spark_mir::Local(1), callback_ty),
        child.prefixed(
            Slot::Capture(DefId::new(9969), 0, future),
            callback_ty,
            &table,
        ),
    );
    assert!(state.tasks.is_empty());
    assert!(state
        .rooted_activity()
        .views()
        .any(|view| view.memory.may_write));
}

#[test]
fn a_new_current_contribution_does_not_query_an_old_weak_callback() {
    let mut table = TypeTable::new();
    let (target, old, ty) = pending(&mut table, 1);
    let (_, new, _) = pending(&mut table, 2);
    let mut weak = old.task_views[&Vec::new()]
        .values()
        .next()
        .unwrap()
        .callbacks[&ty]
        .value();
    weak.join(
        &new.task_views[&Vec::new()]
            .values()
            .next()
            .unwrap()
            .callbacks[&ty]
            .value(),
    );
    let mut state = State::default();
    state.tasks.insert(
        target,
        tasks::Record {
            callback: weak,
            callback_ty: ty,
        },
    );
    let handle = new.futures[&Vec::new()].iter().next().unwrap();
    let (records, unknown) = pending_records(&new, &[], handle, &state);
    assert!(!unknown);
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].callback.origins(),
        rustc_hash::FxHashSet::from_iter([Origin::Loan(Point::new(BlockId::ENTRY, 2))])
    );
}

#[test]
fn joining_a_completed_contribution_preserves_only_the_active_callback() {
    let mut table = TypeTable::new();
    let (_, mut old, ty) = pending(&mut table, 1);
    completed(&mut old);
    let (_, new, _) = pending(&mut table, 2);
    old.join(&new);
    let handle = old.futures[&Vec::new()].iter().find(|h| h.pending).unwrap();
    let (records, unknown) = pending_records(&old, &[], handle, &State::default());
    assert!(!unknown);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].callback_ty, ty);
    assert_eq!(
        records[0].callback.origins(),
        rustc_hash::FxHashSet::from_iter([Origin::Loan(Point::new(BlockId::ENTRY, 2))])
    );
    assert!(!old.join(&old.clone()));
}

#[test]
fn an_untracked_pending_alternative_cannot_be_hidden_by_a_completed_view() {
    let mut table = TypeTable::new();
    let (target, mut known, ty) = pending(&mut table, 1);
    let mut unknown = known.clone();
    unknown.task_views.clear();
    completed(&mut known);
    for (mut left, right) in [(known.clone(), unknown.clone()), (unknown, known)] {
        left.join(&right);
        let handle = left.futures[&Vec::new()]
            .iter()
            .find(|h| h.pending)
            .unwrap();
        let mut state = State::default();
        state.tasks.insert(
            target.clone(),
            tasks::Record {
                callback: Value::reference(Origin::Loan(Point::new(BlockId::ENTRY, 9))),
                callback_ty: ty,
            },
        );
        let (records, unproved) = pending_records(&left, &[], handle, &state);
        assert!(!unproved);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].callback.origins(),
            rustc_hash::FxHashSet::from_iter([Origin::Loan(Point::new(BlockId::ENTRY, 9))])
        );
    }
}

#[test]
fn task_views_follow_bounded_capture_paths_and_capture_qualification() {
    let mut table = TypeTable::new();
    let (_, mut value, ty) = pending(&mut table, 1);
    let capture = Capture {
        owner: DefId::new(9971),
        index: 0,
        ty: value.futures[&Vec::new()]
            .iter()
            .next()
            .unwrap()
            .target
            .ty(),
    };
    value = value.with_future_capture(capture);
    for _ in 0..32 {
        value = value.prefixed(Slot::Capture(capture.owner, capture.index, ty), ty, &table);
    }
    assert!(value.task_views.keys().all(|path| path.len() <= 2));
    let selected = value.capture(capture.owner, capture.index);
    assert!(!selected.futures.is_empty());
    for (path, handles) in &selected.futures {
        for handle in handles {
            assert_eq!(handle.capture, Some(capture));
            let (records, unknown) = pending_records(&selected, path, handle, &State::default());
            assert!(!unknown);
            assert_eq!(records.len(), 1);
        }
    }
}

#[test]
fn widening_distinct_paths_preserves_an_untracked_pending_alternative() {
    for complete_known in [true, false] {
        let mut table = TypeTable::new();
        let (target, mut known, ty) = pending(&mut table, 1);
        let mut unknown = known.clone();
        unknown.task_views.clear();
        if complete_known {
            completed(&mut known);
        }
        let future = target.ty();
        let pair = table.intern(Ty::Tuple(vec![future, future]));
        let mut aggregate = known.prefixed(Slot::Tuple(0), pair, &table);
        aggregate.join(&unknown.prefixed(Slot::Tuple(1), pair, &table));
        let widened = aggregate.prefixed(Slot::Tail, ty, &table);
        let path = vec![Slot::Tail];
        let handle = widened.futures[&path].iter().find(|h| h.pending).unwrap();
        let mut state = State::default();
        state.tasks.insert(
            target,
            tasks::Record {
                callback: Value::reference(Origin::Loan(Point::new(BlockId::ENTRY, 9))),
                callback_ty: ty,
            },
        );
        let (records, unknown) = pending_records(&widened, &path, handle, &state);
        assert!(!unknown);
        let origins: rustc_hash::FxHashSet<_> = records
            .iter()
            .flat_map(|record| record.callback.origins())
            .collect();
        let expected = if complete_known { vec![9] } else { vec![1, 9] };
        assert_eq!(
            origins,
            expected
                .into_iter()
                .map(|index| Origin::Loan(Point::new(BlockId::ENTRY, index)))
                .collect(),
            "{complete_known}"
        );
    }
}

#[test]
fn a_strong_borrowed_field_replacement_detaches_the_old_task_view() {
    use crate::provenance::{address::Endpoint, Flow};
    use spark_mir::{Local, LocalDecl, MirFunction};
    use spark_span::Span;
    let mut table = TypeTable::new();
    let (_, old, _) = pending(&mut table, 1);
    let (_, new, _) = pending(&mut table, 2);
    let future = new.futures[&Vec::new()].iter().next().unwrap().target.ty();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let field = spark_intern::sym("pending");
    let holder_id = DefId::new(9973);
    table.set_struct_fields(holder_id, vec![(field, future)]);
    let holder = table.intern(Ty::Struct(holder_id));
    let function = MirFunction {
        def_id: DefId::new(9974),
        param_count: 0,
        return_type: int,
        locals: [int, holder, future]
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
    let mut state = State::default();
    state.set(Local(1), old.prefixed(Slot::Field(field), holder, &table));
    state.set(Local(2), old);
    let point = Point::new(BlockId::ENTRY, 3);
    let destination = Leaf {
        endpoints: rustc_hash::FxHashSet::from_iter([Endpoint::Frame {
            lifetime: crate::provenance::address::FrameLifetime::Local,
            function: function.def_id,
            root: Point::new(BlockId::ENTRY, 0),
            local: Local(1),
            root_ty: holder,
            pointee: future,
            location: vec![Slot::Field(field)],
        }]),
        ..Leaf::default()
    };
    let mut flow = Flow::new(&function, &table, None);
    flow.store_references(&destination, &new, future, point, &mut state);
    let selected = state.value(Local(1)).project(&Slot::Field(field));
    let handle = selected.futures[&Vec::new()].iter().next().unwrap();
    let (records, unknown) = pending_records(&selected, &[], handle, &state);
    assert!(!unknown);
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].callback.origins(),
        rustc_hash::FxHashSet::from_iter([Origin::Loan(Point::new(BlockId::ENTRY, 2))])
    );
    let retained = state.value(Local(2));
    let retained_handle = retained.futures[&Vec::new()].iter().next().unwrap();
    let (retained_records, _) = pending_records(&retained, &[], retained_handle, &state);
    assert_eq!(
        retained_records[0].callback.origins(),
        rustc_hash::FxHashSet::from_iter([Origin::Loan(Point::new(BlockId::ENTRY, 1))])
    );
}

#[test]
fn capture_requalification_preserves_an_untracked_pending_alternative() {
    let mut table = TypeTable::new();
    let (target, mut known, ty) = pending(&mut table, 1);
    let mut unknown = known.clone();
    unknown.task_views.clear();
    completed(&mut known);
    let capture = Capture {
        owner: DefId::new(9975),
        index: 0,
        ty: target.ty(),
    };
    let mut mixed = known.with_future_capture(capture);
    mixed.join(&unknown.with_future_capture(Capture {
        index: 1,
        ..capture
    }));
    let requalified = mixed.with_future_capture(Capture {
        owner: DefId::new(9976),
        ..capture
    });
    let handle = requalified.futures[&Vec::new()]
        .iter()
        .find(|h| h.pending)
        .unwrap();
    let mut state = State::default();
    state.tasks.insert(
        target,
        tasks::Record {
            callback: Value::reference(Origin::Loan(Point::new(BlockId::ENTRY, 9))),
            callback_ty: ty,
        },
    );
    let (records, unknown) = pending_records(&requalified, &[], handle, &state);
    assert!(!unknown);
    assert_eq!(
        records.len(),
        1,
        "renaming a capture must retain the untracked active alternative"
    );
}
