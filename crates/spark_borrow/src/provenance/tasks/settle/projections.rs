// TAG [SPARK-114]

//! Preserve the visibility of queue publications derived within a call prefix.

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::Local;

use super::{Flow, Handle, Path, Point, State, Storage, Subject, TaskQuery, Value};
use crate::provenance::{
    channels::{QueueEvent, QueueKey},
    demand::Transfer,
    effects::MemoryEvent,
};

pub(super) struct GuardedReads<'a> {
    memory: &'a [MemoryEvent],
    queues: &'a [QueueEvent],
    guarded: FxHashSet<Local>,
    entry: &'a State,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Cut {
    memory: Vec<usize>,
    queues: Vec<usize>,
}

struct Snapshot {
    state: State,
    transfer: Transfer,
    effects: FxHashMap<(Local, Subject), Effects>,
}

#[derive(Default)]
struct Effects {
    queues: FxHashMap<QueueKey, QueueEvent>,
    result: Value,
    normal_return: bool,
}

#[derive(Default)]
pub(super) struct Publications<'a> {
    probe: bool,
    children: FxHashMap<Local, &'a Snapshot>,
    guarded: Option<&'a FxHashSet<Local>>,
    effects: FxHashMap<(Local, Subject), Effects>,
}

fn reads_prefix(flow: &Flow<'_>, state: &State, local: Local) -> bool {
    let Some(value) = state.values.get(&local) else {
        return false;
    };
    value
        .futures
        .get(&Vec::new())
        .into_iter()
        .flatten()
        .any(|handle| {
            if !handle.pending {
                return false;
            }
            let (records, unknown) = super::views::pending_records(value, &[], handle, state);
            // A known callback with no bound inputs or ports reads the same facts in every cut.
            unknown
                || records.is_empty()
                || records
                    .iter()
                    .any(|record| flow.task_reads_caller_storage(record))
        })
}

impl<'a> GuardedReads<'a> {
    pub fn new(
        flow: &Flow<'_>,
        memory: &'a [MemoryEvent],
        queues: &'a [QueueEvent],
        entry: &'a State,
    ) -> Option<Self> {
        let guarded: FxHashSet<_> = memory
            .iter()
            .filter(|event| !event.write.value.is_empty())
            .map(|event| &event.fence.peers)
            .chain(
                queues
                    .iter()
                    .filter(|event| !event.write.value.is_empty())
                    .map(|event| &event.fence.peers),
            )
            .flatten()
            .copied()
            .filter(|local| reads_prefix(flow, entry, *local))
            .collect();
        (!guarded.is_empty()).then_some(Self {
            memory,
            queues,
            guarded,
            entry,
        })
    }

    fn before(&self, cut: &Cut, observer: Local) -> Cut {
        Cut {
            memory: cut
                .memory
                .iter()
                .copied()
                .filter(|index| !self.memory[*index].fence.peers.contains(&observer))
                .collect(),
            queues: cut
                .queues
                .iter()
                .copied()
                .filter(|index| !self.queues[*index].fence.peers.contains(&observer))
                .collect(),
        }
    }

    pub fn settle(self, flow: &mut Flow<'_>, point: Point) -> (State, Transfer) {
        let all = Cut {
            memory: self
                .memory
                .iter()
                .enumerate()
                .filter_map(|(i, event)| (!event.write.value.is_empty()).then_some(i))
                .collect(),
            queues: self
                .queues
                .iter()
                .enumerate()
                .filter_map(|(i, event)| (!event.write.value.is_empty()).then_some(i))
                .collect(),
        };
        let mut solved = FxHashMap::default();
        let entry = flow.transfers[&point].clone();
        self.solve(flow, point, &entry, &all, &mut solved);
        let snapshot = solved.remove(&all).expect("root cut was settled");
        (snapshot.state, snapshot.transfer)
    }

    fn solve(
        &self,
        flow: &mut Flow<'_>,
        point: Point,
        entry: &Transfer,
        cut: &Cut,
        solved: &mut FxHashMap<Cut, Snapshot>,
    ) {
        if solved.contains_key(cut) {
            return;
        }
        let children: FxHashMap<_, _> = self
            .guarded
            .iter()
            .copied()
            .filter_map(|observer| {
                let before = self.before(cut, observer);
                (before != *cut).then_some((observer, before))
            })
            .collect();
        // A child removes at least one event. Equivalent cuts reuse the same closure.
        for child in children.values() {
            self.solve(flow, point, entry, child, solved);
        }
        let mut publications = Publications {
            children: children
                .iter()
                .map(|(local, child)| (*local, &solved[child]))
                .collect(),
            guarded: Some(&self.guarded),
            effects: FxHashMap::default(),
            probe: false,
        };
        let mut state = self.entry.clone();
        flow.transfers.insert(point, entry.clone());
        let memory: Vec<_> = cut
            .memory
            .iter()
            .map(|index| self.memory[*index].clone())
            .collect();
        let queues: Vec<_> = cut
            .queues
            .iter()
            .map(|index| self.queues[*index].clone())
            .collect();
        flow.close_task_effects(point, &mut state, &memory, &queues, &mut publications);
        let effects = publications.effects;
        solved.insert(
            cut.clone(),
            Snapshot {
                state,
                transfer: flow.transfers[&point].clone(),
                effects,
            },
        );
    }
}

impl Publications<'_> {
    pub fn probe() -> Self {
        Self {
            probe: true,
            ..Self::default()
        }
    }

    pub const fn is_probe(&self) -> bool {
        self.probe
    }

    pub fn record(&mut self, storage: &Storage, path: &Path, handle: &Handle, query: &TaskQuery) {
        let Storage::Logical(local) = storage else {
            return;
        };
        if !path.is_empty() || !self.guarded.is_some_and(|guarded| guarded.contains(local)) {
            return;
        }
        let effects = self
            .effects
            .entry((*local, Subject::from(handle)))
            .or_default();
        effects.normal_return |= query.normal_return;
        effects.result.join(&query.result);
        for event in &query.writes {
            let key = (event.site, event.write.target.clone(), event.write.ty);
            effects
                .queues
                .entry(key)
                .and_modify(|current| {
                    current.write.value.join(&event.write.value);
                })
                .or_insert_with(|| event.clone());
        }
    }

    pub fn projected(
        &self,
        flow: &mut Flow<'_>,
        point: Point,
        state: &mut State,
        storage: &Storage,
        path: &Path,
        handle: &Handle,
    ) -> Option<TaskQuery> {
        let Storage::Logical(local) = storage else {
            return None;
        };
        if !path.is_empty() {
            return None;
        }
        let child = self.children.get(local)?;
        let subject = Subject::from(handle);
        let mut query = TaskQuery::default();
        if let Some(effects) = child.effects.get(&(*local, subject.clone())) {
            query.normal_return = effects.normal_return;
            query.result = effects.result.clone();
            query.writes.extend(effects.queues.values().cloned());
        }
        if let Some(view) = child
            .state
            .values
            .get(local)
            .and_then(|value| value.task_views.get(path))
            .and_then(|views| views.get(&subject))
        {
            let mut history = view.history.clone();
            let samples = history.projected_samples(&child.state, &child.transfer);
            flow.remember_samples(samples, &mut history, point, state);
            query.history = Some(history);
        }
        state
            .exposed_queues
            .extend(child.state.exposed_queues.iter().cloned());
        flow.transfers
            .get_mut(&point)
            .expect("current transfer")
            .seeds
            .extend(child.transfer.seeds.iter().cloned());
        Some(query)
    }
}
