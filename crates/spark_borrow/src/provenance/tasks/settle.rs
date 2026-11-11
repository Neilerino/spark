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
}

impl Flow<'_> {
    pub(in crate::provenance) fn settle_pending_tasks(&mut self, point: Point, state: &mut State) {
        self.settle_pending_tasks_in(point, state, &[], &[]);
    }

    pub(in crate::provenance) fn settle_pending_tasks_in(
        &mut self,
        point: Point,
        state: &mut State,
        events: &[MemoryEvent],
        queues: &[crate::provenance::channels::QueueEvent],
    ) {
        let Some(guarded) = GuardedReads::new(self, events, queues, state) else {
            self.close_task_effects(point, state, events, queues, &mut Publications::default());
            return;
        };
        let entry = self.transfers[&point].clone();
        let mut candidate = state.clone();
        if self.close_task_effects(
            point,
            &mut candidate,
            events,
            queues,
            &mut Publications::probe(),
        ) {
            *state = candidate;
            return;
        }
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
        publications: &mut Publications<'_>,
    ) -> bool {
        loop {
            let entry_relation = self.transfers[&point].clone();
            let previous_queues = state.queues.clone();
            let mut writes = Vec::new();
            let mut results = Vec::new();
            let mut histories = Vec::new();
            for (storage, root_ty, carrier) in self.task_carriers(state) {
                let carrier = carrier.sourced(&storage);
                for (path, handles) in &carrier.futures {
                    let mut queried = FxHashSet::default();
                    for handle in handles.iter().filter(|h| h.pending) {
                        if !queried.insert(Subject::from(handle))
                            || matches!(handle.target, Target::Validation { .. })
                        {
                            continue;
                        }
                        let mut query = publications
                            .projected(self, point, state, &storage, path, handle)
                            .unwrap_or_else(|| {
                                let envelope = self.prefix_envelope(events, queues, &storage, path);
                                self.query_pending_task(
                                    &carrier, path, handle, point, state, &envelope,
                                )
                            });
                        publications.record(&storage, path, handle, &query);
                        writes.append(&mut query.writes);
                        if query.normal_return {
                            results.push((storage.clone(), root_ty, path.clone(), query.result));
                        }
                        if let Some(history) = query.history {
                            histories.push((
                                storage.clone(),
                                path.clone(),
                                Subject::from(handle),
                                history,
                            ));
                        }
                    }
                }
            }
            let before_publications = publications
                .is_probe()
                .then(|| self.transfers[&point].clone());
            let mut changed = false;
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
            if before_publications
                .is_some_and(|before| changed || !before.same_sources(&self.transfers[&point]))
            {
                return false;
            }
            if !changed && entry_relation == self.transfers[&point] {
                return true;
            }
        }
    }

    fn query_pending_task(
        &mut self,
        carrier: &Value,
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
        let (records, unknown) = views::pending_records(carrier, path, handle, state);
        if unknown {
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
        for mut record in records {
            record.callback = record
                .callback
                .sourced(&Storage::Task(handle.target.clone(), record.callback_ty));
            let read = ReadView::task(state, &subject, &history, &trace).with_envelope(envelope);
            let mut facts = self.task_facts_in(&record, &handle.target, point, read, inner);
            self.import_prefix_observations(
                &mut facts.prefix.memory,
                &mut facts.prefix.queues,
                point,
                state,
            );
            self.observe_prefix(&facts.prefix, point, state);
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
        let Some(mut current) = state.stored_value(storage).cloned() else {
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

    fn task_carriers(&self, state: &State) -> Vec<(Storage, TypeId, Value)> {
        state
            .values
            .iter()
            .map(|(local, value)| {
                (
                    Storage::Logical(*local),
                    self.function.locals[local.index()].ty,
                    value.clone(),
                )
            })
            .chain(state.external.iter().map(|(port, value)| {
                (Storage::External(port.clone()), port.pointee, value.clone())
            }))
            .chain(state.queues.iter().map(|((target, ty), value)| {
                (Storage::Queue(target.clone(), *ty), *ty, value.clone())
            }))
            .chain(state.tasks.iter().map(|(target, record)| {
                (
                    Storage::Task(target.clone(), record.callback_ty),
                    record.callback_ty,
                    record.callback.clone(),
                )
            }))
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
        self.import_value_observations(&mut result, point, state);
        let result = &result;
        path.push(Slot::Element);
        let mut current = state.stored_value(storage).cloned().unwrap_or_default();
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
        cached.sources.clear();
        if !current.join(&cached) {
            return false;
        }
        current.sources.clear();
        Self::replace_task_carrier(storage, current, state)
    }

    fn replace_task_carrier(storage: &Storage, current: Value, state: &mut State) -> bool {
        match storage {
            Storage::Observation(_) => return false,
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
            Storage::Task(target, _) => {
                state.tasks.get_mut(target).unwrap().callback = current;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests;
