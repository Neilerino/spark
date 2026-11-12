// TAG [SPARK-114]

//! Retained task reads have their own source storage and never define current cells.

use std::{cell::RefCell, sync::Arc};

use rustc_hash::FxHashMap;
use spark_mir::Local;
use spark_resolve::DefId;
use spark_types::TypeId;

use super::{
    address::Selector,
    aliases::Port,
    channels,
    demand::{Coordinate, Storage},
    read::ReadSite,
    tasks::views::{Carrier, Subject},
    value, Flow, Path, Point, Slot, State, Value,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum FrameScope {
    Current,
    Foreign,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Memory {
    Frame {
        scope: FrameScope,
        function: DefId,
        local: Local,
        ty: TypeId,
    },
    External(Port),
    Queue(channels::Target, TypeId),
    Unavailable(super::address::OpaqueSite),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Sample {
    pub memory: Memory,
    pub path: Path,
    pub ty: TypeId,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(super) enum ReadRole {
    #[default]
    Payload,
    Queue,
    Selection(usize),
    Projection(usize),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Lane {
    site: Option<ReadSite>,
    receive: Option<Arc<super::effects::Receive>>,
    actor: Subject,
    function: DefId,
    selector: Selector,
    role: ReadRole,
    pub sample: Sample,
}

impl Lane {
    pub fn rebound(&self, actor: Subject, sample: Sample) -> Arc<Self> {
        Arc::new(Self {
            actor,
            sample,
            ..self.clone()
        })
    }
}

pub(super) type Samples = FxHashMap<Arc<Lane>, Value>;
pub(super) type Envelope = FxHashMap<Storage, Value>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct History {
    facts: FxHashMap<Arc<Lane>, Carrier>,
    imports: FxHashMap<Arc<Lane>, super::demand::Sources>,
}

impl History {
    pub fn join(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (lane, facts) in &other.facts {
            changed |= self.insert(lane.clone(), facts.clone());
        }
        for (lane, sources) in &other.imports {
            let missing = !self.imports.contains_key(lane);
            let current = self.imports.entry(lane.clone()).or_default();
            changed |= missing;
            for (contribution, incoming) in sources {
                let current = current.entry(contribution.clone()).or_default();
                let before = current.len();
                current.extend(incoming.iter().cloned());
                changed |= before != current.len();
            }
        }
        changed
    }

    pub fn retire_frames(&mut self, boundary: super::address::OpaqueSite) {
        for carrier in self.facts.values_mut() {
            carrier.retire_frames(boundary);
        }
    }

    pub fn exported(&self, function: DefId) -> Self {
        Self {
            facts: self
                .facts
                .iter()
                .map(|(lane, facts)| {
                    (
                        lane.clone(),
                        Carrier::seal(super::effects::export_value(&facts.value(), function)),
                    )
                })
                .collect(),
            imports: FxHashMap::default(),
        }
    }

    pub fn samples(&self) -> impl Iterator<Item = (&Arc<Lane>, Value)> {
        self.facts.iter().map(|(lane, facts)| (lane, facts.value()))
    }

    pub fn import_sample(&mut self, lane: Arc<Lane>, mut value: Value) {
        let sources = self.imports.entry(lane.clone()).or_default();
        for (contribution, incoming) in std::mem::take(&mut value.sources) {
            sources.entry(contribution).or_default().extend(incoming);
        }
        self.insert(lane, Carrier::seal(value));
    }

    pub fn clear_imports(&mut self) {
        self.imports.clear();
    }

    pub fn freeze_imports(&mut self, transfer: &super::demand::Transfer) {
        for sources in self.imports.values_mut() {
            transfer.freeze_annotations(sources);
        }
    }

    fn take_imports(&mut self) -> Samples {
        std::mem::take(&mut self.imports)
            .into_iter()
            .map(|(lane, sources)| {
                let mut value = self.facts[&lane].value();
                value.sources = sources;
                (lane, value)
            })
            .collect()
    }

    pub fn projected_samples(&self, state: &State, transfer: &super::demand::Transfer) -> Samples {
        self.facts
            .iter()
            .filter(|(lane, _)| state.observations.contains_key(*lane))
            .map(|(lane, carrier)| {
                let value = carrier.value().sourced(&Storage::Observation(lane.clone()));
                (lane.clone(), transfer.freeze(value))
            })
            .collect()
    }

    fn insert(&mut self, lane: Arc<Lane>, facts: Carrier) -> bool {
        use std::collections::hash_map::Entry;
        match self.facts.entry(lane) {
            Entry::Occupied(mut entry) => entry.get_mut().join(&facts),
            Entry::Vacant(entry) => {
                entry.insert(facts);
                true
            }
        }
    }
}

#[derive(Clone, Copy)]
struct TaskRead<'a> {
    actor: &'a Subject,
    history: &'a History,
    trace: &'a RefCell<Samples>,
}

#[derive(Clone, Copy)]
pub(super) struct ReadView<'a> {
    pub state: &'a State,
    task: Option<TaskRead<'a>>,
    envelope: Option<&'a Envelope>,
    query: Option<(DefId, &'a Selector)>,
    site: Option<ReadSite>,
    receive: Option<&'a super::effects::Receive>,
    receive_queries: Option<&'a RefCell<rustc_hash::FxHashSet<Arc<super::effects::Receive>>>>,
    entry_prefix: Option<&'a super::effects::ReachedPrefix>,
    access_site: Option<ReadSite>,
    role: ReadRole,
}

impl<'a> ReadView<'a> {
    pub fn current(state: &'a State) -> Self {
        Self {
            state,
            task: None,
            envelope: None,
            query: None,
            site: None,
            receive: None,
            receive_queries: None,
            entry_prefix: None,
            access_site: None,
            role: ReadRole::default(),
        }
    }

    pub fn task(
        state: &'a State,
        actor: &'a Subject,
        history: &'a History,
        trace: &'a RefCell<Samples>,
    ) -> Self {
        Self {
            task: Some(TaskRead {
                actor,
                history,
                trace,
            }),
            ..Self::current(state)
        }
    }

    pub const fn select<'b>(self, function: DefId, selector: &'b Selector) -> ReadView<'b>
    where
        'a: 'b,
    {
        ReadView {
            query: Some((function, selector)),
            role: ReadRole::Payload,
            ..self
        }
    }

    pub const fn role(self, role: ReadRole) -> Self {
        Self { role, ..self }
    }

    pub const fn with_read_site(self, site: Option<ReadSite>) -> Self {
        match site {
            Some(_) => Self { site, ..self },
            None => self,
        }
    }

    pub const fn with_receive<'b>(
        self,
        receive: Option<&'b super::effects::Receive>,
    ) -> ReadView<'b>
    where
        'a: 'b,
    {
        match receive {
            Some(receive) => ReadView {
                receive: Some(receive),
                site: Some(receive.site),
                ..self
            },
            None => self,
        }
    }

    pub const fn with_bound_receive<'b>(
        self,
        receive: Option<&'b super::effects::Receive>,
    ) -> ReadView<'b>
    where
        'a: 'b,
    {
        match receive {
            Some(receive) => ReadView {
                receive: Some(receive),
                site: Some(receive.site),
                entry_prefix: None,
                ..self
            },
            None => self,
        }
    }

    pub fn receive(self) -> Option<Arc<super::effects::Receive>> {
        self.receive.map(|receive| {
            Arc::new(
                self.entry_prefix
                    .map_or_else(|| receive.clone(), |prefix| receive.including(prefix)),
            )
        })
    }

    pub const fn with_entry_prefix(self) -> Self {
        Self {
            entry_prefix: Some(&self.state.reached_prefix),
            ..self
        }
    }

    pub const fn with_receive_queries<'b>(
        self,
        queries: &'b RefCell<rustc_hash::FxHashSet<Arc<super::effects::Receive>>>,
    ) -> ReadView<'b>
    where
        'a: 'b,
    {
        ReadView {
            receive_queries: Some(queries),
            ..self
        }
    }

    pub fn record_receive_query(self, receive: &Arc<super::effects::Receive>) {
        if let Some(queries) = self.receive_queries {
            queries.borrow_mut().insert(receive.clone());
        }
    }

    pub const fn with_access_site(self, access_site: Option<ReadSite>) -> Self {
        Self {
            access_site,
            ..self
        }
    }

    pub const fn access_site(self) -> Option<ReadSite> {
        self.access_site
    }

    pub const fn with_envelope(self, envelope: &'a Envelope) -> Self {
        Self {
            envelope: Some(envelope),
            ..self
        }
    }

    pub fn observe(self, sample: Sample, mut current: Value) -> Value {
        let (Some(task), Some((function, selector))) = (self.task, self.query) else {
            return current;
        };
        let lane = Arc::new(Lane {
            site: self.site,
            receive: self.receive(),
            actor: task.actor.clone(),
            function,
            selector: selector.clone(),
            role: self.role,
            sample,
        });
        task.trace
            .borrow_mut()
            .entry(lane.clone())
            .or_default()
            .join(&current);
        for (old, value) in &task.history.facts {
            if old.site == lane.site
                && old.function == lane.function
                && old.selector == lane.selector
                && old.role == lane.role
                && old.sample == lane.sample
                && self.state.observations.contains_key(old)
            {
                current.join(&value.value().sourced(&Storage::Observation(old.clone())));
            }
        }
        current
    }
}

