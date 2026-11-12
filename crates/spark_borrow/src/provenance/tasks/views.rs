// TAG [SPARK-114]

//! Flat callback facts on current Future contributions; handles retain graph edges.

use super::{Capture, Handle, Target};
use crate::provenance::{
    value::{Contribution, Leaf},
    Path, Value,
};
use rustc_hash::FxHashMap;
use spark_types::TypeId;

mod callback;
pub(in crate::provenance) use callback::{Callback, NodeKey};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) struct Subject {
    pub target: Target,
    pub capture: Option<Capture>,
}

impl From<&Handle> for Subject {
    fn from(handle: &Handle) -> Self {
        Self {
            target: handle.target.clone(),
            capture: handle.capture,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct Carrier {
    leaves: FxHashMap<Contribution, Leaf>,
    callees: FxHashMap<Path, crate::provenance::callbacks::Targets>,
    queues: FxHashMap<Path, rustc_hash::FxHashSet<crate::provenance::channels::Handle>>,
    futures: FxHashMap<Path, rustc_hash::FxHashSet<Handle>>,
}

impl Carrier {
    pub fn seal(value: Value) -> Self {
        Self {
            leaves: value.leaves,
            callees: value.callees,
            queues: value.queues,
            futures: value.futures,
        }
    }
    pub fn value(&self) -> Value {
        Value {
            leaves: self.leaves.clone(),
            callees: self.callees.clone(),
            queues: self.queues.clone(),
            futures: self.futures.clone(),
            ..Value::default()
        }
    }
    pub(in crate::provenance) fn retire_frames(
        &mut self,
        boundary: crate::provenance::address::OpaqueSite,
    ) {
        for leaf in self.leaves.values_mut() {
            leaf.retire_frames(boundary);
        }
    }

    pub(in crate::provenance) fn join(&mut self, other: &Self) -> bool {
        let mut joined = self.value();
        let changed = joined.join(&other.value());
        if changed {
            *self = Self::seal(joined);
        }
        changed
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct View {
    pub callbacks: FxHashMap<TypeId, Callback>,
    pub fallback: bool,
    pub history: crate::provenance::observations::History,
    pub memory: super::memory::Effects,
}

impl View {
    pub fn pending(callback: Value, ty: TypeId) -> Self {
        Self {
            callbacks: FxHashMap::from_iter([(ty, Callback::seal(callback))]),
            ..Self::default()
        }
    }
    pub fn unknown() -> Self {
        Self {
            fallback: true,
            ..Self::default()
        }
    }
    pub(in crate::provenance) fn retains_activity(&self) -> bool {
        self.fallback
            || self.memory.may_write
            || self.memory.samples().next().is_some()
            || self
                .callbacks
                .values()
                .any(|callback| callback.shallow_value().retains_task_activity())
    }

    pub fn join(&mut self, other: &Self) -> bool {
        let mut changed = other.fallback && !self.fallback;
        self.fallback |= other.fallback;
        changed |= self.history.join(&other.history);
        changed |= self.memory.join(&other.memory);
        for (ty, carrier) in &other.callbacks {
            if let Some(current) = self.callbacks.get_mut(ty) {
                changed |= current.join(carrier);
            } else {
                self.callbacks.insert(*ty, carrier.clone());
                changed = true;
            }
        }
        changed
    }
}

pub(in crate::provenance) type Views = FxHashMap<Path, FxHashMap<Subject, View>>;

pub(in crate::provenance) fn join(
    current: &mut Views,
    before: &FxHashMap<Path, rustc_hash::FxHashSet<Handle>>,
    other: &Value,
) -> bool {
    let mut changed = false;
    for (path, incoming) in &other.task_views {
        for (subject, view) in incoming {
            let subjects = current.entry(path.clone()).or_default();
            if let Some(existing) = subjects.get_mut(subject) {
                changed |= existing.join(view);
            } else {
                let mut view = view.clone();
                view.fallback |= before
                    .get(path)
                    .into_iter()
                    .flatten()
                    .any(|h| h.pending && Subject::from(h) == *subject);
                subjects.insert(subject.clone(), view);
                changed = true;
            }
        }
    }
    for (path, subjects) in current {
        for (subject, view) in subjects {
            if other
                .futures
                .get(path)
                .into_iter()
                .flatten()
                .any(|h| h.pending && Subject::from(h) == *subject)
                && !other
                    .task_views
                    .get(path)
                    .is_some_and(|views| views.contains_key(subject))
            {
                changed |= !view.fallback;
                view.fallback = true;
            }
        }
    }
    changed
}

pub(in crate::provenance) fn contribution(
    value: &Value,
    path: &[crate::provenance::Slot],
    handle: &Handle,
) -> View {
    if handle.pending {
        value
            .task_views
            .get(path)
            .and_then(|views| views.get(&Subject::from(handle)))
            .cloned()
            .unwrap_or_else(View::unknown)
    } else {
        View::default()
    }
}

pub(in crate::provenance) fn map(
    value: &Value,
    select: impl Fn(&[crate::provenance::Slot]) -> Option<Path>,
    subject: impl Fn(&[crate::provenance::Slot], &Handle) -> Subject,
) -> Views {
    let mut result: Views = FxHashMap::default();
    for (path, handles) in &value.futures {
        if let Some(mapped) = select(path) {
            for handle in handles {
                result
                    .entry(mapped.clone())
                    .or_default()
                    .entry(subject(path, handle))
                    .or_default()
                    .join(&contribution(value, path, handle));
            }
        }
    }
    result
}

pub(in crate::provenance) fn pending_records_at(
    value: &Value,
    path: &[crate::provenance::Slot],
    handle: &Handle,
    state: &crate::provenance::State,
    storage: &crate::provenance::demand::Storage,
) -> (Vec<super::Record>, bool) {
    let (records, unknown) = super::activity::located_records(value, path, handle, state, storage);
    (
        records.into_iter().map(|located| located.record).collect(),
        unknown,
    )
}

pub(in crate::provenance) fn pending_records(
    value: &Value,
    path: &[crate::provenance::Slot],
    handle: &Handle,
    state: &crate::provenance::State,
) -> (Vec<super::Record>, bool) {
    let view = value
        .task_views
        .get(path)
        .and_then(|views| views.get(&Subject::from(handle)));
    let mut records = Vec::new();
    if let Some(view) = view {
        records.extend(view.callbacks.iter().map(|(ty, callback)| super::Record {
            callback: callback.value(),
            callback_ty: *ty,
        }));
    }
    let mut unknown = false;
    if view.is_none_or(|v| v.fallback) {
        if let Some(record) = state.tasks.get(&handle.target) {
            records.push(super::Record {
                callback: record.callback.clone().sourced(
                    &crate::provenance::demand::Storage::Task(
                        handle.target.clone(),
                        record.callback_ty,
                    ),
                ),
                callback_ty: record.callback_ty,
            });
        } else {
            unknown = true;
        }
    }
    (records, unknown)
}

impl Carrier {
    pub fn remap_queues(
        &mut self,
        bindings: &FxHashMap<
            crate::provenance::channels::Target,
            crate::provenance::channels::Target,
        >,
    ) {
        crate::provenance::channels::remap_queues(&mut self.queues, bindings);
    }
}

#[cfg(test)]
mod tests;

impl Value {
    pub(in crate::provenance) fn callback_storage_values(
        &self,
        storage: &crate::provenance::demand::Storage,
    ) -> Vec<(crate::provenance::demand::Storage, Self)> {
        use crate::provenance::{demand::Storage, tasks::activity::Location};
        let mut result = Vec::new();
        for (path, subjects) in &self.task_views {
            for (actor, view) in subjects {
                for (ty, callback) in &view.callbacks {
                    let location = Location::new(storage, path.clone(), actor.clone(), *ty);
                    result.extend(callback.source_values().map(|(key, value)| {
                        let selected = key
                            .map_or_else(|| location.clone(), |key| location.for_node(key.clone()));
                        (Storage::CallbackNode(Box::new(selected)), value)
                    }));
                }
            }
        }
        result
    }

    pub(in crate::provenance) fn source_callback_nodes(
        &mut self,
        storage: &crate::provenance::demand::Storage,
    ) {
        for (path, subjects) in &mut self.task_views {
            for (actor, view) in subjects {
                for (ty, callback) in &mut view.callbacks {
                    callback.source_at(&crate::provenance::tasks::activity::Location::new(
                        storage,
                        path.clone(),
                        actor.clone(),
                        *ty,
                    ));
                }
            }
        }
    }

    pub(in crate::provenance) fn visit_callback_sources(
        &mut self,
        mut visit: impl FnMut(&mut crate::provenance::demand::Sources),
    ) {
        for view in self
            .task_views
            .values_mut()
            .flat_map(|subjects| subjects.values_mut())
        {
            for callback in view.callbacks.values_mut() {
                callback.visit_sources(&mut visit);
            }
        }
    }

    pub(in crate::provenance) fn visit_task_progress(
        &mut self,
        mut visit: impl FnMut(
            &mut crate::provenance::observations::History,
            &mut super::memory::Effects,
        ),
    ) {
        for view in self
            .task_views
            .values_mut()
            .flat_map(|subjects| subjects.values_mut())
        {
            visit(&mut view.history, &mut view.memory);
            for callback in view.callbacks.values_mut() {
                callback.visit_progress(&mut visit);
            }
        }
    }

    pub(in crate::provenance) fn clear_task_imports(&mut self) {
        self.visit_task_progress(|history, memory| {
            history.clear_imports();
            memory.clear_imports();
        });
    }
}

impl Value {
    pub(in crate::provenance) fn retains_task_activity(&self) -> bool {
        !self.leaves.is_empty()
            || !self.queues.is_empty()
            || self.futures.values().flatten().any(|handle| handle.pending)
            || self
                .callees
                .iter()
                .any(|(path, targets)| !path.is_empty() || targets.open || targets.unresolved)
    }
}
