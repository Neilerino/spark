// TAG [SPARK-114]

use rustc_hash::FxHashMap;
use spark_types::TypeId;
use std::collections::VecDeque;

use super::located_records;
use crate::provenance::{
    demand::{Coordinate, Storage},
    tasks::{
        views::{Subject, View},
        Record,
    },
    Flow, State, Value,
};

pub(super) struct Import {
    pub at: Coordinate,
    pub actor: Subject,
    pub record: Record,
}

pub(in crate::provenance) struct Activity {
    carriers: FxHashMap<Storage, Value>,
    pub(super) imports: FxHashMap<(Coordinate, Subject, TypeId), Record>,
}

impl Activity {
    pub fn into_carriers(self) -> impl Iterator<Item = (Storage, Value)> {
        self.carriers.into_iter()
    }

    pub fn views(&self) -> impl Iterator<Item = &View> {
        self.carriers.values().flat_map(|value| {
            value.futures.iter().flat_map(move |(path, handles)| {
                handles
                    .iter()
                    .filter(|handle| handle.pending)
                    .filter_map(move |handle| {
                        value
                            .task_views
                            .get(path)
                            .and_then(|subjects| subjects.get(&Subject::from(handle)))
                    })
            })
        })
    }

    pub(super) fn imports(&self) -> impl Iterator<Item = Import> + '_ {
        self.imports.iter().map(|((at, actor, _), record)| Import {
            at: at.clone(),
            actor: actor.clone(),
            record: record.clone(),
        })
    }
}

impl State {
    pub(in crate::provenance) fn rooted_activity(&self) -> Activity {
        let mut pending: VecDeque<_> =
            self.values
                .iter()
                .filter(|(local, _)| self.live.contains(local))
                .map(|(local, value)| (Storage::Logical(*local), value.clone()))
                .chain(
                    self.external
                        .iter()
                        .map(|(port, value)| (Storage::External(port.clone()), value.clone())),
                )
                .chain(self.queues.iter().map(|((target, ty), value)| {
                    (Storage::Queue(target.clone(), *ty), value.clone())
                }))
                .chain(self.executions.iter().map(|((local, ty), value)| {
                    (Storage::ExecutionCallback(*local, *ty), value.clone())
                }))
                .chain(
                    self.residuals
                        .iter()
                        .map(|(key, value)| (Storage::Residual(key.clone()), value.clone())),
                )
                .chain(
                    self.settled_memory.iter().map(|(sample, value)| {
                        (Storage::SettledMemory(sample.clone()), value.clone())
                    }),
                )
                .collect();
        let mut result = Activity {
            carriers: FxHashMap::default(),
            imports: FxHashMap::default(),
        };
        while let Some((storage, value)) = pending.pop_front() {
            if let Some(current) = result.carriers.get_mut(&storage) {
                if !current.join(&value) {
                    continue;
                }
            } else {
                result.carriers.insert(storage.clone(), value);
            }
            let carrier = result.carriers[&storage].clone().sourced(&storage);
            for (path, handles) in &carrier.futures {
                for handle in handles.iter().filter(|handle| handle.pending) {
                    let actor = Subject::from(handle);
                    if let Some(view) = carrier
                        .task_views
                        .get(path)
                        .and_then(|subjects| subjects.get(&actor))
                    {
                        for (lane, sample) in view.memory.samples() {
                            if self.pending_memory.contains_key(lane) {
                                pending.push_back((
                                    Storage::PendingWrite(lane.clone()),
                                    Flow::sample_pending_write(lane, sample, self),
                                ));
                            }
                        }
                    }
                    let (records, _) = located_records(&carrier, path, handle, self, &storage);
                    for located in records {
                        if let Some(at) = located.import_at {
                            let key = (at, actor.clone(), located.record.callback_ty);
                            if let Some(record) = result.imports.get_mut(&key) {
                                record.join(&located.record);
                            } else {
                                result.imports.insert(key, located.record.clone());
                            }
                        }
                        pending.push_back((located.storage, located.record.callback));
                    }
                }
            }
        }
        result
    }
}
