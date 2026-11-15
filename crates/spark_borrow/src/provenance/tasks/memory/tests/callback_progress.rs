// TAG [SPARK-114]

use super::*;
use crate::provenance::Slot;

#[test]
fn a_nested_callback_write_keeps_its_rhs_source_after_the_local_is_consumed() {
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
    let function = function(&[int, reference, reference]);
    let point = Point::new(BlockId::ENTRY, 1);
    let origin = Origin::Loan(Point::new(BlockId::ENTRY, 0));
    let mut state = State::default();
    state.set(Local(1), Value::reference(origin.clone()));
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    let (target, mut child) = tasks::birth(function.def_id, point, future);
    let actor = Subject {
        target,
        capture: None,
    };
    let lane = lane(&function, point, &actor, Local(2));
    let mut view = tasks::views::View::unknown();
    view.memory.import(
        lane.clone(),
        state.value(Local(1)).sourced(&Storage::Logical(Local(1))),
    );
    child
        .task_views
        .entry(Vec::new())
        .or_default()
        .insert(actor, view);
    for index in 0..2 {
        let callback = child.prefixed(
            Slot::Capture(DefId::new(10011 + index), 0, future),
            callback_ty,
            &table,
        );
        let (target, mut parent) = tasks::birth(
            function.def_id,
            Point::new(BlockId::ENTRY, 2 + index),
            future,
        );
        parent.task_views.entry(Vec::new()).or_default().insert(
            Subject {
                target,
                capture: None,
            },
            tasks::views::View::pending(callback, callback_ty),
        );
        child = parent;
    }
    let mut frozen = flow.freeze_sources(point, child);
    state.consume(Local(1));
    flow.record_consumed(point, Local(1));
    flow.import_task_sources(&mut frozen, point, &mut state);
    assert!(state.pending_memory[&lane].origins().contains(&origin));
    let before = flow.transfers[&point].before(&Demand::from_iter([Atom {
        coordinate: Coordinate {
            storage: Storage::PendingWrite(lane),
            path: Vec::new(),
        },
        origin: origin.clone(),
        capture: None,
    }]));
    assert!(before.contains(&Atom {
        coordinate: Coordinate {
            storage: Storage::Logical(Local(1)),
            path: Vec::new()
        },
        origin,
        capture: None,
    }));
}
