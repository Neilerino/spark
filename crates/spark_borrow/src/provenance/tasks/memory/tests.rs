// TAG [SPARK-114]

mod callback_progress;

use super::*;
use crate::provenance::{
    demand::{Atom, Demand},
    tasks, Origin,
};
use rustc_hash::FxHashSet;
use spark_mir::{BlockId, Local, LocalDecl, MirFunction, Place};
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::{IntWidth, LifetimeId, Ty, TypeId, TypeTable};

fn function(types: &[TypeId]) -> MirFunction {
    MirFunction {
        def_id: DefId::new(10010),
        param_count: 0,
        return_type: types[0],
        locals: types
            .iter()
            .enumerate()
            .map(|(index, ty)| LocalDecl {
                local: Local(index as u32),
                ty: *ty,
                name: None,
                mutable: true,
                is_user_variable: false,
                span: Span::DUMMY,
            })
            .collect(),
        blocks: Vec::new(),
        captures: Vec::new(),
        is_async: false,
        span: Span::DUMMY,
    }
}

fn lane(function: &MirFunction, point: Point, actor: &Subject, local: Local) -> Arc<Lane> {
    Arc::new(Lane {
        event: OpaqueSite {
            function: function.def_id,
            point,
        },
        actor: actor.clone(),
        location: Sample {
            memory: Memory::Frame {
                scope: FrameScope::Current,
                function: function.def_id,
                local,
                ty: function.locals[local.index()].ty,
            },
            path: Vec::new(),
            ty: function.locals[local.index()].ty,
        },
    })
}

#[test]
fn projected_pending_writes_invalidate_current_future_copy_peers_even_without_data() {
    let mut table = spark_types::TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let function = function(&[future, future, future]);
    let point = Point::new(BlockId::ENTRY, 0);
    let (target, current) = tasks::birth(function.def_id, point, future);
    let actor = Subject {
        target,
        capture: None,
    };
    let mut state = State::default();
    for local in [Local(0), Local(1), Local(2)] {
        state.set(local, current.clone());
    }
    state
        .future_copies
        .assign(Local(2), state.future_copies.witness(Local(1)), &state.live);
    assert_eq!(state.future_copies.peers(Local(1)).len(), 2);
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    flow.cache_pending_memory(
        &Storage::Logical(Local(0)),
        Vec::new(),
        actor.clone(),
        (
            vec![(lane(&function, point, &actor, Local(1)), Value::default())],
            true,
        ),
        point,
        &mut state,
    );
    assert_eq!(
        state.future_copies.peers(Local(1)),
        FxHashSet::from_iter([Local(1)])
    );
    assert_eq!(
        state.future_copies.peers(Local(2)),
        FxHashSet::from_iter([Local(2)])
    );
}

#[test]
fn pending_and_settled_memory_reach_the_original_rhs_after_argument_consumption() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let future = table.intern(Ty::Future { inner: int });
    let function = function(&[future, reference, reference, reference]);
    let issued = Point::new(BlockId::ENTRY, 0);
    let point = Point::new(BlockId::ENTRY, 1);
    let origin = Origin::Loan(issued);
    let (target, current) = tasks::birth(function.def_id, issued, future);
    let actor = Subject {
        target,
        capture: None,
    };
    let mut state = State::default();
    state.set(Local(0), current);
    state.set(Local(1), Value::reference(origin.clone()));
    state.set(Local(2), Value::default());
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    let rhs = flow.freeze_sources(
        point,
        state.value(Local(1)).sourced(&Storage::Logical(Local(1))),
    );
    flow.cache_pending_memory(
        &Storage::Logical(Local(0)),
        Vec::new(),
        actor.clone(),
        (vec![(lane(&function, point, &actor, Local(2)), rhs)], true),
        point,
        &mut state,
    );
    flow.settle_task_memory(
        &state.future_copies.witness(Local(0)),
        &[],
        point,
        &mut state,
    );
    state.consume(Local(1));
    flow.record_consumed(point, Local(1));
    let read = flow.read(&Place::from_local(Local(2)), &state);
    assert_eq!(read.origins(), FxHashSet::from_iter([origin.clone()]));
    flow.record_store(point, &Place::from_local(Local(3)), &read);
    let atom = |local| Atom {
        coordinate: Coordinate {
            storage: Storage::Logical(local),
            path: Vec::new(),
        },
        origin: origin.clone(),
        capture: None,
    };
    assert_eq!(
        flow.transfers[&point].before(&Demand::from_iter([atom(Local(3))])),
        Demand::from_iter([atom(Local(1))])
    );
}

