// TAG [SPARK-114]

use super::*;
use crate::provenance::{
    address::{Intent, Selector},
    demand::{Atom, Demand},
    tasks, Origin,
};
use rustc_hash::FxHashSet;
use spark_mir::{BlockId, LocalDecl, MirFunction, Place};
use spark_span::Span;
use spark_types::{IntWidth, LifetimeId, Ty, TypeTable};

#[test]
fn empty_read_support_resamples_and_survives_clear_with_its_original_source() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let channel = table.intern(Ty::Channel { inner: reference });
    let future = table.intern(Ty::Future { inner: int });
    let function = MirFunction {
        def_id: DefId::new(9980),
        param_count: 0,
        return_type: int,
        locals: [int, reference]
            .into_iter()
            .enumerate()
            .map(|(i, ty)| LocalDecl {
                local: Local(i as u32),
                ty,
                name: None,
                mutable: true,
                is_user_variable: false,
                span: Span::DUMMY,
            })
            .collect(),
        blocks: vec![],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let start = Point::new(BlockId::ENTRY, 0);
    let update = Point::new(BlockId::ENTRY, 1);
    let local = Local(1);
    let (target, _) = tasks::birth(function.def_id, start, future);
    let actor = Subject {
        target,
        capture: None,
    };
    let selector = Selector {
        input: local,
        selection: Vec::new(),
        intent: Intent::Value,
        expected: Some(reference),
        capture: None,
        channel_capture: None,
        queue: None,
    };
    let sample = Sample {
        memory: Memory::Frame {
            function: function.def_id,
            local,
            ty: reference,
        },
        path: Vec::new(),
        ty: reference,
    };
    let mut state = State::default();
    let mut flow = Flow::new(&function, &table, None);
    let mut history = History::default();
    let trace = RefCell::default();
    let first = ReadView::task(&state, &actor, &history, &trace)
        .select(function.def_id, &selector)
        .observe(sample.clone(), Value::default());
    assert!(first.is_empty());
    flow.ensure_transfer(start, &state);
    assert!(flow.remember_samples(trace.into_inner(), &mut history, start, &mut state));
    assert!(
        state.atoms().is_empty(),
        "empty support is not a fabricated reference source"
    );

    let origin = Origin::Loan(start);
    state.set(local, Value::reference(origin.clone()));
    flow.ensure_transfer(update, &state);
    let samples = flow.resample_history(&history, ReadView::current(&state));
    assert!(flow.remember_samples(samples, &mut history, update, &mut state));
    flow.store(
        &Place::from_local(local),
        Value::default(),
        update,
        &mut state,
    );
    assert!(state.cell(local).is_empty());
    assert!(
        flow.transfers[&update].seeds.is_empty(),
        "retaining data is not itself a use"
    );

    let trace = RefCell::default();
    let current = state.cell(local).sourced(&Storage::Cell(local));
    let retained = ReadView::task(&state, &actor, &history, &trace)
        .select(function.def_id, &selector)
        .observe(sample.clone(), current.clone());
    assert_eq!(retained.origins(), FxHashSet::from_iter([origin.clone()]));
    let independent = ReadView::task(&state, &actor, &History::default(), &trace)
        .select(function.def_id, &selector)
        .observe(sample, current);
    assert!(
        independent.is_empty(),
        "the weak source catalog cannot supply a different view's facts"
    );
    let queue = channels::Target::Allocation {
        function: function.def_id,
        point: update,
        ty: channel,
        constructor: None,
    };
    flow.queue_send(&queue, &retained, reference, update, &mut state);
    let atom = |storage| Atom {
        coordinate: Coordinate {
            storage,
            path: Vec::new(),
        },
        origin: origin.clone(),
        capture: None,
    };
    let outgoing = Demand::from_iter([atom(Storage::Queue(queue, reference))]);
    assert_eq!(
        flow.transfers[&update].before(&outgoing),
        Demand::from_iter([atom(Storage::Cell(local))])
    );
    assert!(state
        .atoms()
        .iter()
        .any(|atom| matches!(atom.coordinate.storage, Storage::Observation(_))));
}

#[test]
fn queue_only_prefixes_are_visible_until_the_matching_observer_joins() {
    use crate::provenance::address::OpaqueSite;
    use crate::provenance::tasks::proofs::EventFence;
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let channel = table.intern(Ty::Channel { inner: reference });
    let function = MirFunction {
        def_id: DefId::new(9986),
        param_count: 0,
        return_type: int,
        locals: Vec::new(),
        blocks: Vec::new(),
        captures: Vec::new(),
        is_async: false,
        span: Span::DUMMY,
    };
    let point = Point::new(BlockId::ENTRY, 0);
    let target = channels::Target::Allocation {
        function: function.def_id,
        point,
        ty: channel,
        constructor: None,
    };
    let value = Value::reference(Origin::Loan(point));
    let events = [channels::QueueEvent {
        site: OpaqueSite {
            function: function.def_id,
            point,
        },
        write: channels::Write {
            target: target.clone(),
            ty: reference,
            value: value.clone(),
        },
        fence: EventFence {
            peers: FxHashSet::from_iter([Local(1)]),
            ..EventFence::default()
        },
    }];
    let sample = Sample {
        memory: Memory::Queue(target, reference),
        path: Vec::new(),
        ty: reference,
    };
    let state = State::default();
    let flow = Flow::new(&function, &table, None);
    let active = flow.prefix_envelope(&[], &events, &Storage::Logical(Local(2)), &Vec::new());
    assert_eq!(
        flow.read_memory(
            sample.clone(),
            ReadView::current(&state).with_envelope(&active)
        ),
        value
    );
    let joined = flow.prefix_envelope(&[], &events, &Storage::Logical(Local(1)), &Vec::new());
    assert!(flow
        .read_memory(sample, ReadView::current(&state).with_envelope(&joined))
        .is_empty());
}
