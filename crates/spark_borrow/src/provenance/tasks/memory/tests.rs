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
            false,
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
        (
            vec![(lane(&function, point, &actor, Local(2)), rhs)],
            true,
            false,
        ),
        point,
        &mut state,
    );
    let prepared = flow.prepare_task_memory(
        vec![state.future_copies.witness(Local(0))],
        &[],
        point,
        &mut state,
    );
    flow.commit_task_memory(prepared, point, &mut state);
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
            false,
        ),
        point,
        &mut state,
    );
    let prepared = flow.prepare_task_memory(proof.completed, &[], point, &mut state);
    let settled = flow.commit_task_memory(prepared, point, &mut state);
    let returned = flow.apply_call_copy_proof(proof.returned, &[], &mut state);
    for completion in settled {
        completion.finish(&mut flow, None, point, &mut state);
    }
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
        (Vec::new(), true, false),
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
        actual_definitions: Vec::new(),
        ordered_definitions: Vec::new(),
        fresh_outputs: Vec::new(),
        spawn_outputs: Vec::new(),
        producer_events: Vec::new(),
        queue_accesses: crate::provenance::channels::access::Accesses::default(),
        queue_send_counts: crate::provenance::channels::counts::Sends::default(),
        receipt: None,
        direct_queue_send: None,
        spawned_memory: None,
        spawn_return: None,
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
        actual_definitions: Vec::new(),
        ordered_definitions: Vec::new(),
        fresh_outputs: Vec::new(),
        spawn_outputs: Vec::new(),
        producer_events: Vec::new(),
        queue_accesses: crate::provenance::channels::access::Accesses::default(),
        queue_send_counts: crate::provenance::channels::counts::Sends::default(),
        receipt: None,
        direct_queue_send: None,
        spawned_memory: None,
        spawn_return: None,
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
        actual_definitions: Vec::new(),
        ordered_definitions: Vec::new(),
        fresh_outputs: Vec::new(),
        spawn_outputs: Vec::new(),
        producer_events: Vec::new(),
        queue_accesses: crate::provenance::channels::access::Accesses::default(),
        queue_send_counts: crate::provenance::channels::counts::Sends::default(),
        receipt: None,
        direct_queue_send: None,
        spawned_memory: None,
        spawn_return: None,
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
        (vec![(write.clone(), Value::default())], true, false),
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
        (vec![(write, rhs)], true, false),
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
            (vec![(write.clone(), rhs)], true, false),
            point,
            &mut state,
        );
        let prepared = flow.prepare_task_memory(vec![witness], &[], point, &mut state);
        flow.commit_task_memory(prepared, point, &mut state);
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

#[test]
fn unsupported_frame_writes_keep_payload_sources_and_unknown_support() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let function = function(&[future, int]);
    let point = Point::new(BlockId::ENTRY, 0);
    let (target, current) = tasks::birth(function.def_id, point, future);
    let actor = Subject {
        target,
        capture: None,
    };
    for (scope, owner) in [
        (FrameScope::Foreign, function.def_id),
        (FrameScope::Current, DefId::new(10011)),
    ] {
        let mut state = State::default();
        state.set(Local(0), current.clone());
        let origin = Origin::Loan(point);
        state.set(Local(1), Value::reference(origin.clone()));
        let mut flow = Flow::new(&function, &table, None);
        flow.ensure_transfer(point, &state);
        let mut write = (*lane(&function, point, &actor, Local(1))).clone();
        let Memory::Frame {
            scope: actual_scope,
            function: actual_owner,
            ..
        } = &mut write.location.memory
        else {
            unreachable!()
        };
        *actual_scope = scope;
        *actual_owner = owner;
        let write = Arc::new(write);
        let rhs = state.value(Local(1)).sourced(&Storage::Logical(Local(1)));
        flow.cache_pending_memory(
            &Storage::Logical(Local(0)),
            Vec::new(),
            actor.clone(),
            (vec![(write.clone(), rhs)], true, false),
            point,
            &mut state,
        );
        assert!(flow.unknown_memory_write);
        let view = &state.values[&Local(0)].task_views[&Vec::new()][&actor];
        assert!(view.memory.unknown_write);
        let (_, value) = view.memory.samples().next().unwrap();
        let sampled = Flow::sample_pending_write(&write, value, &state);
        assert!(sampled.origins().contains(&origin));
        let atom = |storage| Atom {
            coordinate: Coordinate {
                storage,
                path: Vec::new(),
            },
            origin: origin.clone(),
            capture: None,
        };
        assert_eq!(
            flow.transfers[&point].before(&Demand::from_iter([atom(Storage::PendingWrite(write))])),
            Demand::from_iter([atom(Storage::Logical(Local(1)))])
        );
    }
}