#[test]
fn a_captured_call_witness_cannot_complete_a_peer_written_during_the_prefix() {
    use crate::provenance::tasks::proofs::CallProof;
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let function = function(&[future, future, future, future]);
    let point = Point::new(BlockId::ENTRY, 0);
    let (target, current) = tasks::birth(function.def_id, point, future);
    let actor = Subject {
        target,
        capture: None,
    };
    let mut state = State::default();
    for local in [Local(0), Local(1), Local(2), Local(3)] {
        state.set(local, current.clone());
    }
    state
        .future_copies
        .assign(Local(2), state.future_copies.witness(Local(1)), &state.live);
    let proof = CallProof {
        completed: vec![state.future_copies.witness(Local(1))],
        returned: Some(state.future_copies.witness(Local(1))),
        ..CallProof::default()
    };
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    flow.cache_pending_memory(
        &Storage::Logical(Local(0)),
        Vec::new(),
        actor.clone(),
        (
            vec![(lane(&function, point, &actor, Local(1)), Value::default())],
            true,
        ),
        point,
        &mut state,
    );
    let returned = flow.apply_call_future_proof(proof, &[], point, &mut state);
    flow.store_future_copy(&Place::from_local(Local(3)), returned, &mut state);
    let pending = |local| {
        state
            .value(local)
            .futures
            .values()
            .flatten()
            .any(|handle| handle.pending)
    };
    assert!(
        pending(Local(1)),
        "the mutable coordinate no longer proves the captured object"
    );
    assert!(
        !pending(Local(2)),
        "the unaffected proven peer still completes"
    );
    assert!(
        !state.future_copies.peers(Local(3)).contains(&Local(1)),
        "return cannot restore the invalidated equality"
    );
}

#[test]
fn a_helper_residual_callback_keeps_its_bound_rhs_without_a_task_catalog_row() {
    use tasks::residual::{Import, Key};
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let future = table.intern(Ty::Future { inner: int });
    let function = function(&[future, reference]);
    let issued = Point::new(BlockId::ENTRY, 0);
    let point = Point::new(BlockId::ENTRY, 1);
    let origin = Origin::Loan(issued);
    let (target, current) = tasks::birth(function.def_id, issued, future);
    let key = Arc::new(Key {
        site: OpaqueSite {
            function: function.def_id,
            point,
        },
        role: Vec::new(),
        ty: future,
        actor: Subject {
            target,
            capture: None,
        },
    });
    let mut state = State::default();
    state.set(Local(1), Value::reference(origin.clone()));
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    let callback = flow.freeze_sources(
        point,
        state.value(Local(1)).sourced(&Storage::Logical(Local(1))),
    );
    assert!(flow.import_residual(
        Import {
            key: key.clone(),
            value: current,
            callbacks: vec![(reference, callback)],
            visibility: tasks::residual::Visibility::Continuation(
                tasks::proofs::EventFence::default()
            ),
        },
        point,
        &mut state
    ));
    state.consume(Local(1));
    flow.record_consumed(point, Local(1));
    assert!(state.tasks.is_empty());
    let atom = |storage| Atom {
        coordinate: Coordinate {
            storage,
            path: Vec::new(),
        },
        origin: origin.clone(),
        capture: None,
    };
    assert_eq!(
        flow.transfers[&point].before(&Demand::from_iter([atom(Storage::ResidualCallback(
            key, reference
        )),])),
        Demand::from_iter([atom(Storage::Logical(Local(1)))])
    );
}

