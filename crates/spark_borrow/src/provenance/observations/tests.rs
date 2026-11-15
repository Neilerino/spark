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
            scope: crate::provenance::observations::FrameScope::Current,
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

#[test]
fn exporting_a_task_view_preserves_empty_observation_support() {
    use crate::provenance::aliases::Port;
    use crate::provenance::tasks::views::View;

    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let function = DefId::new(9990);
    let point = Point::new(BlockId::ENTRY, 0);
    let actor = Subject {
        target: tasks::birth(function, point, future).0,
        capture: None,
    };
    let port = Port::root(Local(1), int);
    let lane = Arc::new(Lane {
        site: None,
        receive: None,
        actor: actor.clone(),
        function,
        selector: port.selector(),
        role: ReadRole::Payload,
        sample: Sample {
            memory: Memory::External(port),
            path: Vec::new(),
            ty: int,
        },
    });
    let mut history = History::default();
    history.insert(lane, Carrier::default());
    let mut value = Value::default();
    value.task_views.entry(Vec::new()).or_default().insert(
        actor.clone(),
        View {
            history: history.clone(),
            ..View::default()
        },
    );
    let exported = crate::provenance::effects::export_value(&value, function);
    assert_eq!(exported.task_views[&Vec::new()][&actor].history, history);
}

