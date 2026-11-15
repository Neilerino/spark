// TAG [SPARK-114]

//! Possible queue effects close at an existing MIR point, without replaying task bodies.

use super::{
    views::{self, Subject},
    Handle, Target,
};
use crate::provenance::{
    demand::{Coordinate, Storage},
    effects::MemoryEvent,
    observations::{Envelope, History, ReadView},
    value, Flow, Path, Point, Slot, State, Value,
};
use rustc_hash::FxHashSet;

mod projections;
use projections::{GuardedReads, Publications};
use spark_types::TypeId;
use std::cell::RefCell;

#[derive(Default)]
struct TaskQuery {
    writes: Vec<crate::provenance::channels::QueueEvent>,
    result: Value,
    normal_return: bool,
    history: Option<History>,
    memory: super::memory::Delta,
    memory_support: bool,
    unknown_memory_write: bool,
    residuals: Vec<super::residual::Import>,
}

#[derive(Default)]
struct TaskQueries {
    writes: Vec<crate::provenance::channels::QueueEvent>,
    results: Vec<(Storage, TypeId, Path, Value)>,
    histories: Vec<(Storage, Path, Subject, History)>,
    memories: Vec<(Storage, Path, Subject, super::memory::Delta, bool, bool)>,
    residuals: Vec<super::residual::Import>,
}