#[test]
fn a_writer_with_no_resolved_target_survives_uncertain_root_loss() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let function = function(&[future, future]);
    let point = Point::new(BlockId::ENTRY, 0);
    let (target, current) = tasks::birth(function.def_id, point, future);
    let actor = Subject {
        target,
        capture: None,
    };
    let mut state = State::default();
    state.set(Local(1), current);
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    flow.cache_pending_memory(
        &Storage::Logical(Local(1)),
        Vec::new(),
        actor,
        (Vec::new(), true),
        point,
        &mut state,
    );
    flow.preserve_writer_loss(&Place::from_local(Local(1)), point, &mut state);
    state.consume(Local(1));
    flow.record_consumed(point, Local(1));
    assert_eq!(state.residuals.len(), 1);
    let residual = state.residuals.values().next().unwrap();
    assert!(residual
        .task_views
        .values()
        .flat_map(|subjects| subjects.values())
        .any(|view| view.memory.may_write));
    assert!(
        state.pending_memory.is_empty(),
        "an unresolved target is not a fabricated wildcard write"
    );
}

#[test]
fn installing_normal_activity_does_not_reobserve_a_loss_without_its_join_fence() {
    use tasks::{
        proofs::EventFence,
        residual::{Import, Key},
    };
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let function = function(&[future, future]);
    let point = Point::new(BlockId::ENTRY, 0);
    let (target, current) = tasks::birth(function.def_id, point, future);
    let key = Arc::new(Key {
        site: OpaqueSite {
            function: function.def_id,
            point,
        },
        role: Vec::new(),
        ty: future,
        actor: Subject {
            target,
            capture: None,
        },
    });
    let mut state = State::default();
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    flow.import_residual(
        Import {
            key: key.clone(),
            value: current.clone(),
            callbacks: Vec::new(),
            visibility: tasks::residual::Visibility::Occurrence(EventFence {
                inputs: FxHashSet::from_iter([Local(1)]),
                ..EventFence::default()
            }),
        },
        point,
        &mut state,
    );
    flow.import_residual(
        Import {
            key: key.clone(),
            value: current,
            callbacks: Vec::new(),
            visibility: tasks::residual::Visibility::Continuation(
                tasks::proofs::EventFence::default(),
            ),
        },
        point,
        &mut state,
    );
    assert_eq!(
        flow.event_guards[&crate::provenance::effects::EventKey::Residual(key)],
        FxHashSet::from_iter([Local(1)])
    );
}

#[test]
fn a_residual_only_diverging_dispatch_preserves_the_reachable_prefix() {
    use crate::provenance::effects::{Dispatch, PrefixWrites};
    use tasks::residual::{Import, Key};
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let function = function(&[future, future]);
    let point = Point::new(BlockId::ENTRY, 0);
    let (target, current) = tasks::birth(function.def_id, point, future);
    let key = Arc::new(Key {
        site: OpaqueSite {
            function: function.def_id,
            point,
        },
        role: Vec::new(),
        ty: future,
        actor: Subject {
            target,
            capture: None,
        },
    });
    let mut state = State::default();
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    let dispatch = Dispatch {
        returning: Vec::new(),
        diverging: vec![PrefixWrites {
            memory: Vec::new(),
            queues: Vec::new(),
            residuals: vec![Import {
                key: key.clone(),
                value: current,
                callbacks: Vec::new(),
                visibility: tasks::residual::Visibility::Occurrence(
                    tasks::proofs::EventFence::default(),
                ),
            }],
        }],
    };
    assert!(!flow.apply_dispatch(
        dispatch,
        &[],
        &Place::from_local(Local(0)),
        point,
        &mut state
    ));
    assert!(flow.prefix_residuals.contains_key(&key));
    assert!(
        state.residuals.is_empty(),
        "a diverging alternative is not a normal continuation"
    );
}