#[test]
fn only_a_destination_written_on_every_normal_path_can_replace_memory() {
    use crate::provenance::effects::MemoryWrite;

    let mut table = TypeTable::new();
    let ty = table.intern(Ty::Int(IntWidth::I64));
    let function = function(&[ty, ty, ty]);
    let flow = Flow::new(&function, &table, None);
    let state = State::default();
    let point = Point::new(BlockId::ENTRY, 0);
    let store = |local| MemoryWrite {
        target: flow.reference_metadata(&Place::from_local(Local(local)), point, &state),
        ty,
        value: Value::default(),
    };

    let all_paths = flow.normalize_final_writes(
        NormalMemory {
            alternatives: vec![vec![store(1), store(2)], vec![store(1)]],
            ..Default::default()
        },
        point,
    );
    assert_eq!(all_paths.exact.len(), 1);
    assert!(matches!(
        all_paths.exact.keys().next().unwrap().memory,
        Memory::Frame {
            local: Local(1),
            ..
        }
    ));
    assert_eq!(all_paths.weak.len(), 1);

    let skipped = flow.normalize_final_writes(
        NormalMemory {
            alternatives: vec![vec![store(1)], vec![]],
            ..Default::default()
        },
        point,
    );
    assert!(skipped.exact.is_empty());
    assert_eq!(skipped.weak.len(), 1);
}

#[test]
fn unresolved_nested_settlement_preserves_an_earlier_borrow() {
    let mut table = TypeTable::new();
    let ty = table.intern(Ty::Int(IntWidth::I64));
    let function = function(&[ty, ty, ty]);
    let mut flow = Flow::new(&function, &table, None);
    let point = Point::new(BlockId::ENTRY, 0);
    let mut state = State::default();
    flow.ensure_transfer(point, &state);
    let (_, parent) = tasks::birth(function.def_id, point, ty);
    let actor = Subject::from(parent.futures[&Vec::new()].iter().next().unwrap());
    let known = lane(&function, point, &actor, Local(1));
    let known_sample = known.location.clone();
    let before = Value::reference(Origin::Loan(point));
    flow.store_settled_sample(known_sample.clone(), before.clone(), point, &mut state);

    let request = flow
        .reserve_completions(vec![state.future_copies.witness(Local(2))], point)
        .pop()
        .unwrap();
    let destination = flow.physical_sample(&known_sample).unwrap();
    flow.stage_final_write(
        tasks::Role::root(Local(2)),
        known_sample.clone(),
        destination,
        Vec::new(),
        Value::default(),
        point,
    );
    request
        .settle(&mut flow, point)
        .unwrap()
        .finish(&mut flow, None, point, &mut state);

    let mut without_nested = state.clone();
    let saved_scope = flow.completion_scope.clone();
    flow.finish_final_writes(point, &mut without_nested);
    assert_ne!(
        without_nested.settled_memory.get(&known_sample),
        Some(&before)
    );
    flow.completion_scope = saved_scope;

    let mut unresolved = (*known).clone();
    unresolved.location.memory = Memory::Unavailable(unresolved.event);
    let nested = PreparedMemory {
        requests: Vec::new(),
        facets: vec![PreparedFacet {
            emitter: None,
            interference: Effects::default(),
            prefix: vec![(Arc::new(unresolved), Value::default())],
            normal: NormalMemory::default(),
        }],
    };
    flow.commit_task_memory(nested, point, &mut state);
    flow.finish_final_writes(point, &mut state);
    assert_eq!(state.settled_memory.get(&known_sample), Some(&before));
}

#[test]
fn helper_clear_keeps_the_new_source_of_the_same_loan() {
    use crate::provenance::demand::Source;

    let mut table = TypeTable::new();
    let ty = table.intern(Ty::Int(IntWidth::I64));
    let function = function(&[ty, ty, ty, ty]);
    let mut flow = Flow::new(&function, &table, None);
    let point = Point::new(BlockId::ENTRY, 0);
    let mut state = State::default();
    let origin = Origin::Loan(point);
    let value = Value::reference(origin.clone());
    state.set(Local(2), value.clone());
    state.set(Local(3), value.clone());
    flow.ensure_transfer(point, &state);
    let sample = Sample {
        memory: Memory::Frame {
            scope: FrameScope::Current,
            function: function.def_id,
            local: Local(1),
            ty,
        },
        path: Vec::new(),
        ty,
    };
    let destination = flow.physical_sample(&sample).unwrap();
    flow.store_settled_sample(
        sample.clone(),
        value.clone().sourced(&Storage::Logical(Local(2))),
        point,
        &mut state,
    );
    flow.reserve_completions(Vec::new(), point);
    let new = flow.freeze_sources(point, value.sourced(&Storage::Logical(Local(3))));
    let (_, mut writer) = tasks::birth(function.def_id, point, ty);
    let actor = Subject::from(writer.futures[&Vec::new()].iter().next().unwrap());
    let publication = lane(&function, point, &actor, Local(1));
    let view = writer
        .task_views
        .entry(Vec::new())
        .or_default()
        .entry(actor)
        .or_default();
    view.memory.import(publication.clone(), new.clone());
    flow.record_path_store(
        point,
        Coordinate {
            storage: Storage::PendingWrite(publication.clone()),
            path: Vec::new(),
        },
        &new,
        false,
    );
    state.pending_memory.insert(publication, new);
    flow.preserve_pending_loss(&writer, &Vec::new(), point, &mut state, true);
    let observe = |state: &State| {
        state.settled_memory[&sample]
            .clone()
            .sourced(&Storage::SettledMemory(sample.clone()))
    };
    let before = flow.freeze_sources(point, observe(&state));
    assert_eq!(before.all_sources().len(), 2);

    flow.clear_entry_settled_memory(&destination, point, &mut state);

    let after = flow.freeze_sources(point, observe(&state));
    assert_eq!(
        after.all_sources(),
        FxHashSet::from_iter([Source::Entry(Atom {
            coordinate: Coordinate {
                storage: Storage::Logical(Local(3)),
                path: Vec::new()
            },
            origin,
            capture: None,
        })])
    );
    assert_eq!(
        before.all_sources().len(),
        2,
        "a previously saved read retains both sources"
    );
}