impl Flow<'_> {
    pub(in crate::provenance) fn settle_pending_tasks(&mut self, point: Point, state: &mut State) {
        self.settle_pending_tasks_in(point, state, &[], &[], &[]);
    }

    pub(in crate::provenance) fn settle_pending_tasks_in(
        &mut self,
        point: Point,
        state: &mut State,
        events: &[MemoryEvent],
        queues: &[crate::provenance::channels::QueueEvent],
        residuals: &[super::residual::Import],
    ) {
        let Some(guarded) = GuardedReads::new(self, events, queues, residuals, state) else {
            self.close_task_effects(
                point,
                state,
                events,
                queues,
                residuals,
                &mut Publications::default(),
            );
            return;
        };
        let entry = self.transfers[&point].clone();
        let source_scope = self.completion_scope.clone();
        let mut candidate = state.clone();
        if self.close_task_effects(
            point,
            &mut candidate,
            events,
            queues,
            residuals,
            &mut Publications::probe(),
        ) {
            *state = candidate;
            return;
        }
        self.completion_scope = source_scope;
        self.transfers.insert(point, entry);
        let (closed, transfer) = guarded.settle(self, point);
        *state = closed;
        self.transfers.insert(point, transfer);
    }

    fn close_task_effects(
        &mut self,
        point: Point,
        state: &mut State,
        events: &[MemoryEvent],
        queues: &[crate::provenance::channels::QueueEvent],
        residuals: &[super::residual::Import],
        publications: &mut Publications<'_>,
    ) -> bool {
        self.import_residuals(residuals, point, state);
        loop {
            let entry_relation = self.transfers[&point].clone();
            let entry_basis = state.activity_basis();
            let previous_queues = state.queues.clone();
            let TaskQueries {
                writes,
                results,
                histories,
                mut memories,
                mut residuals,
            } = self.query_active_tasks(point, state, events, queues, publications);
            let before_publications = publications
                .is_probe()
                .then(|| self.transfers[&point].clone());
            for (_, _, _, delta, _, _) in &mut memories {
                self.freeze_memory_delta(delta, point);
            }
            for residual in &mut residuals {
                residual.freeze_sources(self, point);
            }
            let mut changed = false;
            for residual in residuals {
                changed |= self.import_residual(residual, point, state);
            }
            for (storage, path, actor, delta, support, unknown) in memories {
                changed |= self.cache_pending_memory(
                    &storage,
                    path,
                    actor,
                    (delta, support, unknown),
                    point,
                    state,
                );
            }
            for write in writes {
                self.apply_queue_event(&write, point, state);
            }
            for (storage, ty, path, result) in results {
                changed |= self.cache_task_result(&storage, ty, path, &result, point, state);
            }
            changed |= previous_queues != state.queues;
            for (storage, path, subject, history) in histories {
                changed |= Self::cache_task_history(&storage, path, subject, &history, state);
            }
            if before_publications.is_some_and(|before| {
                changed
                    || entry_basis != state.activity_basis()
                    || !before.same_sources(&self.transfers[&point])
            }) {
                return false;
            }
            if !changed
                && entry_basis == state.activity_basis()
                && entry_relation == self.transfers[&point]
            {
                return true;
            }
        }
    }

    fn query_active_tasks(
        &mut self,
        point: Point,
        state: &mut State,
        events: &[MemoryEvent],
        queues: &[crate::provenance::channels::QueueEvent],
        publications: &mut Publications<'_>,
    ) -> TaskQueries {
        let mut queries = TaskQueries::default();
        let activity = self.prepare_task_activity(point, state);
        for (storage, carrier) in activity.into_carriers() {
            let root_ty = self.storage_type(&storage);
            let carrier = carrier.sourced(&storage);
            for (path, handles) in &carrier.futures {
                let mut seen = FxHashSet::default();
                for handle in handles.iter().filter(|h| h.pending) {
                    if !seen.insert(Subject::from(handle))
                        || matches!(handle.target, Target::Validation { .. })
                    {
                        continue;
                    }
                    let mut query = publications
                        .projected(self, point, state, &storage, path, handle)
                        .unwrap_or_else(|| {
                            let envelope = self.prefix_envelope(events, queues, &storage, path);
                            self.query_pending_task(
                                (&carrier, &storage),
                                path,
                                handle,
                                point,
                                state,
                                &envelope,
                            )
                        });
                    publications.record(&storage, path, handle, &query);
                    for mut write in query.writes.drain(..) {
                        if !self.route_producer_queue_event(
                            &storage, path, handle, &mut write, point, state,
                        ) {
                            queries.writes.push(write);
                        }
                    }
                    queries.residuals.append(&mut query.residuals);
                    if query.memory_support
                        || query.unknown_memory_write
                        || !query.memory.is_empty()
                    {
                        queries.memories.push((
                            storage.clone(),
                            path.clone(),
                            Subject::from(handle),
                            query.memory,
                            query.memory_support,
                            query.unknown_memory_write,
                        ));
                    }
                    if query.normal_return {
                        queries.results.push((
                            storage.clone(),
                            root_ty,
                            path.clone(),
                            query.result,
                        ));
                    }
                    if let Some(history) = query.history {
                        queries.histories.push((
                            storage.clone(),
                            path.clone(),
                            Subject::from(handle),
                            history,
                        ));
                    }
                }
            }
        }
        queries
    }

    fn query_pending_task(
        &mut self,
        (carrier, carrier_storage): (&Value, &Storage),
        path: &Path,
        handle: &Handle,
        point: Point,
        state: &mut State,
        envelope: &Envelope,
    ) -> TaskQuery {
        let mut query = TaskQuery::default();
        let Some(inner) = value::child_type(handle.target.ty(), &Slot::Element, self.table) else {
            return query;
        };
        let subject = Subject::from(handle);
        let (records, unknown) =
            views::pending_records_at(carrier, path, handle, state, carrier_storage);
        if unknown {
            query.memory_support = true;
            query.unknown_memory_write = true;
            self.unknown_memory_write = true;
            let mut accesses = crate::provenance::Accesses::default();
            accesses.observe_reference(
                &carrier.task_lineage(),
                None,
                &[],
                crate::provenance::AccessKind::Value,
            );
            self.record_accesses(point, accesses, state);
        }
        let mut history = views::contribution(carrier, path, handle).history;
        let resampled =
            self.resample_history(&history, ReadView::current(state).with_envelope(envelope));
        let mut changed = self.remember_samples(resampled, &mut history, point, state);
        let trace = RefCell::default();
        for record in records {
            let read = ReadView::task(state, &subject, &history, &trace).with_envelope(envelope);
            let mut facts = self.task_facts_in(&record, &handle.target, point, read, inner);
            self.import_prefix_observations(
                &mut facts.prefix.memory,
                &mut facts.prefix.queues,
                point,
                state,
            );
            query.memory_support |= facts.memory_support;
            query.unknown_memory_write |= facts.unknown_memory_write;
            query
                .memory
                .extend(self.task_memory_delta(&facts.prefix.memory, &subject));
            self.observe_task_prefix(&facts.prefix, point, state);
            query.residuals.extend(facts.prefix.residuals);
            self.expose_prefix_memory(&facts.prefix.memory, state);
            for mut write in facts.prefix.queues {
                write.write.value = self.freeze_sources(point, write.write.value);
                query.writes.push(write);
            }
            if facts.normal_return {
                query.normal_return = true;
                query
                    .result
                    .join(&self.freeze_sources(point, facts.returned));
            }
        }
        changed |= self.remember_samples(trace.into_inner(), &mut history, point, state);
        query.history = changed.then_some(history);
        query
    }

    fn cache_task_history(
        storage: &Storage,
        path: Path,
        subject: Subject,
        history: &History,
        state: &mut State,
    ) -> bool {
        let Some(mut current) = state
            .stored_value(storage)
            .map(std::borrow::Cow::into_owned)
        else {
            return false;
        };
        let view = current
            .task_views
            .entry(path)
            .or_default()
            .entry(subject)
            .or_insert_with(views::View::unknown);
        if !view.history.join(history) {
            return false;
        }
        Self::replace_task_carrier(storage, current, state)
    }

    #[cfg(test)]
    pub(in crate::provenance) fn task_carriers(
        &self,
        state: &State,
    ) -> Vec<(Storage, TypeId, Value)> {
        state
            .rooted_activity()
            .into_carriers()
            .map(|(storage, value)| {
                let ty = self.storage_type(&storage);
                (storage, ty, value)
            })
            .collect()
    }

    fn cache_task_result(
        &mut self,
        storage: &Storage,
        root_ty: TypeId,
        mut path: Path,
        result: &Value,
        point: Point,
        state: &mut State,
    ) -> bool {
        let mut result = self.freeze_sources(point, result.clone());
        self.import_task_sources(&mut result, point, state);
        let result = &result;
        path.push(Slot::Element);
        let mut current = state
            .stored_value(storage)
            .map(std::borrow::Cow::into_owned)
            .unwrap_or_default();
        self.record_path_store(
            point,
            Coordinate {
                storage: storage.clone(),
                path: path.clone(),
            },
            result,
            false,
        );
        if let Storage::Logical(local) = storage {
            self.record_path_store(
                point,
                Coordinate {
                    storage: Storage::Cell(*local),
                    path: path.clone(),
                },
                result,
                false,
            );
        }
        let mut cached = result.prefixed_path(&path, root_ty, self.table);
        cached.clear_sources();
        if !current.join(&cached) {
            return false;
        }
        current.clear_sources();
        Self::replace_task_carrier(storage, current, state)
    }

    pub(in crate::provenance) fn replace_task_carrier(
        storage: &Storage,
        current: Value,
        state: &mut State,
    ) -> bool {
        match storage {
            Storage::CallbackNode(location) => {
                let Some(mut root) = state
                    .stored_value(location.root())
                    .map(std::borrow::Cow::into_owned)
                else {
                    return false;
                };
                let Some(callback) = root
                    .task_views
                    .get_mut(location.path())
                    .and_then(|subjects| subjects.get_mut(location.actor()))
                    .and_then(|view| view.callbacks.get_mut(&location.callback_type()))
                else {
                    return false;
                };
                if !callback.replace_node(location.node(), current) {
                    return false;
                }
                return Self::replace_task_carrier(location.root(), root, state);
            }
            Storage::Observation(_) => return false,
            Storage::PendingWrite(lane) => {
                state.pending_memory.insert(lane.clone(), current);
            }
            Storage::SettledMemory(sample) => {
                state.settled_memory.insert(sample.clone(), current);
            }
            Storage::ResidualCallback(key, ty) => {
                state.residual_callbacks.insert((key.clone(), *ty), current);
            }
            Storage::Residual(key) => {
                state.residuals.insert(key.clone(), current);
            }
            Storage::Logical(local) => {
                state.set(*local, current);
            }
            Storage::Cell(local) => {
                state.cells.insert(*local, current);
            }
            Storage::External(port) => {
                state.external.insert(port.clone(), current);
            }
            Storage::Queue(target, ty) => {
                state.queues.insert((target.clone(), *ty), current);
            }
            Storage::QueueCurrent(slot) => {
                let Some(message) = state.queue_current.get_mut(slot) else {
                    return false;
                };
                message.value = current;
            }
            Storage::Task(target, _) => {
                let Some(record) = state.tasks.get_mut(target) else {
                    return false;
                };
                record.callback = current;
            }
            Storage::ExecutionCallback(local, ty) => {
                state.executions.insert((*local, *ty), current);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests;