#[test]
fn prefix_only_activity_does_not_become_a_normal_continuation_owner() {
    use crate::provenance::effects::{CallOutcome, PrefixWrites};
    use tasks::residual::{Import, Key};
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let function = function(&[int, future]);
    let point = Point::new(BlockId::ENTRY, 0);
    let (target, current) = tasks::birth(function.def_id, point, future);
    let old = Arc::new(Key {
        site: OpaqueSite {
            function: function.def_id,
            point,
        },
        role: Vec::new(),
        ty: future,
        actor: Subject {
            target,
            capture: None,
        },
    });
    let mut new = (*old).clone();
    new.role.push(crate::provenance::Slot::Element);
    let new = Arc::new(new);
    let mut state = State::default();
    state.residuals.insert(old.clone(), current.clone());
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    let outcome = CallOutcome {
        returned: Value::default(),
        tasks: Vec::new(),
        completed_task: None,
        clobbered_copies: Vec::new(),
        future_proof: tasks::proofs::CallProof::default(),
        returned_prefix: Default::default(),
        publications: PrefixWrites::default(),
        place_writes: Vec::new(),
        memory: Vec::new(),
        async_memory: Vec::new(),
        fresh_outputs: Vec::new(),
        spawn_outputs: Vec::new(),
        producer_events: Vec::new(),
        queue_accesses: crate::provenance::channels::access::Accesses::default(),
        queue_send_counts: crate::provenance::channels::counts::Sends::default(),
        direct_queue_send: None,
        fresh_birth: None,
        fresh_leaf_task: false,
        spawn_owners: None,
        returned_owner_tags: crate::provenance::tasks::lifecycle::Tags::default(),
        retained_captures: Vec::new(),
        returned_callbacks: crate::provenance::tasks::outputs::CallbackOutputs::default(),
        residuals: Vec::new(),
        prefix_memory: Vec::new(),
        queues: Vec::new(),
        queue_contents: Vec::new(),
        prefix_residuals: vec![old.clone(), new.clone()]
            .into_iter()
            .map(|key| Import {
                key,
                value: current.clone(),
                callbacks: Vec::new(),
                visibility: tasks::residual::Visibility::Occurrence(
                    tasks::proofs::EventFence::default(),
                ),
            })
            .collect(),
    };
    flow.apply_call_outcomes(
        &[],
        &Place::from_local(Local(0)),
        vec![outcome],
        point,
        &mut state,
    );
    assert!(
        state.residuals.contains_key(&old),
        "a historical key cannot remove a preexisting owner"
    );
    assert!(
        !state.residuals.contains_key(&new),
        "activity confined to a nonreturning branch is not a continuation owner"
    );
    assert!(
        flow.prefix_residuals.contains_key(&new),
        "the reachable prefix still exports the occurrence"
    );
}

