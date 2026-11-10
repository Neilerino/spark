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
