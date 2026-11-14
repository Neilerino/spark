// TAG [SPARK-114]

//! Task writes retain their own source rows while ordinary memory stays point-sensitive.

pub(in crate::provenance) mod completion;
mod final_writes;
pub(in crate::provenance) use completion::SettledCompletion;

use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};

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
    pub execution: crate::provenance::effects::Execution,
    pub location: Sample,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct Effects {
    pub may_write: bool,
    pub unknown_write: bool,
    pub interference: FxHashSet<Sample>,
    pub unknown_interference: bool,
    facts: FxHashMap<Arc<Lane>, Carrier>,
    imports: FxHashMap<Arc<Lane>, Sources>,
}

impl Effects {
    pub fn join(&mut self, other: &Self) -> bool {
        let mut changed = !self.may_write && other.may_write;
        self.may_write |= other.may_write;
        changed |= !self.unknown_write && other.unknown_write;
        self.unknown_write |= other.unknown_write;
        let before = self.interference.len();
        self.interference.extend(other.interference.iter().cloned());
        changed |= self.interference.len() != before;
        changed |= !self.unknown_interference && other.unknown_interference;
        self.unknown_interference |= other.unknown_interference;
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
        self.unknown_write |= matches!(
            lane.location.memory,
            Memory::Unavailable(_)
                | Memory::Frame {
                    scope: FrameScope::Foreign,
                    ..
                }
        );
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
            unknown_write: self.unknown_write,
            interference: self.interference.clone(),
            unknown_interference: self.unknown_interference,
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

#[derive(Default)]
pub(in crate::provenance) struct NormalMemory {
    pub alternatives: Vec<Vec<crate::provenance::effects::MemoryWrite>>,
    pub incomplete: bool,
    pub(in crate::provenance) support: FxHashSet<Sample>,
}

impl NormalMemory {
    pub fn join(&mut self, mut other: Self) {
        self.incomplete |= other.incomplete;
        self.support.extend(other.support);
        self.alternatives.append(&mut other.alternatives);
    }
}

pub(in crate::provenance) struct PreparedMemory {
    requests: Vec<completion::Request>,
    facets: Vec<PreparedFacet>,
}

struct PreparedFacet {
    emitter: Option<Emitter>,
    interference: Effects,
    prefix: Vec<(Arc<Lane>, Value)>,
    normal: NormalMemory,
}

#[derive(Clone)]
struct Emitter {
    role: super::Role,
    backing: EmitterBacking,
}

#[derive(Clone)]
enum EmitterBacking {
    Leaf(super::fresh::Birth),
    Execution(spark_mir::Local, spark_types::TypeId),
}

impl Emitter {
    fn at_role(flow: &Flow<'_>, role: &super::Role, state: &State) -> Option<Self> {
        let local = role.as_local()?;
        let peers = state.future_copies.role_witness(role).peers;
        if !matches!(
            state.future_owners.group_coverage(&peers),
            Some(super::lifecycle::Coverage::Closed | super::lifecycle::Coverage::Direct)
        ) || peers.iter().any(|peer| {
            !state.role_live(peer)
                || peer.frame_clobbered(&[], &state.invalidated_copies)
                || state.future_owners.exposed_role(peer)
        }) {
            return None;
        }
        let current = state.value(local);
        let handles = current.futures.get(&Vec::new())?;
        if handles.len() != 1
            || !handles
                .iter()
                .all(|handle| handle.pending && handle.capture.is_none())
        {
            return None;
        }
        let backing = if let Some(birth) = flow.current_leaf_birth(local, state) {
            EmitterBacking::Leaf(birth)
        } else {
            let (Storage::ExecutionCallback(anchor, ty), _) = state.current_execution(local)?
            else {
                return None;
            };
            EmitterBacking::Execution(anchor, ty)
        };
        Some(Self {
            role: role.clone(),
            backing,
        })
    }

    fn still_current(&self, flow: &Flow<'_>, state: &State) -> bool {
        let Some(current) = Self::at_role(flow, &self.role, state) else {
            return false;
        };
        match (&self.backing, current.backing) {
            (EmitterBacking::Leaf(before), EmitterBacking::Leaf(after)) => *before == after,
            (
                EmitterBacking::Execution(before, before_ty),
                EmitterBacking::Execution(after, after_ty),
            ) => *before == after && *before_ty == after_ty,
            _ => false,
        }
    }
}

impl PreparedMemory {
    pub(in crate::provenance) fn normal_values_mut(&mut self) -> impl Iterator<Item = &mut Value> {
        self.facets
            .iter_mut()
            .filter(|facet| !facet.normal.incomplete)
            .flat_map(|facet| facet.normal.alternatives.iter_mut().flatten())
            .map(|write| &mut write.value)
    }

    fn visit_values(&mut self, mut visit: impl FnMut(&mut Value)) {
        for facet in &mut self.facets {
            for (_, value) in &mut facet.prefix {
                visit(value);
            }
        }
        for value in self.normal_values_mut() {
            visit(value);
        }
    }
}

pub(in crate::provenance) type Delta = Vec<(Arc<Lane>, Value)>;

impl State {
    pub(in crate::provenance) fn record_task_interference(&mut self, sample: Option<&Sample>) {
        self.record_competing_task_write(sample, None);
    }

    fn record_competing_task_write(
        &mut self,
        sample: Option<&Sample>,
        own: Option<(&FxHashSet<super::Role>, &Subject)>,
    ) {
        let mut record = |_: &mut crate::provenance::observations::History,
                          memory: &mut Effects| {
            if let Some(sample) = sample {
                memory.interference.insert(sample.clone());
            } else {
                memory.unknown_interference = true;
            }
        };
        for (local, value) in &mut self.values {
            let own = own.and_then(|(peers, subject)| {
                peers
                    .contains(&super::Role::root(*local))
                    .then_some(subject)
            });
            value.visit_task_progress_except_root(own, &mut record);
        }
        let carriers = self
            .cells
            .values_mut()
            .chain(self.external.values_mut())
            .chain(self.executions.values_mut())
            .chain(self.queues.values_mut())
            .chain(
                self.queue_current
                    .values_mut()
                    .map(|message| &mut message.value),
            )
            .chain(self.residuals.values_mut())
            .chain(self.residual_callbacks.values_mut())
            .chain(self.pending_memory.values_mut())
            .chain(self.settled_memory.values_mut());
        for value in carriers {
            value.visit_task_progress(&mut record);
        }
    }
}

impl Flow<'_> {
    pub(in crate::provenance) fn seed_spawn_interference(returned: &mut Value, state: &State) {
        let activity = state.rooted_activity();
        let mut interference = FxHashSet::default();
        let mut unknown = false;
        for view in activity.views() {
            unknown |= view.fallback || view.memory.unknown_write;
            interference.extend(view.memory.facts.keys().map(|lane| lane.location.clone()));
        }
        for view in returned
            .task_views
            .get_mut(&Vec::new())
            .into_iter()
            .flat_map(|views| views.values_mut())
        {
            view.memory
                .interference
                .extend(interference.iter().cloned());
            view.memory.unknown_interference |= unknown;
        }
    }

    pub(in crate::provenance) fn record_call_prefix_interference(
        &mut self,
        events: &[crate::provenance::effects::MemoryEvent],
        unknown: bool,
        point: Point,
        state: &mut State,
    ) {
        if unknown {
            self.trace_unknown_completion_write(point);
            state.record_task_interference(None);
        }
        for event in events {
            for sample in self.memory_locations(&event.write.target, event.write.ty, event.site) {
                self.trace_memory_location(&sample, point, false);
                state.record_task_interference(Some(&sample));
            }
        }
    }

    pub(in crate::provenance) fn record_task_interference(
        &self,
        destination: &Coordinate,
        ty: spark_types::TypeId,
        state: &mut State,
    ) {
        let memory = match &destination.storage {
            Storage::Cell(local) | Storage::Logical(local) => Memory::Frame {
                scope: FrameScope::Current,
                function: self.function.def_id,
                local: *local,
                ty: self.storage_type(&destination.storage),
            },
            Storage::External(port) => Memory::External(port.clone()),
            _ => return,
        };
        state.record_task_interference(Some(&Sample {
            memory,
            path: destination.path.clone(),
            ty,
        }));
    }

    pub(in crate::provenance) fn clear_settled_memory(
        &mut self,
        destination: &Coordinate,
        point: Point,
        state: &mut State,
    ) {
        self.clear_settled_samples(destination, point, state, |_| true);
    }

    pub(in crate::provenance) fn clear_entry_settled_memory(
        &mut self,
        destination: &Coordinate,
        point: Point,
        state: &mut State,
    ) {
        let mut retained = Vec::new();
        let eligible: FxHashSet<_> = state
            .settled_memory
            .iter()
            .filter_map(|(location, current)| {
                let sample = self.physical_sample(location)?;
                if self.settlement_unchanged_since_entry(&sample, point) {
                    return Some(location.clone());
                }
                if !completion::overlaps(destination, &sample)
                    || !crate::provenance::writes::owned_path(&destination.path)
                    || !crate::provenance::writes::owned_path(&sample.path)
                    || !current.futures.is_empty()
                    || !current.task_views.is_empty()
                {
                    return None;
                }
                let new = self.new_settled_contribution(location, point)?;
                retained.push((location.clone(), new.clone()));
                Some(location.clone())
            })
            .collect();
        self.clear_settled_samples(destination, point, state, |sample| {
            eligible.contains(sample)
        });
        for (location, value) in retained {
            self.store_settled_sample(location, value, point, state);
        }
    }

    fn clear_settled_samples(
        &mut self,
        destination: &Coordinate,
        point: Point,
        state: &mut State,
        eligible: impl Fn(&Sample) -> bool,
    ) {
        if !crate::provenance::writes::owned_path(&destination.path) {
            return;
        }
        let affected: Vec<_> = state
            .settled_memory
            .keys()
            .filter_map(|location| {
                if !eligible(location) || !crate::provenance::writes::owned_path(&location.path) {
                    return None;
                }
                let storage = match &location.memory {
                    Memory::Frame {
                        scope: FrameScope::Current,
                        function,
                        local,
                        ..
                    } if *function == self.function.def_id => Storage::Cell(*local),
                    Memory::External(port) => Storage::External(port.clone()),
                    Memory::Frame { .. } | Memory::Queue(_, _) | Memory::Unavailable(_) => {
                        return None
                    }
                };
                if storage != destination.storage {
                    return None;
                }
                if crate::provenance::storage::prefix(&destination.path, &location.path) {
                    Some((location.clone(), Vec::new()))
                } else if crate::provenance::storage::prefix(&location.path, &destination.path) {
                    Some((
                        location.clone(),
                        destination.path[location.path.len()..].to_vec(),
                    ))
                } else {
                    None
                }
            })
            .collect();
        for (location, relative) in affected {
            self.clear_new_settled(&location, &relative, point);
            self.record_path_store(
                point,
                Coordinate {
                    storage: Storage::SettledMemory(location.clone()),
                    path: relative.clone(),
                },
                &Value::default(),
                true,
            );
            if relative.is_empty() {
                state.settled_memory.remove(&location);
            } else if let Some(value) = state.settled_memory.get_mut(&location) {
                value.clear_path(&relative);
            }
        }
    }

    fn trace_memory_location(&mut self, location: &Sample, point: Point, settled: bool) {
        let storage = match &location.memory {
            Memory::Frame {
                scope: FrameScope::Current,
                function,
                local,
                ..
            } if *function == self.function.def_id => Storage::Cell(*local),
            Memory::External(port) => Storage::External(port.clone()),
            Memory::Queue(_, _) => return,
            Memory::Frame { .. } | Memory::Unavailable(_) => {
                self.trace_unknown_completion_write(point);
                return;
            }
        };
        let destination = Coordinate {
            storage,
            path: location.path.clone(),
        };
        if settled {
            self.trace_completion_import(destination, point);
        } else {
            self.trace_completion_write(destination, point);
        }
    }

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
                self.trace_memory_location(&lane.location, point, false);
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
                self.clobber_completion_local(*local, point);
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
                self.forget_completion_sources(point);
                self.unknown_copy_clobber = true;
                self.unknown_memory_write = true;
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
                                execution: event.execution,
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
            self.store_independent_settled_sample(location, value.clone(), point, state);
        }
    }

    pub(super) fn store_independent_settled_sample(
        &mut self,
        location: Sample,
        value: Value,
        point: Point,
        state: &mut State,
    ) {
        self.trace_memory_location(&location, point, true);
        self.store_settled_sample(location, value, point, state);
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
        (delta, may_write, unknown_write): (Delta, bool, bool),
        point: Point,
        state: &mut State,
    ) -> bool {
        if unknown_write {
            self.trace_unknown_completion_write(point);
        }
        let own = match storage {
            Storage::Logical(local) if path.is_empty() => {
                let role = super::Role::root(*local);
                Emitter::at_role(self, &role, state)
                    .map(|_| state.future_copies.role_witness(&role).peers)
            }
            _ => None,
        };
        if unknown_write {
            state.record_task_interference(None);
        }
        for (lane, _) in &delta {
            let emitter = if lane.execution == crate::provenance::effects::Execution::Current {
                own.as_ref()
            } else {
                None
            };
            if let Some(destination) = self.physical_sample(&lane.location) {
                self.trace_task_write(destination, emitter, point);
            } else if !matches!(lane.location.memory, Memory::Queue(..)) {
                self.trace_unknown_completion_write(point);
            }
            state.record_competing_task_write(
                Some(&lane.location),
                emitter.map(|peers| (peers, &actor)),
            );
        }
        let Some(mut current) = state
            .stored_value(storage)
            .map(std::borrow::Cow::into_owned)
        else {
            return false;
        };
        let mut effects = Effects {
            may_write,
            unknown_write,
            ..Effects::default()
        };
        for (lane, value) in delta {
            effects.unknown_write |= match &lane.location.memory {
                Memory::Unavailable(_) => true,
                Memory::Frame {
                    scope, function, ..
                } => *scope != FrameScope::Current || *function != self.function.def_id,
                Memory::External(_) | Memory::Queue(_, _) => false,
            };
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

    fn normal_task_memory(
        &mut self,
        current: &Value,
        storage: &Storage,
        handle: &super::Handle,
        point: Point,
        state: &mut State,
    ) -> NormalMemory {
        let Some(inner) = crate::provenance::value::child_type(
            handle.target.ty(),
            &crate::provenance::Slot::Element,
            self.table,
        ) else {
            return NormalMemory {
                incomplete: true,
                ..NormalMemory::default()
            };
        };
        let (records, unknown) =
            super::views::pending_records_at(current, &[], handle, state, storage);
        let mut normal = NormalMemory {
            incomplete: unknown || records.is_empty(),
            ..NormalMemory::default()
        };
        let subject = Subject::from(handle);
        let mut history = super::views::contribution(current, &[], handle).history;
        let samples = self.resample_history(
            &history,
            crate::provenance::observations::ReadView::current(state),
        );
        self.remember_samples(samples, &mut history, point, state);
        let trace = std::cell::RefCell::default();
        for record in records {
            let read =
                crate::provenance::observations::ReadView::task(state, &subject, &history, &trace);
            normal.join(
                self.normal_task_facts_in(&record, &handle.target, point, read, inner)
                    .normal_memory,
            );
        }
        let reads = trace.into_inner();
        normal
            .support
            .extend(reads.keys().map(|lane| lane.sample.clone()));
        self.remember_samples(reads, &mut history, point, state);
        normal
    }

    #[cfg(test)]
    pub(in crate::provenance) fn prepare_task_memory(
        &mut self,
        witnesses: Vec<super::Witness>,
        clobbered: &[spark_mir::Local],
        point: Point,
        state: &mut State,
    ) -> PreparedMemory {
        let requests = self.reserve_completions(witnesses, point);
        self.prepare_reserved_memory(requests, clobbered, point, state)
    }

    pub(in crate::provenance) fn prepare_reserved_memory(
        &mut self,
        requests: Vec<completion::Request>,
        clobbered: &[spark_mir::Local],
        point: Point,
        state: &mut State,
    ) -> PreparedMemory {
        self.restrict_completion_requests(&requests, clobbered, point);
        let roles: rustc_hash::FxHashSet<_> = requests
            .iter()
            .filter_map(|request| request.witness(self, point))
            .flat_map(|witness| witness.peers)
            .filter(|role| !role.frame_clobbered(clobbered, &state.invalidated_copies))
            .collect();
        let mut prepared = PreparedMemory {
            requests,
            facets: Vec::new(),
        };
        for role in &roles {
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
                let normal =
                    self.normal_task_memory(&current, &role.storage(), handle, point, state);
                let view = super::views::contribution(&current, &[], handle);
                let prefix = view
                    .memory
                    .samples()
                    .map(|(lane, value)| {
                        (lane.clone(), Self::sample_pending_write(lane, value, state))
                    })
                    .collect();
                prepared.facets.push(PreparedFacet {
                    emitter: Emitter::at_role(self, role, state),
                    interference: view.memory,
                    prefix,
                    normal,
                });
            }
        }
        prepared.visit_values(|value| *value = self.freeze_sources(point, std::mem::take(value)));
        prepared
    }
}

#[cfg(test)]
mod tests;