#[test]
fn normal_activity_keeps_its_exact_facts_when_a_prefix_instance_has_the_same_key() {
    use crate::provenance::effects::{CallOutcome, PrefixWrites};
    use tasks::residual::{Import, Key, Visibility};
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let future = table.intern(Ty::Future { inner: int });
    let function = function(&[int, reference, reference]);
    let point = Point::new(BlockId::ENTRY, 2);
    let live_origin = Origin::Loan(Point::new(BlockId::ENTRY, 0));
    let prefix_origin = Origin::Loan(Point::new(BlockId::ENTRY, 1));
    let (target, current) = tasks::birth(function.def_id, point, future);
    let key = Arc::new(Key {
        site: OpaqueSite {
            function: function.def_id,
            point,
        },
        role: Vec::new(),
        ty: future,
        actor: Subject {
            target,
            capture: None,
        },
    });
    let mut state = State::default();
    state.set(Local(1), Value::reference(live_origin.clone()));
    state.set(Local(2), Value::reference(prefix_origin));
    let live_callback = state.value(Local(1)).sourced(&Storage::Logical(Local(1)));
    let mut prefix_callback = live_callback.clone();
    prefix_callback.join(&state.value(Local(2)).sourced(&Storage::Logical(Local(2))));
    let mut live = current.clone();
    live.join(&live_callback);
    let mut prefix = current;
    prefix.join(&prefix_callback);
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    let outcome = CallOutcome {
        returned: Value::default(),
        tasks: Vec::new(),
        completed_task: None,
        clobbered_copies: Vec::new(),
        future_proof: tasks::proofs::CallProof::default(),
        returned_prefix: Default::default(),
        publications: PrefixWrites::default(),
        place_writes: Vec::new(),
        memory: Vec::new(),
        async_memory: Vec::new(),
        fresh_outputs: Vec::new(),
        spawn_outputs: Vec::new(),
        producer_events: Vec::new(),
        queue_accesses: crate::provenance::channels::access::Accesses::default(),
        queue_send_counts: crate::provenance::channels::counts::Sends::default(),
        direct_queue_send: None,
        fresh_birth: None,
        fresh_leaf_task: false,
        spawn_owners: None,
        returned_owner_tags: crate::provenance::tasks::lifecycle::Tags::default(),
        retained_captures: Vec::new(),
        returned_callbacks: crate::provenance::tasks::outputs::CallbackOutputs::default(),
        prefix_memory: Vec::new(),
        queues: Vec::new(),
        queue_contents: Vec::new(),
        residuals: vec![Import {
            key: key.clone(),
            value: live,
            callbacks: vec![(reference, live_callback)],
            visibility: Visibility::Continuation(tasks::proofs::EventFence::default()),
        }],
        prefix_residuals: vec![Import {
            key: key.clone(),
            value: prefix,
            callbacks: vec![(reference, prefix_callback)],
            visibility: Visibility::Occurrence(tasks::proofs::EventFence::default()),
        }],
    };
    flow.apply_call_outcomes(
        &[],
        &Place::from_local(Local(0)),
        vec![outcome],
        point,
        &mut state,
    );
    assert_eq!(
        state.residuals[&key].origins(),
        FxHashSet::from_iter([live_origin.clone()])
    );
    assert_eq!(
        state.residual_callbacks[&(key.clone(), reference)].origins(),
        FxHashSet::from_iter([live_origin.clone()])
    );
    let atom = |storage| Atom {
        coordinate: Coordinate {
            storage,
            path: Vec::new(),
        },
        origin: live_origin.clone(),
        capture: None,
    };
    let callback = state.residual_callbacks[&(key.clone(), reference)]
        .clone()
        .sourced(&Storage::ResidualCallback(key, reference));
    assert_eq!(
        flow.freeze_sources(point, callback).all_sources(),
        FxHashSet::from_iter([crate::provenance::demand::Source::Entry(atom(
            Storage::Logical(Local(1))
        ))])
    );
    assert!(
        flow.transfers[&point]
            .seeds
            .iter()
            .any(|atom| atom.coordinate.storage == Storage::Logical(Local(2))),
        "the separate prefix observation still demands its own RHS"
    );
}

