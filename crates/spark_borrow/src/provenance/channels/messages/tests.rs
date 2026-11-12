// TAG [SPARK-114]

use super::*;
use crate::provenance::{
    demand::{Atom, Coordinate, Demand},
    tasks::{
        birth,
        proofs::EventFence,
        views::{Subject, View},
    },
    Origin, Slot as ValueSlot,
};
use spark_mir::{BlockId, LocalDecl, MirFunction, Place};
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::{IntWidth, LifetimeId, Ty, TypeTable};

fn with_messages(run: impl FnOnce(&mut Flow<'_>, State, super::super::Target, Origin, Origin)) {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let future = table.intern(Ty::Future { inner: int });
    let channel = table.intern(Ty::Channel { inner: future });
    let callback_ty = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let function = MirFunction {
        def_id: DefId::new(9978),
        param_count: 0,
        return_type: int,
        locals: [int, channel, future, future, future]
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
    let mut state = State::default();
    state.set(
        Local(1),
        super::super::allocation(function.def_id, Point::new(BlockId::ENTRY, 0), channel),
    );
    state.channel_copies.seed(Local(1));
    let queue = state
        .value(Local(1))
        .root_queue_handles()
        .next()
        .unwrap()
        .target
        .clone();
    let mut origins = Vec::new();
    for (local, step) in [(Local(2), 1), (Local(3), 2)] {
        let origin = Origin::Loan(Point::new(BlockId::ENTRY, step));
        let (target, mut payload) =
            birth(function.def_id, Point::new(BlockId::ENTRY, step), future);
        let callback_def = DefId::new(9979);
        let mut callback = Value::function(callback_def);
        callback.join(&Value::reference(origin.clone()).prefixed(
            ValueSlot::Capture(callback_def, 0, reference),
            callback_ty,
            &table,
        ));
        payload.join(&callback.task_lineage());
        payload.task_views.entry(Vec::new()).or_default().insert(
            Subject {
                target,
                capture: None,
            },
            View::pending(callback, callback_ty),
        );
        state.set(local, payload);
        origins.push(origin);
    }
    run(
        &mut Flow::new(&function, &table, None),
        state,
        queue,
        origins.remove(0),
        origins.remove(0),
    );
}

fn event(
    flow: &Flow<'_>,
    state: &State,
    queue: &super::super::Target,
    source: Local,
) -> super::super::QueueEvent {
    super::super::QueueEvent {
        site: OpaqueSite {
            function: flow.function.def_id,
            point: Point::new(BlockId::ENTRY, 10),
        },
        write: super::super::Write {
            target: queue.clone(),
            ty: flow.function.locals[source.index()].ty,
            value: state.value(source).sourced(&Storage::Logical(source)),
        },
        fence: EventFence::default(),
    }
}

fn atom(storage: Storage, value: &Value, origin: &Origin) -> Atom {
    let (path, _, capture) = value
        .leaves
        .keys()
        .find(|(_, found, _)| found == origin)
        .unwrap();
    Atom {
        coordinate: Coordinate {
            storage,
            path: path.clone(),
        },
        origin: origin.clone(),
        capture: *capture,
    }
}

#[test]
fn an_actual_send_owns_one_writable_contribution_and_preserves_its_entry_sources() {
    with_messages(|flow, mut state, queue, origin, _| {
        let point = Point::new(BlockId::ENTRY, 10);
        flow.ensure_transfer(point, &state);
        let event = event(flow, &state, &queue, Local(2));
        flow.apply_direct_queue_send(Local(1), &[event], point, &mut state);
        assert!(
            state.queues.is_empty(),
            "current contribution must not also be an independent history root"
        );
        assert_eq!(state.queue_current.len(), 1);
        let (slot, message) = state.queue_current.iter().next().unwrap();
        let slot = slot.clone();
        let stored = Storage::QueueCurrent(slot.clone());
        assert!(message.value.origins().contains(&origin));
        let read = flow.queue_payload(&state.value(Local(1)), slot.ty, &state);
        assert!(read.origins().contains(&origin));
        flow.record_store(point, &Place::from_local(Local(4)), &read);
        assert_eq!(
            flow.transfers[&point].before(&Demand::from_iter([atom(
                Storage::Logical(Local(4)),
                &read,
                &origin
            )])),
            Demand::from_iter([atom(
                Storage::Logical(Local(2)),
                &state.value(Local(2)),
                &origin
            )])
        );
        assert!(state
            .rooted_activity()
            .into_carriers()
            .any(|(storage, value)| storage == stored
                && value
                    .futures
                    .values()
                    .flatten()
                    .any(|handle| handle.pending)));
        let actor = Subject::from(
            state.queue_current[&slot].value.futures[&Vec::new()]
                .iter()
                .next()
                .unwrap(),
        );
        let before = state.activity_basis();
        assert!(flow.cache_pending_memory(
            &stored,
            Vec::new(),
            actor.clone(),
            (Vec::new(), true),
            point,
            &mut state
        ));
        assert!(before != state.activity_basis());
        assert!(
            state.queue_current[&slot].value.task_views[&Vec::new()][&actor]
                .memory
                .may_write
        );
        assert!(
            !state.future_owners.known(Local(4)),
            "storage alone grants no receipt ownership"
        );
    });
}

#[test]
fn repeating_a_real_send_keeps_old_activity_and_sources_in_history() {
    with_messages(|flow, mut state, queue, old, new| {
        let first = Point::new(BlockId::ENTRY, 10);
        let sent = event(flow, &state, &queue, Local(2));
        flow.ensure_transfer(first, &state);
        flow.apply_direct_queue_send(Local(1), &[sent], first, &mut state);
        assert_eq!(state.queue_current.len(), 1);
        let slot = state.queue_current.keys().next().unwrap().clone();
        let previous = state.queue_current[&slot].value.clone();
        let point = Point::new(BlockId::ENTRY, 20);
        let sent = event(flow, &state, &queue, Local(3));
        flow.ensure_transfer(point, &state);
        flow.apply_direct_queue_send(Local(1), &[sent], point, &mut state);
        assert!(state.queue_current[&slot].value.origins().contains(&new));
        assert!(!state.queue_current[&slot].value.origins().contains(&old));
        let history = &state.queues[&(queue.clone(), slot.ty)];
        assert!(history.origins().contains(&old));
        assert!(!history.origins().contains(&new));
        assert_eq!(
            flow.transfers[&point].before(&Demand::from_iter([atom(
                Storage::Queue(queue.clone(), slot.ty),
                history,
                &old
            )])),
            Demand::from_iter([atom(Storage::QueueCurrent(slot.clone()), &previous, &old)])
        );
        assert!(state
            .rooted_activity()
            .into_carriers()
            .any(|(storage, value)| matches!(storage, Storage::Queue(_, _))
                && value.origins().contains(&old)));
        let read = flow.queue_payload(&state.value(Local(1)), slot.ty, &state);
        assert!(read.origins().contains(&old) && read.origins().contains(&new));
    });
}

#[test]
fn unassociated_samples_do_not_replace_a_current_contribution() {
    with_messages(|flow, mut state, queue, old, new| {
        let point = Point::new(BlockId::ENTRY, 10);
        flow.ensure_transfer(point, &state);
        let sent = event(flow, &state, &queue, Local(2));
        flow.apply_direct_queue_send(Local(1), &[sent], point, &mut state);
        assert_eq!(state.queue_current.len(), 1);
        let before = state.queue_current.clone();
        let sample = event(flow, &state, &queue, Local(3));
        flow.apply_queue_event(&sample, point, &mut state);
        flow.apply_queue_event(&sample, point, &mut state);
        assert_eq!(state.queue_current, before);
        let history = &state.queues[&(queue, flow.function.locals[2].ty)];
        assert!(!history.origins().contains(&old));
        assert!(history.origins().contains(&new));
    });
}

#[test]
fn a_send_import_that_clobbers_its_receiver_archives_both_messages() {
    use crate::provenance::{
        observations::{FrameScope, Memory, Sample},
        tasks::memory::Lane,
    };
    with_messages(|flow, mut state, queue, old, new| {
        let first = Point::new(BlockId::ENTRY, 10);
        flow.ensure_transfer(first, &state);
        let sent = event(flow, &state, &queue, Local(2));
        flow.apply_direct_queue_send(Local(1), &[sent], first, &mut state);
        let slot = state.queue_current.keys().next().unwrap().clone();
        let previous = state.queue_current[&slot].value.clone();
        let point = Point::new(BlockId::ENTRY, 20);
        flow.ensure_transfer(point, &state);
        let mut sent = event(flow, &state, &queue, Local(3));
        let channel_ty = flow.function.locals[1].ty;
        let (actor, view) = sent
            .write
            .value
            .task_views
            .get_mut(&Vec::new())
            .unwrap()
            .iter_mut()
            .next()
            .unwrap();
        view.memory.import(
            std::sync::Arc::new(Lane {
                event: OpaqueSite {
                    function: flow.function.def_id,
                    point,
                },
                actor: actor.clone(),
                location: Sample {
                    memory: Memory::Frame {
                        scope: FrameScope::Current,
                        function: flow.function.def_id,
                        local: Local(1),
                        ty: channel_ty,
                    },
                    path: Vec::new(),
                    ty: channel_ty,
                },
            }),
            super::super::allocation(flow.function.def_id, point, channel_ty),
        );
        flow.apply_direct_queue_send(Local(1), &[sent], point, &mut state);
        assert!(state.invalidated_copies.contains(&Local(1)));
        assert!(
            state.queue_current.is_empty(),
            "invalidated current coordinates must be retired after archival"
        );
        let history = &state.queues[&(queue.clone(), slot.ty)];
        assert!(history.origins().contains(&old) && history.origins().contains(&new));
        assert_eq!(
            flow.transfers[&point].before(&Demand::from_iter([atom(
                Storage::Queue(queue, slot.ty),
                history,
                &old
            ),])),
            Demand::from_iter([atom(Storage::QueueCurrent(slot.clone()), &previous, &old),])
        );
        assert!(flow.transfers[&point]
            .before(&Demand::from_iter([atom(
                Storage::QueueCurrent(slot),
                &previous,
                &old
            ),]))
            .is_empty());
    });
}
