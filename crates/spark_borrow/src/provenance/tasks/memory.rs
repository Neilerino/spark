// TAG [SPARK-114]

//! Task writes retain their own source rows while ordinary memory stays point-sensitive.

use std::sync::Arc;

use rustc_hash::FxHashMap;

use super::views::{Carrier, Subject};
use crate::provenance::{
    address::OpaqueSite,
    demand::{Coordinate, Sources, Storage, Transfer},
    observations::{FrameScope, Memory, Sample},
    Flow, Path, Point, State, Value,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) struct Lane {
    pub event: OpaqueSite,
    pub actor: Subject,
    pub location: Sample,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct Effects {
    pub may_write: bool,
    facts: FxHashMap<Arc<Lane>, Carrier>,
    imports: FxHashMap<Arc<Lane>, Sources>,
}

impl Effects {
    pub fn join(&mut self, other: &Self) -> bool {
        let mut changed = !self.may_write && other.may_write;
        self.may_write |= other.may_write;
        for (lane, carrier) in &other.facts {
            let absent = !self.facts.contains_key(lane);
            changed |= self.facts.entry(lane.clone()).or_default().join(carrier) || absent;
        }
        for (lane, sources) in &other.imports {
            let current = self.imports.entry(lane.clone()).or_default();
            for (contribution, frontier) in sources {
                let current = current.entry(contribution.clone()).or_default();
                let before = current.len();
                current.extend(frontier.iter().cloned());
                changed |= before != current.len();
            }
        }
        changed
    }

    pub fn samples(&self) -> impl Iterator<Item = (&Arc<Lane>, Value)> {
        self.facts
            .iter()
            .map(|(lane, carrier)| (lane, carrier.value()))
    }

    pub fn import(&mut self, lane: Arc<Lane>, mut value: Value) {
        let sources = self.imports.entry(lane.clone()).or_default();
        for (contribution, frontier) in std::mem::take(&mut value.sources) {
            sources.entry(contribution).or_default().extend(frontier);
        }
        self.facts
            .entry(lane)
            .or_default()
            .join(&Carrier::seal(value));
    }

    pub fn freeze_imports(&mut self, transfer: &Transfer) {
        for sources in self.imports.values_mut() {
            transfer.freeze_annotations(sources);
        }
    }

    pub fn clear_imports(&mut self) {
        self.imports.clear();
    }

    pub fn exported(&self, function: spark_resolve::DefId) -> Self {
        Self {
            may_write: self.may_write,
            facts: self
                .samples()
                .map(|(lane, value)| {
                    (
                        lane.clone(),
                        Carrier::seal(crate::provenance::effects::export_value(&value, function)),
                    )
                })
                .collect(),
            imports: FxHashMap::default(),
        }
    }

    pub fn retire_frames(&mut self, boundary: OpaqueSite) {
        for carrier in self.facts.values_mut() {
            carrier.retire_frames(boundary);
        }
    }
}

pub(in crate::provenance) type Delta = Vec<(Arc<Lane>, Value)>;

impl Flow<'_> {
    pub(in crate::provenance) fn freeze_memory_delta(&self, delta: &mut Delta, point: Point) {
        for (_, value) in delta {
            *value = self.freeze_sources(point, std::mem::take(value));
        }
    }

    pub(in crate::provenance) fn import_memory_sources(
        &mut self,
        value: &mut Value,
        point: Point,
        state: &mut State,
    ) {
        value.visit_task_progress(|_, effects| {
            for (lane, sources) in std::mem::take(&mut effects.imports) {
                let mut value = effects.facts[&lane].value();
                value.sources = sources;
                self.remember_pending_write(lane, value, point, state);
            }
        });
    }

    fn remember_pending_write(
        &mut self,
        lane: Arc<Lane>,
        mut value: Value,
        point: Point,
        state: &mut State,
    ) {
        self.import_task_sources(&mut value, point, state);
        match &lane.location.memory {
            Memory::Frame {
                scope: FrameScope::Current,
                function,
                local,
                ..
            } if *function == self.function.def_id => {
                self.clobber_input_owners(*local, state);
                state.invalidated_copies.insert(*local);
            }
            Memory::External(port) => {
                self.written_inputs
                    .insert(crate::provenance::aliases::View {
                        root: port.clone(),
                        path: lane.location.path.clone(),
                    });
            }
            Memory::Frame { .. } | Memory::Unavailable(_) => {
                self.unknown_copy_clobber = true;
                let live: Vec<_> = state.live.iter().copied().collect();
                for local in live {
                    self.clobber_input_owners(local, state);
                    state.invalidated_copies.insert(local);
                }
            }
            Memory::Queue(_, _) => {}
        }
        self.record_path_store(
            point,
            Coordinate {
                storage: Storage::PendingWrite(lane.clone()),
                path: Vec::new(),
            },
            &value,
            false,
        );
        value.clear_sources();
        state.pending_memory.entry(lane).or_default().join(&value);
    }

    fn memory_locations(
        &self,
        metadata: &crate::provenance::value::Leaf,
        ty: spark_types::TypeId,
        site: OpaqueSite,
    ) -> Vec<Sample> {
        let targets = self.write_targets(metadata);
        let mut locations = Vec::new();
        for target in targets.all {
            let memory = match target.storage {
                Storage::Cell(local) | Storage::Logical(local) => Memory::Frame {
                    scope: FrameScope::Current,
                    function: self.function.def_id,
                    local,
                    ty: self.function.locals[local.index()].ty,
                },
                Storage::External(port) => Memory::External(port),
                Storage::Queue(_, _)
                | Storage::QueueCurrent(_)
                | Storage::Task(_, _)
                | Storage::ExecutionCallback(_, _)
                | Storage::Observation(_)
                | Storage::PendingWrite(_)
                | Storage::SettledMemory(_)
                | Storage::Residual(_)
                | Storage::CallbackNode(_)
                | Storage::ResidualCallback(_, _) => continue,
            };
            locations.push(Sample {
                memory,
                path: target.path,
                ty,
            });
        }
        if targets.unknown {
            locations.push(Sample {
                memory: Memory::Unavailable(site),
                path: Vec::new(),
                ty,
            });
        }
        locations
    }

    pub(in crate::provenance) fn task_memory_delta(
        &self,
        events: &[crate::provenance::effects::MemoryEvent],
        actor: &Subject,
    ) -> Delta {
        events
            .iter()
            .flat_map(|event| {
                self.memory_locations(&event.write.target, event.write.ty, event.site)
                    .into_iter()
                    .map(|location| {
                        (
                            Arc::new(Lane {
                                event: event.site,
                                actor: actor.clone(),
                                location,
                            }),
                            event.write.value.clone(),
                        )
                    })
            })
            .collect()
    }

    pub(in crate::provenance) fn import_settled_memory(
        &mut self,
        write: &crate::provenance::effects::MemoryWrite,
        point: Point,
        state: &mut State,
    ) {
        let mut value = self.freeze_sources(point, write.value.clone());
        self.import_task_sources(&mut value, point, state);
        let locations = self.memory_locations(
            &write.target,
            write.ty,
            OpaqueSite {
                function: self.function.def_id,
                point,
            },
        );
        for location in locations {
            self.record_path_store(
                point,
                Coordinate {
                    storage: Storage::SettledMemory(location.clone()),
                    path: Vec::new(),
                },
                &value,
                false,
            );
            let mut stored = value.clone();
            stored.clear_sources();
            state
                .settled_memory
                .entry(location)
                .or_default()
                .join(&stored);
        }
    }

    pub(in crate::provenance) fn sample_pending_write(
        lane: &Arc<Lane>,
        mut sample: Value,
        state: &State,
    ) -> Value {
        if let Some(backing) = state.pending_memory.get(lane) {
            for (path, handles) in &sample.futures {
                for handle in handles {
                    sample
                        .task_views
                        .entry(path.clone())
                        .or_default()
                        .entry(Subject::from(handle))
                        .or_default()
                        .join(&super::views::contribution(backing, path, handle));
                }
            }
        }
        sample.sourced(&Storage::PendingWrite(lane.clone()))
    }

    pub(in crate::provenance) fn external_async_values(state: &State) -> Vec<Value> {
        let mut values = Vec::new();
        for (location, value) in &state.settled_memory {
            if matches!(location.memory, Memory::External(_)) {
                values.push(
                    value
                        .clone()
                        .sourced(&Storage::SettledMemory(location.clone())),
                );
            }
        }
        let activity = state.rooted_activity();
        for view in activity.views() {
            for (lane, value) in view.memory.samples() {
                if matches!(lane.location.memory, Memory::External(_))
                    && state.pending_memory.contains_key(lane)
                {
                    values.push(Self::sample_pending_write(lane, value, state));
                }
            }
        }
        values
    }

    pub(in crate::provenance) fn async_modified(
        state: &State,
    ) -> rustc_hash::FxHashSet<crate::provenance::aliases::View> {
        let mut modified = rustc_hash::FxHashSet::default();
        let activity = state.rooted_activity();
        let pending = activity
            .views()
            .flat_map(|view| view.memory.samples())
            .map(|(lane, _)| &lane.location);
        for location in state.settled_memory.keys().chain(pending) {
            if let Memory::External(port) = &location.memory {
                modified.insert(crate::provenance::aliases::View {
                    root: port.clone(),
                    path: location.path.clone(),
                });
            }
        }
        modified
    }

    pub(in crate::provenance) fn cache_pending_memory(
        &mut self,
        storage: &Storage,
        path: Path,
        actor: Subject,
        (delta, may_write): (Delta, bool),
        point: Point,
        state: &mut State,
    ) -> bool {
        let Some(mut current) = state
            .stored_value(storage)
            .map(std::borrow::Cow::into_owned)
        else {
            return false;
        };
        let mut effects = Effects {
            may_write,
            ..Effects::default()
        };
        for (lane, value) in delta {
            self.remember_pending_write(lane.clone(), value.clone(), point, state);
            effects
                .facts
                .entry(lane)
                .or_default()
                .join(&Carrier::seal(value));
        }
        let view = current
            .task_views
            .entry(path)
            .or_default()
            .entry(actor)
            .or_insert_with(super::views::View::unknown);
        if !view.memory.join(&effects) {
            return false;
        }
        Self::replace_task_carrier(storage, current, state)
    }

    pub(in crate::provenance) fn async_memory(
        &self,
        storage: &Storage,
        ty: spark_types::TypeId,
        state: &State,
    ) -> Value {
        let mut result = Value::default();
        if state.pending_memory.is_empty() && state.settled_memory.is_empty() {
            return result;
        }
        let mut add = |location: &Sample, value: Value| {
            let matches = match (&location.memory, storage) {
                (
                    Memory::Frame {
                        scope: FrameScope::Current,
                        function,
                        local,
                        ..
                    },
                    Storage::Cell(actual) | Storage::Logical(actual),
                ) => *function == self.function.def_id && local == actual,
                (Memory::External(port), Storage::External(actual)) => port == actual,
                (Memory::Unavailable(_), _) => {
                    result.join(&crate::provenance::value::tainted_from(
                        ty, &value, self.table,
                    ));
                    return;
                }
                _ => false,
            };
            if matches {
                result.join(&value.prefixed_path(&location.path, ty, self.table));
            }
        };
        let activity = state.rooted_activity();
        for view in activity.views() {
            for (lane, value) in view.memory.samples() {
                if state.pending_memory.contains_key(lane) {
                    add(
                        &lane.location,
                        Self::sample_pending_write(lane, value, state),
                    );
                }
            }
        }
        for (location, value) in &state.settled_memory {
            add(
                location,
                value
                    .clone()
                    .sourced(&Storage::SettledMemory(location.clone())),
            );
        }
        result
    }

    pub(in crate::provenance) fn settle_task_memory(
        &mut self,
        witness: &super::Witness,
        clobbered: &[spark_mir::Local],
        point: Point,
        state: &mut State,
    ) {
        let mut values = Vec::new();
        for role in witness.peers.iter().filter(|role| {
            !clobbered.contains(&role.local()) && !state.invalidated_copies.contains(&role.local())
        }) {
            let Some(current) = self.current_role_value(role, state) else {
                continue;
            };
            for handle in current
                .futures
                .get(&Vec::new())
                .into_iter()
                .flatten()
                .filter(|handle| handle.pending)
            {
                let view = super::views::contribution(&current, &[], handle);
                for (lane, value) in view.memory.samples() {
                    values.push((
                        lane.location.clone(),
                        self.freeze_sources(point, Self::sample_pending_write(lane, value, state)),
                    ));
                }
            }
        }
        for (location, mut value) in values {
            self.record_path_store(
                point,
                Coordinate {
                    storage: Storage::SettledMemory(location.clone()),
                    path: Vec::new(),
                },
                &value,
                false,
            );
            value.clear_sources();
            state
                .settled_memory
                .entry(location)
                .or_default()
                .join(&value);
        }
    }
}

#[cfg(test)]
mod tests;