#[test]
fn restored_live_history_keeps_data_sampled_only_by_the_normal_closure() {
    use crate::provenance::{
        address::{Intent, Selector},
        effects::{CallOutcome, PrefixWrites},
        observations::{History, ReadView},
    };
    use tasks::residual::{Import, Key, Visibility};
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let future = table.intern(Ty::Future { inner: int });
    let function = function(&[int, reference]);
    let point = Point::new(BlockId::ENTRY, 1);
    let origin = Origin::Loan(Point::new(BlockId::ENTRY, 0));
    let (target, current) = tasks::birth(function.def_id, point, future);
    let actor = Subject {
        target,
        capture: None,
    };
    let key = Arc::new(Key {
        site: OpaqueSite {
            function: function.def_id,
            point,
        },
        role: Vec::new(),
        ty: future,
        actor: actor.clone(),
    });
    let mut state = State::default();
    state.set(Local(1), Value::reference(origin.clone()));
    let sample = Sample {
        memory: Memory::Frame {
            scope: FrameScope::Current,
            function: function.def_id,
            local: Local(1),
            ty: reference,
        },
        path: Vec::new(),
        ty: reference,
    };
    let selector = Selector {
        input: Local(1),
        selection: Vec::new(),
        intent: Intent::Value,
        expected: Some(reference),
        capture: None,
        channel_capture: None,
        queue: None,
    };
    let trace = std::cell::RefCell::default();
    ReadView::task(&state, &actor, &History::default(), &trace)
        .select(function.def_id, &selector)
        .observe(sample.clone(), Value::default());
    let (saved, _) = trace.into_inner().into_iter().next().unwrap();
    let mut live = current.clone();
    let mut view = tasks::views::View::unknown();
    view.history.import_sample(saved.clone(), Value::default());
    live.task_views
        .entry(Vec::new())
        .or_default()
        .insert(actor.clone(), view);
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    let outcome = CallOutcome {
        returned: Value::default(),
        tasks: Vec::new(),
        completed_task: None,
        clobbered_copies: Vec::new(),
        future_proof: tasks::proofs::CallProof::default(),
        returned_prefix: Default::default(),
        publications: PrefixWrites::default(),
        place_writes: Vec::new(),
        memory: Vec::new(),
        async_memory: Vec::new(),
        fresh_outputs: Vec::new(),
        spawn_outputs: Vec::new(),
        producer_events: Vec::new(),
        queue_accesses: crate::provenance::channels::access::Accesses::default(),
        queue_send_counts: crate::provenance::channels::counts::Sends::default(),
        direct_queue_send: None,
        fresh_birth: None,
        fresh_leaf_task: false,
        spawn_owners: None,
        returned_owner_tags: crate::provenance::tasks::lifecycle::Tags::default(),
        retained_captures: Vec::new(),
        returned_callbacks: crate::provenance::tasks::outputs::CallbackOutputs::default(),
        prefix_memory: Vec::new(),
        queues: Vec::new(),
        queue_contents: Vec::new(),
        residuals: vec![Import {
            key: key.clone(),
            value: live,
            callbacks: Vec::new(),
            visibility: Visibility::Continuation(tasks::proofs::EventFence::default()),
        }],
        prefix_residuals: vec![Import {
            key: key.clone(),
            value: current,
            callbacks: Vec::new(),
            visibility: Visibility::Occurrence(tasks::proofs::EventFence::default()),
        }],
    };
    flow.apply_call_outcomes(
        &[],
        &Place::from_local(Local(0)),
        vec![outcome],
        point,
        &mut state,
    );
    let history = &state.residuals[&key].task_views[&Vec::new()][&actor].history;
    assert!(
        history
            .samples()
            .any(|(_, value)| value.origins().contains(&origin)),
        "normal closure sampled the source"
    );
    assert!(
        state.observations[&saved].origins().contains(&origin),
        "the restored history must retain its live source backing"
    );
    let next = Point::new(BlockId::ENTRY, 2);
    flow.ensure_transfer(next, &state);
    let trace = std::cell::RefCell::default();
    let reread = ReadView::task(&state, &actor, history, &trace)
        .select(function.def_id, &selector)
        .observe(sample, Value::default());
    let frozen = flow.freeze_sources(next, reread);
    assert!(frozen.origins().contains(&origin));
    assert!(
        !frozen.all_sources().is_empty(),
        "a later MIR boundary must still resolve the old observation to its source"
    );
}

#[test]
fn completing_a_reused_static_task_does_not_retire_an_older_residual_writer() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let future = table.intern(Ty::Future { inner: int });
    let function = function(&[int, future, reference, reference]);
    let issued = Point::new(BlockId::ENTRY, 0);
    let point = Point::new(BlockId::ENTRY, 1);
    let origin = Origin::Loan(issued);
    let (target, old) = tasks::birth(function.def_id, issued, future);
    let actor = Subject {
        target,
        capture: None,
    };
    let write = lane(&function, point, &actor, Local(2));
    let mut state = State::default();
    state.set(Local(1), old);
    state.set(Local(2), Value::default());
    state.set(Local(3), Value::reference(origin.clone()));
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    flow.cache_pending_memory(
        &Storage::Logical(Local(1)),
        Vec::new(),
        actor.clone(),
        (vec![(write.clone(), Value::default())], true),
        point,
        &mut state,
    );
    flow.preserve_writer_loss(&Place::from_local(Local(1)), point, &mut state);
    let key = state.residuals.keys().next().unwrap().clone();
    state.consume(Local(1));
    flow.record_consumed(point, Local(1));
    let (_, replacement) = tasks::birth(function.def_id, issued, future);
    flow.store(&Place::from_local(Local(1)), replacement, point, &mut state);
    flow.complete_future(Local(1), &Value::default(), point, &mut state);
    assert!(
        state.residuals[&key]
            .futures
            .values()
            .flatten()
            .any(|handle| handle.pending),
        "current completion cannot certify every instance at an equal static target"
    );
    let rhs = state.value(Local(3)).sourced(&Storage::Logical(Local(3)));
    flow.cache_pending_memory(
        &Storage::Residual(key),
        Vec::new(),
        actor,
        (vec![(write, rhs)], true),
        point,
        &mut state,
    );
    let read = flow.read(&Place::from_local(Local(2)), &state);
    assert_eq!(read.origins(), FxHashSet::from_iter([origin.clone()]));
    assert_eq!(
        flow.freeze_sources(point, read).all_sources(),
        FxHashSet::from_iter([crate::provenance::demand::Source::Entry(Atom {
            coordinate: Coordinate {
                storage: Storage::Logical(Local(3)),
                path: Vec::new()
            },
            origin,
            capture: None
        })])
    );
}