impl Flow<'_> {
    pub(super) fn import_observation_sources(
        &mut self,
        value: &mut Value,
        point: Point,
        state: &mut State,
    ) {
        value.visit_task_progress(|history, _| {
            let imports = history.take_imports();
            self.remember_samples(imports, history, point, state);
        });
    }

    pub(super) fn import_prefix_observations(
        &mut self,
        memory: &mut [super::effects::MemoryEvent],
        queues: &mut [super::channels::QueueEvent],
        point: Point,
        state: &mut State,
    ) {
        for value in memory
            .iter_mut()
            .map(|event| &mut event.write.value)
            .chain(queues.iter_mut().map(|event| &mut event.write.value))
        {
            *value = self.freeze_sources(point, std::mem::take(value));
        }
        for value in memory
            .iter_mut()
            .map(|event| &mut event.write.value)
            .chain(queues.iter_mut().map(|event| &mut event.write.value))
        {
            self.import_task_sources(value, point, state);
        }
    }

    pub(super) fn read_memory(&self, sample: Sample, read: ReadView<'_>) -> Value {
        let current = self.sample_memory(&sample, read);
        read.observe(sample, current)
    }

    pub(super) fn sample_memory(&self, sample: &Sample, read: ReadView<'_>) -> Value {
        let storage = match &sample.memory {
            Memory::Frame {
                scope: FrameScope::Current,
                function,
                local,
                ..
            } if *function == self.function.def_id => Storage::Cell(*local),
            Memory::Frame { .. } | Memory::Unavailable(_) => return Value::default(),
            Memory::External(port) => Storage::External(port.clone()),
            Memory::Queue(target, ty) => Storage::Queue(target.clone(), *ty),
        };
        let mut value = read
            .state
            .stored_value(&storage)
            .map(std::borrow::Cow::into_owned)
            .unwrap_or_default()
            .sourced(&storage);
        if let Some(prefix) = read.envelope.and_then(|envelope| envelope.get(&storage)) {
            value.join(prefix);
        }
        let root = match &sample.memory {
            Memory::Frame { ty, .. } | Memory::Queue(_, ty) => *ty,
            Memory::External(port) => port.pointee,
            Memory::Unavailable(_) => sample.ty,
        };
        value.join(&self.async_memory(&storage, root, read.state));
        if let Memory::Frame { ty, .. } = sample.memory {
            let value = self.project_path(value, ty, &sample.path, read);
            if sample.path.contains(&Slot::Tail) {
                value::tainted_from(sample.ty, &value, self.table)
            } else {
                value
            }
        } else {
            sample
                .path
                .iter()
                .fold(value, |value, slot| value.project(slot))
        }
    }

    pub(super) fn resample_history(&self, history: &History, current: ReadView<'_>) -> Samples {
        history
            .facts
            .keys()
            .map(|lane| (lane.clone(), self.sample_memory(&lane.sample, current)))
            .collect()
    }

    pub(super) fn remember_samples(
        &mut self,
        samples: Samples,
        history: &mut History,
        point: Point,
        state: &mut State,
    ) -> bool {
        let mut changed = false;
        for (lane, mut value) in samples {
            self.record_path_store(
                point,
                Coordinate {
                    storage: Storage::Observation(lane.clone()),
                    path: Vec::new(),
                },
                &value,
                false,
            );
            value.clear_sources();
            state
                .observations
                .entry(lane.clone())
                .or_default()
                .join(&value);
            changed |= history.insert(lane, Carrier::seal(value));
        }
        changed
    }
}

#[cfg(test)]
mod tests;