#[test]
fn imported_history_frontiers_survive_consuming_the_call_argument() {
    use crate::provenance::tasks::views::View;

    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let future = table.intern(Ty::Future { inner: int });
    let function = MirFunction {
        def_id: DefId::new(9998),
        param_count: 0,
        return_type: future,
        locals: [future, reference]
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
    let issued = Point::new(BlockId::ENTRY, 0);
    let call = Point::new(BlockId::ENTRY, 1);
    let origin = Origin::Loan(issued);
    let mut state = State::default();
    state.set(Local(1), Value::reference(origin.clone()));
    let (target, mut returned) = tasks::birth(function.def_id, issued, future);
    let actor = Subject {
        target,
        capture: None,
    };
    let lane = Arc::new(Lane {
        site: None,
        receive: None,
        actor: actor.clone(),
        function: function.def_id,
        selector: Selector {
            input: Local(1),
            selection: Vec::new(),
            intent: Intent::Value,
            expected: Some(reference),
            capture: None,
            channel_capture: None,
            queue: None,
        },
        role: ReadRole::Payload,
        sample: Sample {
            memory: Memory::Frame {
                scope: FrameScope::Current,
                function: function.def_id,
                local: Local(1),
                ty: reference,
            },
            path: Vec::new(),
            ty: reference,
        },
    });
    let mut history = History::default();
    history.import_sample(
        lane.clone(),
        state.value(Local(1)).sourced(&Storage::Logical(Local(1))),
    );
    returned.task_views.entry(Vec::new()).or_default().insert(
        actor,
        View {
            history,
            ..View::default()
        },
    );
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(call, &state);
    let returned = flow.freeze_sources(call, returned);
    state.consume(Local(1));
    flow.record_consumed(call, Local(1));
    flow.store(&Place::from_local(Local(0)), returned, call, &mut state);
    assert!(state.observations.contains_key(&lane));
    let atom = |storage| Atom {
        coordinate: Coordinate {
            storage,
            path: Vec::new(),
        },
        origin: origin.clone(),
        capture: None,
    };
    let demand = Demand::from_iter([atom(Storage::Observation(lane))]);
    assert_eq!(
        flow.transfers[&call].before(&demand),
        Demand::from_iter([atom(Storage::Logical(Local(1)))])
    );
}

#[test]
fn prefix_history_imports_freeze_all_sources_before_installing_any_rows() {
    use crate::provenance::{address::OpaqueSite, tasks::views::View};

    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let function = MirFunction {
        def_id: DefId::new(9999),
        param_count: 0,
        return_type: future,
        locals: Vec::new(),
        blocks: Vec::new(),
        captures: Vec::new(),
        is_async: false,
        span: Span::DUMMY,
    };
    let point = Point::new(BlockId::ENTRY, 0);
    let actor = Subject {
        target: tasks::birth(function.def_id, point, future).0,
        capture: None,
    };
    let lane = |local| {
        Arc::new(Lane {
            site: None,
            receive: None,
            actor: actor.clone(),
            function: function.def_id,
            selector: Selector {
                input: local,
                selection: Vec::new(),
                intent: Intent::Value,
                expected: Some(int),
                capture: None,
                channel_capture: None,
                queue: None,
            },
            role: ReadRole::Payload,
            sample: Sample {
                memory: Memory::Frame {
                    scope: FrameScope::Current,
                    function: function.def_id,
                    local,
                    ty: int,
                },
                path: Vec::new(),
                ty: int,
            },
        })
    };
    let first = lane(Local(1));
    let second = lane(Local(2));
    let origin = Origin::Loan(point);
    let value = Value::reference(origin.clone());
    let mut state = State::default();
    state.set(Local(3), value.clone());
    state.observations.insert(first.clone(), value.clone());
    let site = OpaqueSite {
        function: function.def_id,
        point,
    };
    let event = |lane, source| {
        let mut history = History::default();
        history.import_sample(lane, value.clone().sourced(&source));
        let mut payload = Value::default();
        payload.task_views.entry(Vec::new()).or_default().insert(
            actor.clone(),
            View {
                history,
                ..View::default()
            },
        );
        channels::QueueEvent {
            site,
            write: channels::Write {
                target: channels::Target::Unknown {
                    site,
                    ty: future,
                    constructor: None,
                },
                ty: future,
                value: payload,
            },
            fence: Default::default(),
        }
    };
    let mut events = [
        event(first.clone(), Storage::Logical(Local(3))),
        event(second.clone(), Storage::Observation(first.clone())),
    ];
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    flow.import_prefix_observations(&mut [], &mut events, point, &mut state);
    let atom = |lane| Atom {
        coordinate: Coordinate {
            storage: Storage::Observation(lane),
            path: Vec::new(),
        },
        origin: origin.clone(),
        capture: None,
    };
    assert_eq!(
        flow.transfers[&point].before(&Demand::from_iter([atom(second)])),
        Demand::from_iter([atom(first)]),
        "a later payload must resolve against entry rows, before the first import adds a source"
    );
}

#[test]
fn returned_history_follows_only_live_unclobbered_pending_copy_witnesses() {
    use crate::provenance::{
        effects::{CallOutcome, PrefixWrites},
        tasks::{proofs::CallProof, views::View, Capture},
    };
    use spark_mir::Operand;

    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let function = MirFunction {
        def_id: DefId::new(10000),
        param_count: 0,
        return_type: future,
        locals: (0..3)
            .map(|index| LocalDecl {
                local: Local(index),
                ty: future,
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
    let point = Point::new(BlockId::ENTRY, 0);
    let (target, original) = tasks::birth(function.def_id, point, future);
    let subject = Subject {
        target,
        capture: None,
    };
    let lane = Arc::new(Lane {
        site: None,
        receive: None,
        actor: subject.clone(),
        function: function.def_id,
        selector: Selector {
            input: Local(1),
            selection: Vec::new(),
            intent: Intent::Value,
            expected: Some(int),
            capture: None,
            channel_capture: None,
            queue: None,
        },
        role: ReadRole::Payload,
        sample: Sample {
            memory: Memory::Frame {
                scope: FrameScope::Current,
                function: function.def_id,
                local: Local(1),
                ty: int,
            },
            path: Vec::new(),
            ty: int,
        },
    });
    let mut learned = History::default();
    learned.insert(lane.clone(), Carrier::default());
    for (name, proved, clobbered, live, capture_matches, pending, source_learned, expected) in [
        ("moved peer", true, false, true, true, true, true, true),
        (
            "same target without proof",
            false,
            false,
            true,
            true,
            true,
            true,
            false,
        ),
        ("clobbered peer", true, true, true, true, true, true, false),
        ("dead peer", true, false, false, true, true, true, false),
        (
            "different capture",
            true,
            false,
            true,
            false,
            true,
            true,
            false,
        ),
        (
            "completed return",
            true,
            false,
            true,
            true,
            false,
            true,
            false,
        ),
        (
            "unproven peer learned",
            true,
            false,
            true,
            true,
            true,
            false,
            false,
        ),
    ] {
        let mut source = original.clone();
        source.task_views.entry(Vec::new()).or_default().insert(
            subject.clone(),
            View {
                history: if source_learned {
                    learned.clone()
                } else {
                    History::default()
                },
                ..View::default()
            },
        );
        let mut other = original.clone();
        other.task_views.entry(Vec::new()).or_default().insert(
            subject.clone(),
            View {
                history: learned.clone(),
                ..View::default()
            },
        );
        let mut state = State::default();
        state.set(Local(1), source);
        state.set(Local(2), other);
        state.observations.insert(lane.clone(), Value::default());
        let witness = state.future_copies.witness(Local(1));
        if !live {
            state.consume(Local(1));
        }
        let mut returned = original.clone();
        let handles = returned.futures.get_mut(&Vec::new()).unwrap();
        *handles = handles
            .iter()
            .map(|handle| tasks::Handle {
                pending,
                capture: if capture_matches {
                    None
                } else {
                    Some(Capture {
                        owner: function.def_id,
                        index: 1,
                        ty: future,
                    })
                },
                ..handle.clone()
            })
            .collect();
        let outcome = CallOutcome {
            fresh_leaf_task: false,
            residuals: Vec::new(),
            async_memory: Vec::new(),
            returned,
            tasks: Vec::new(),
            completed_task: None,
            clobbered_copies: if clobbered {
                vec![Local(1)]
            } else {
                Vec::new()
            },
            future_proof: CallProof {
                completed: Vec::new(),
                returned: proved.then_some(witness),
            },
            returned_prefix: Default::default(),
            publications: PrefixWrites::default(),
            place_writes: Vec::new(),
            memory: Vec::new(),
            prefix_memory: Vec::new(),
            prefix_residuals: Vec::new(),
            queues: Vec::new(),
            queue_contents: Vec::new(),
        };
        let mut flow = Flow::new(&function, &table, None);
        flow.ensure_transfer(point, &state);
        flow.apply_call_outcomes(
            &[Operand::Move(Place::from_local(Local(1)))],
            &Place::from_local(Local(0)),
            vec![outcome],
            point,
            &mut state,
        );
        let carried = state.value(Local(0));
        let retained = carried
            .task_views
            .values()
            .flat_map(|views| views.values())
            .any(|view| view.history.samples().next().is_some());
        assert_eq!(retained, expected, "{name}");
        assert!(
            !state.live.contains(&Local(1)),
            "the input was consumed: {name}"
        );
    }
}