#[test]
fn a_capture_only_child_settles_its_last_write_before_its_backing_is_removed() {
    use crate::provenance::{tasks::Role, Slot};
    for empty_support in [false, true] {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let reference = table.intern(Ty::Ref {
            inner: int,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let future = table.intern(Ty::Future { inner: int });
        let callback = table.intern(Ty::Function {
            params: vec![],
            ret: int,
            is_async: false,
        });
        let function = function(&[future, reference, reference, reference, future]);
        let issued = Point::new(BlockId::ENTRY, 0);
        let point = Point::new(BlockId::ENTRY, 1);
        let origin = Origin::Loan(issued);
        let (_, child) = tasks::birth(function.def_id, issued, future);
        let capture = tasks::Capture {
            owner: DefId::new(10030),
            index: 0,
            ty: future,
        };
        let path = vec![Slot::Capture(capture.owner, capture.index, future)];
        let qualified = child.clone().with_future_capture(capture);
        let actor = Subject::from(qualified.futures[&vec![]].iter().next().unwrap());
        let (_, parent) = tasks::birth(function.def_id, point, future);
        let mut state = State::default();
        state.set(Local(0), parent);
        state.set(Local(1), Value::reference(origin.clone()));
        state.set(Local(2), Value::default());
        state.set(Local(4), child);
        let role = Role::capture(Local(0), capture.owner, capture.index, future)
            .in_execution(Local(0), callback)
            .unwrap();
        state.future_copies.assign_role(
            role.clone(),
            state.future_copies.witness(Local(4)),
            &state.live,
        );
        state.executions.insert(
            (Local(0), callback),
            qualified.prefixed_path(&path, callback, &table),
        );
        state.consume(Local(4));
        let witness = state.future_copies.role_witness(&role);
        assert!(witness.locals().next().is_none());
        let mut flow = Flow::new(&function, &table, None);
        flow.ensure_transfer(point, &state);
        let mut rhs = flow.freeze_sources(
            point,
            state.value(Local(1)).sourced(&Storage::Logical(Local(1))),
        );
        if empty_support {
            for sources in rhs.sources.values_mut() {
                sources.clear();
            }
        }
        let write = lane(&function, point, &actor, Local(2));
        flow.cache_pending_memory(
            &Storage::ExecutionCallback(Local(0), callback),
            path,
            actor,
            (vec![(write.clone(), rhs)], true),
            point,
            &mut state,
        );
        flow.settle_task_memory(&witness, &[], point, &mut state);
        assert!(
            state
                .settled_memory
                .get(&write.location)
                .is_some_and(|value| value.origins().contains(&origin)),
            "the sole captured owner must settle its RHS before callback storage disappears"
        );
        state.executions.remove(&(Local(0), callback));
        state.consume(Local(1));
        flow.record_consumed(point, Local(1));
        let read = flow.read(&Place::from_local(Local(2)), &state);
        assert!(read.origins().contains(&origin));
        flow.record_store(point, &Place::from_local(Local(3)), &read);
        let atom = |local| Atom {
            coordinate: Coordinate {
                storage: Storage::Logical(local),
                path: vec![],
            },
            origin: origin.clone(),
            capture: None,
        };
        let expected = if empty_support {
            Demand::default()
        } else {
            Demand::from_iter([atom(Local(1))])
        };
        assert_eq!(
            flow.transfers[&point].before(&Demand::from_iter([atom(Local(3))])),
            expected
        );
    }
}
