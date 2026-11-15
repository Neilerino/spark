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
            frame_caller: None,
            read: crate::provenance::observations::ReadView::current(&state),
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

#[test]
fn callable_resolution_reads_the_task_envelope_and_retained_cells() {
    use crate::provenance::{
        address::Endpoint,
        demand::Storage,
        observations::{Envelope, History, ReadView},
        value::{Leaf, ReferenceCapture},
        Origin, Slot,
    };
    use rustc_hash::FxHashSet;
    use spark_types::LifetimeId;
    use std::cell::RefCell;
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let callable = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let reference = table.intern(Ty::Ref {
        inner: callable,
        mutable: true,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let future = table.intern(Ty::Future { inner: int });
    let function = |id, argument| MirFunction {
        def_id: DefId::new(id),
        param_count: 1,
        return_type: int,
        locals: [int, argument]
            .into_iter()
            .enumerate()
            .map(|(i, ty)| LocalDecl {
                local: Local(i as u32),
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
    let caller = function(9981, callable);
    let worker = function(9982, reference);
    let mut flow = Flow::new(&caller, &table, None);
    let point = Point::new(BlockId::ENTRY, 0);
    let local = Local(1);
    let capture = ReferenceCapture {
        owner: worker.def_id,
        index: 0,
        ty: reference,
    };
    let pointer = Value::reference(Origin::Loan(point))
        .with_metadata(&Leaf {
            endpoints: FxHashSet::from_iter([Endpoint::Frame {
                lifetime: crate::provenance::address::FrameLifetime::Local,
                function: caller.def_id,
                root: point,
                local,
                root_ty: callable,
                pointee: callable,
                location: Vec::new(),
            }]),
            ..Leaf::default()
        })
        .with_reference_capture(capture);
    let actor = Subject {
        target: crate::provenance::tasks::birth(caller.def_id, point, future).0,
        capture: None,
    };
    let arguments = [pointer];
    let callee = Value::default();
    let (current, temporary, replacement) = (DefId::new(9983), DefId::new(9984), DefId::new(9985));
    let mut state = State::default();
    state.set(local, Value::function(current));
    let envelope = Envelope::from_iter([(Storage::Cell(local), Value::function(temporary))]);
    let mut history = History::default();
    let trace = RefCell::default();
    let read = ReadView::task(&state, &actor, &history, &trace).with_envelope(&envelope);
    let during = flow.callback_inputs(
        &worker,
        CallActuals {
            callee: &callee,
            arguments: &arguments,
            ty: int,
            point,
            read: read,
            instantiation_site: None,
            frame_caller: None,
            aliases: None,
        },
    );
    let qualified = (local, Vec::new(), callable, Some(capture));
    let unqualified = (local, vec![Slot::Deref], callable, None);
    for slot in [&qualified, &unqualified] {
        assert_eq!(
            during.slots[slot].known,
            FxHashSet::from_iter([current, temporary])
        );
    }
    flow.ensure_transfer(point, &state);
    assert!(flow.remember_samples(trace.into_inner(), &mut history, point, &mut state));
    state.set(local, Value::function(replacement));
    let trace = RefCell::default();
    let retained = flow.callback_inputs(
        &worker,
        CallActuals {
            callee: &callee,
            arguments: &arguments,
            ty: int,
            point,
            read: ReadView::task(&state, &actor, &history, &trace),
            instantiation_site: None,
            frame_caller: None,
            aliases: None,
        },
    );
    for slot in [&qualified, &unqualified] {
        assert_eq!(
            retained.slots[slot].known,
            FxHashSet::from_iter([current, temporary, replacement])
        );
    }
}
