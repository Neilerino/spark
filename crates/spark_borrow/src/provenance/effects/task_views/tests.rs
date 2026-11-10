// TAG [SPARK-114]

use super::*;
use crate::provenance::{
    address::OpaqueSite,
    tasks::{views::pending_records, Handle},
    Point, State,
};
use spark_mir::{BlockId, Local, LocalDecl};
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::{IntWidth, Ty, TypeTable};

#[test]
fn binding_collapsed_targets_preserves_an_untracked_pending_alternative() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let function = MirFunction {
        def_id: DefId::new(9979),
        param_count: 0,
        return_type: future,
        locals: vec![LocalDecl {
            local: Local(0),
            ty: future,
            name: None,
            is_user_variable: false,
            mutable: false,
            span: Span::DUMMY,
        }],
        blocks: Vec::new(),
        captures: Vec::new(),
        is_async: false,
        span: Span::DUMMY,
    };
    let target = |owner| Target::Unknown {
        site: OpaqueSite {
            function: DefId::new(owner),
            point: Point::new(BlockId::ENTRY, 0),
        },
        ty: future,
    };
    let completed = Handle {
        target: target(9977),
        pending: false,
        capture: None,
    };
    let untracked = Handle {
        target: target(9978),
        pending: true,
        capture: None,
    };
    let mut summary = Value::default();
    summary
        .futures
        .entry(Vec::new())
        .or_default()
        .extend([completed.clone(), untracked]);
    summary
        .task_views
        .entry(Vec::new())
        .or_default()
        .insert(Subject::from(&completed), View::default());
    let mut flow = Flow::new(&function, &table, None);
    let state = State::default();
    let callee = Value::default();
    let bound = flow.bind(
        &summary,
        &function,
        CallActuals {
            callee: &callee,
            arguments: &[],
            ty: future,
            point: Point::new(BlockId::ENTRY, 17),
            instantiation_site: None,
            state: &state,
            aliases: None,
        },
    );
    let handle = bound.futures[&Vec::new()]
        .iter()
        .find(|h| h.pending)
        .unwrap();
    let (records, unknown) = pending_records(&bound, &[], handle, &state);
    assert!(records.is_empty());
    assert!(
        unknown,
        "a completed target cannot suppress the untracked target collapsed into its caller class"
    );
}
