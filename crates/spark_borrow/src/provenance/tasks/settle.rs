// TAG [SPARK-114]

//! Possible queue effects close at an existing MIR point, without replaying task bodies.

use super::{views::Subject, Target};
use crate::provenance::{
    demand::{Coordinate, Storage},
    value, Flow, Path, Point, Slot, State, Value,
};
use rustc_hash::FxHashSet;
use spark_types::TypeId;

impl Flow<'_> {
    pub(in crate::provenance) fn settle_pending_tasks(&mut self, point: Point, state: &mut State) {
        loop {
            let entry_relation = self.transfers[&point].clone();
            let previous_queues = state.queues.clone();
            let mut writes = Vec::new();
            let mut results = Vec::new();
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
                        let Some(inner) =
                            value::child_type(handle.target.ty(), &Slot::Element, self.table)
                        else {
                            continue;
                        };
                        let (records, unknown) =
                            super::views::pending_records(&carrier, path, handle, state);
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
                        for mut record in records {
                            record.callback = record
                                .callback
                                .sourced(&Storage::Task(handle.target.clone(), record.callback_ty));
                            let facts =
                                self.task_facts(&record, &handle.target, point, state, inner);
                            self.observe_prefix(&facts.prefix, point, state);
                            for mut write in facts.prefix.queues {
                                write.value = self.freeze_sources(point, write.value);
                                writes.push(write);
                            }
                            if facts.normal_return {
                                results.push((
                                    storage.clone(),
                                    root_ty,
                                    path.clone(),
                                    self.freeze_sources(point, facts.returned),
                                ));
                            }
                        }
                    }
                }
            }
            let mut changed = false;
            for write in writes {
                self.queue_send(&write.target, &write.value, write.ty, point, state);
            }
            for (storage, ty, path, result) in results {
                changed |= self.cache_task_result(&storage, ty, path, &result, point, state);
            }
            changed |= previous_queues != state.queues;
            if !changed && entry_relation == self.transfers[&point] {
                break;
            }
        }
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
        match storage {
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
