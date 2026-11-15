// TAG [SPARK-114]

use super::Location;
use crate::provenance::{
    demand::{Coordinate, Storage},
    tasks::{views::Subject, Handle, Record},
    State, Value,
};

pub(in crate::provenance) struct Located {
    pub storage: Storage,
    pub record: Record,
    pub import_at: Option<Coordinate>,
}

pub(in crate::provenance) fn located_records(
    value: &Value,
    path: &[crate::provenance::Slot],
    handle: &Handle,
    state: &State,
    storage: &Storage,
) -> (Vec<Located>, bool) {
    if !handle.pending {
        return (Vec::new(), false);
    }
    if path.is_empty() {
        if let Storage::Logical(local) = storage {
            if state
                .values
                .get(local)
                .and_then(|value| value.futures.get(path))
                .is_some_and(|handles| handles.contains(handle))
            {
                if let Some((storage, record)) = state.current_execution(*local) {
                    return (
                        vec![Located {
                            storage,
                            record,
                            import_at: None,
                        }],
                        false,
                    );
                }
            }
        }
    }
    let actor = Subject::from(handle);
    let view = value
        .task_views
        .get(path)
        .and_then(|subjects| subjects.get(&actor));
    let mut records = Vec::new();
    if let Some(view) = view {
        for (ty, callback) in &view.callbacks {
            records.push(Located {
                storage: Storage::CallbackNode(Box::new(Location::new(
                    storage,
                    path.to_vec(),
                    actor.clone(),
                    *ty,
                ))),
                record: Record {
                    callback: callback.value(),
                    callback_ty: *ty,
                },
                import_at: None,
            });
        }
    }
    if let Storage::Residual(key) = storage {
        if path.is_empty() && key.actor == actor {
            for ((owner, ty), callback) in &state.residual_callbacks {
                if owner == key {
                    let storage = Storage::ResidualCallback(key.clone(), *ty);
                    records.push(Located {
                        record: Record {
                            callback: callback.clone().sourced(&storage),
                            callback_ty: *ty,
                        },
                        storage,
                        import_at: None,
                    });
                }
            }
        }
    }
    let mut unknown = false;
    if view.is_none_or(|view| view.fallback) {
        if let Some(record) = state.tasks.get(&handle.target) {
            records.push(Located {
                storage: Storage::CallbackNode(Box::new(Location::new(
                    storage,
                    path.to_vec(),
                    actor,
                    record.callback_ty,
                ))),
                record: Record {
                    callback: record
                        .callback
                        .clone()
                        .sourced(&Storage::Task(handle.target.clone(), record.callback_ty)),
                    callback_ty: record.callback_ty,
                },
                import_at: Some(Coordinate {
                    storage: storage.clone(),
                    path: path.to_vec(),
                }),
            });
        } else {
            unknown = true;
        }
    }
    (records, unknown)
}
