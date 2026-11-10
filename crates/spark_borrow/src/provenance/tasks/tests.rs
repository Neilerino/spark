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