#[test]
fn helper_field_clear_keeps_new_data_and_the_untouched_sibling() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let ty = table.intern(Ty::Tuple(vec![int, int]));
    let function = function(&[int, ty, int, int, int]);
    let point = Point::new(BlockId::ENTRY, 0);
    let mut state = State::default();
    let loan = |index| Value::reference(Origin::Loan(Point::new(BlockId::ENTRY, index)));
    for index in 2..=4 {
        state.set(Local(index), loan(index));
    }
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    let sample = Sample {
        memory: Memory::Frame {
            scope: FrameScope::Current,
            function: function.def_id,
            local: Local(1),
            ty,
        },
        path: Vec::new(),
        ty,
    };
    let field = crate::provenance::Slot::Tuple;
    let sourced = |local| {
        state
            .value(Local(local))
            .sourced(&Storage::Logical(Local(local)))
    };
    let mut before = sourced(2).prefixed(field(0), ty, &table);
    before.join(&sourced(3).prefixed(field(1), ty, &table));
    let fresh = flow.freeze_sources(point, sourced(4).prefixed(field(0), ty, &table));
    flow.store_settled_sample(sample.clone(), before, point, &mut state);
    flow.reserve_completions(Vec::new(), point);
    let root = flow.physical_sample(&sample).unwrap();
    flow.trace_completion_import(root.clone(), point);
    flow.store_settled_sample(sample.clone(), fresh, point, &mut state);
    let destination = Coordinate {
        storage: root.storage,
        path: vec![field(0)],
    };

    flow.clear_entry_settled_memory(&destination, point, &mut state);

    let after = &state.settled_memory[&sample];
    assert_eq!(after.project(&field(0)).origins(), loan(4).origins());
    assert_eq!(after.project(&field(1)).origins(), loan(3).origins());
}

#[test]
fn staged_final_value_retires_its_helper_frame_before_installation() {
    use crate::provenance::address::{Endpoint, FrameLifetime};
    use crate::provenance::value::Leaf;

    let ty = TypeId::ERROR;
    let table = TypeTable::new();
    let function = function(&[ty, ty, ty]);
    let mut flow = Flow::new(&function, &table, None);
    let point = Point::new(BlockId::ENTRY, 0);
    let boundary = OpaqueSite {
        function: function.def_id,
        point,
    };
    let mut state = State::default();
    flow.ensure_transfer(point, &state);
    flow.frame_boundaries.insert(boundary);
    let sample = Sample {
        memory: Memory::Frame {
            scope: FrameScope::Current,
            function: function.def_id,
            local: Local(1),
            ty,
        },
        path: Vec::new(),
        ty,
    };
    let mut endpoint = Endpoint::Frame {
        lifetime: FrameLifetime::DuringCall {
            boundary,
            owner_return: boundary,
        },
        function: DefId::new(10011),
        root: point,
        local: Local(1),
        root_ty: ty,
        pointee: ty,
        location: Vec::new(),
    };
    let value = Value::reference(Origin::Loan(point)).with_metadata(&Leaf {
        endpoints: FxHashSet::from_iter([endpoint.clone()]),
        ..Leaf::default()
    });
    let request = flow
        .reserve_completions(vec![state.future_copies.witness(Local(2))], point)
        .pop()
        .unwrap();
    flow.stage_final_write(
        tasks::Role::root(Local(2)),
        sample.clone(),
        flow.physical_sample(&sample).unwrap(),
        Vec::new(),
        value,
        point,
    );
    request
        .settle(&mut flow, point)
        .unwrap()
        .finish(&mut flow, None, point, &mut state);
    flow.retire_call_frames(point, &mut state);
    flow.finish_final_writes(point, &mut state);

    if let Endpoint::Frame { lifetime, .. } = &mut endpoint {
        *lifetime = FrameLifetime::Returned(boundary);
    }
    let installed = &state.settled_memory[&sample];
    assert!(installed
        .leaves
        .values()
        .any(|leaf| leaf.endpoints.contains(&endpoint)));
}
