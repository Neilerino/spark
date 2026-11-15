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
    residuals: &'a [super::super::residual::Import],
    guarded: FxHashSet<Local>,
    entry: &'a State,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Cut {
    memory: Vec<usize>,
    queues: Vec<usize>,
    residuals: Vec<usize>,
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
    residuals:
        FxHashMap<std::sync::Arc<super::super::residual::Key>, super::super::residual::Import>,
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
            let (records, unknown) = super::views::pending_records_at(
                value,
                &[],
                handle,
                state,
                &Storage::Logical(local),
            );
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
        residuals: &'a [super::super::residual::Import],
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
            .chain(
                residuals
                    .iter()
                    .map(|residual| &residual.visibility.fence().peers),
            )
            .flatten()
            .copied()
            .filter(|local| reads_prefix(flow, entry, *local))
            .collect();
        (!guarded.is_empty()).then_some(Self {
            memory,
            queues,
            residuals,
            guarded,
            entry,
        })
    }

    fn before(&self, cut: &Cut, observer: Local) -> Cut {
        Cut {
            residuals: cut
                .residuals
                .iter()
                .copied()
                .filter(|index| {
                    !self.residuals[*index]
                        .visibility
                        .fence()
                        .peers
                        .contains(&observer)
                })
                .collect(),
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
            residuals: (0..self.residuals.len()).collect(),
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
        let residuals: Vec<_> = cut
            .residuals
            .iter()
            .map(|index| self.residuals[*index].clone())
            .collect();
        flow.close_task_effects(
            point,
            &mut state,
            &memory,
            &queues,
            &residuals,
            &mut publications,
        );
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
        for residual in &query.residuals {
            effects
                .residuals
                .entry(residual.key.clone())
                .and_modify(|current| current.join(residual))
                .or_insert_with(|| residual.clone());
        }
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
            for mut residual in effects.residuals.values().cloned() {
                residual
                    .visit_values(|value| *value = child.transfer.freeze(std::mem::take(value)));
                query.residuals.push(residual);
            }
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
            query.memory_support = view.memory.may_write;
            query.memory = view
                .memory
                .samples()
                .filter(|(lane, _)| child.state.pending_memory.contains_key(*lane))
                .map(|(lane, value)| {
                    (
                        lane.clone(),
                        child
                            .transfer
                            .freeze(value.sourced(&Storage::PendingWrite(lane.clone()))),
                    )
                })
                .collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provenance::{
        address::OpaqueSite,
        demand::{Atom, Coordinate, Demand},
        tasks::{
            self,
            proofs::EventFence,
            residual::{Import, Key},
        },
        Origin,
    };
    use spark_mir::{BlockId, LocalDecl, MirFunction};
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::{IntWidth, LifetimeId, Ty, TypeId, TypeTable};
    use std::sync::Arc;

    fn function(types: &[TypeId]) -> MirFunction {
        MirFunction {
            def_id: DefId::new(19001),
            param_count: 0,
            return_type: types[0],
            locals: types
                .iter()
                .enumerate()
                .map(|(i, ty)| LocalDecl {
                    local: Local(i as u32),
                    ty: *ty,
                    name: None,
                    mutable: true,
                    is_user_variable: false,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: Vec::new(),
            captures: Vec::new(),
            is_async: false,
            span: Span::DUMMY,
        }
    }

    #[test]
    fn a_residual_cut_removes_only_activity_after_the_selected_observer_join() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let future = table.intern(Ty::Future { inner: int });
        let function = function(&[int, future]);
        let point = Point::new(BlockId::ENTRY, 0);
        let (target, value) = tasks::birth(function.def_id, point, future);
        let key = Arc::new(Key {
            site: OpaqueSite {
                function: function.def_id,
                point,
            },
            role: Vec::new(),
            ty: future,
            actor: Subject {
                target,
                capture: None,
            },
        });
        let imports: Vec<_> = [Local(1), Local(2)]
            .into_iter()
            .map(|observer| Import {
                key: key.clone(),
                value: value.clone(),
                callbacks: Vec::new(),
                visibility: super::super::super::residual::Visibility::Occurrence(EventFence {
                    peers: FxHashSet::from_iter([observer]),
                    ..EventFence::default()
                }),
            })
            .collect();
        let state = State::default();
        let guarded = GuardedReads {
            memory: &[],
            queues: &[],
            residuals: &imports,
            guarded: FxHashSet::default(),
            entry: &state,
        };
        let all = Cut {
            memory: Vec::new(),
            queues: Vec::new(),
            residuals: vec![0, 1],
        };
        assert_eq!(guarded.before(&all, Local(1)).residuals, vec![1]);
        assert_eq!(guarded.before(&all, Local(2)).residuals, vec![0]);
        assert_eq!(guarded.before(&all, Local(3)), all);
    }

    #[test]
    fn projected_activity_uses_only_its_producer_and_freezes_the_child_source_row() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let reference = table.intern(Ty::Ref {
            inner: int,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let future = table.intern(Ty::Future { inner: int });
        let function = function(&[int, future, reference]);
        let point = Point::new(BlockId::ENTRY, 1);
        let origin = Origin::Loan(Point::new(BlockId::ENTRY, 0));
        let (target, pending) = tasks::birth(function.def_id, point, future);
        let actor = Subject {
            target,
            capture: None,
        };
        let key = Arc::new(Key {
            site: OpaqueSite {
                function: function.def_id,
                point,
            },
            role: Vec::new(),
            ty: future,
            actor: actor.clone(),
        });
        let mut state = State::default();
        state.set(Local(1), pending.clone());
        state.set(Local(2), Value::reference(origin.clone()));
        let mut parent_state = state.clone();
        let mut child_flow = Flow::new(&function, &table, None);
        child_flow.ensure_transfer(point, &state);
        let callback = state.value(Local(2)).sourced(&Storage::Logical(Local(2)));
        child_flow.import_residual(
            Import {
                key: key.clone(),
                value: pending.clone(),
                callbacks: vec![(reference, callback)],
                visibility: super::super::super::residual::Visibility::Continuation(
                    EventFence::default(),
                ),
            },
            point,
            &mut state,
        );
        let selected = Import {
            key: key.clone(),
            value: pending.clone(),
            callbacks: vec![(
                reference,
                state.residual_callbacks[&(key.clone(), reference)]
                    .clone()
                    .sourced(&Storage::ResidualCallback(key.clone(), reference)),
            )],
            visibility: super::super::super::residual::Visibility::Continuation(
                EventFence::default(),
            ),
        };
        let mut unrelated_key = (*key).clone();
        unrelated_key.role.push(crate::provenance::Slot::Element);
        state.residuals.insert(Arc::new(unrelated_key), pending);
        let child = Snapshot {
            state: state.clone(),
            transfer: child_flow.transfers[&point].clone(),
            effects: FxHashMap::from_iter([(
                (Local(1), actor),
                Effects {
                    residuals: FxHashMap::from_iter([(key.clone(), selected)]),
                    ..Effects::default()
                },
            )]),
        };
        let publications = Publications {
            children: FxHashMap::from_iter([(Local(1), &child)]),
            ..Publications::default()
        };
        let handle = child.state.value(Local(1)).futures[&Vec::new()]
            .iter()
            .next()
            .unwrap()
            .clone();
        let mut flow = Flow::new(&function, &table, None);
        flow.ensure_transfer(point, &parent_state);
        let query = publications
            .projected(
                &mut flow,
                point,
                &mut parent_state,
                &Storage::Logical(Local(1)),
                &Vec::new(),
                &handle,
            )
            .unwrap();
        assert_eq!(
            query.residuals.len(),
            1,
            "unrelated child-state activity is not this producer's publication"
        );
        parent_state.consume(Local(2));
        flow.record_consumed(point, Local(2));
        for residual in query.residuals {
            flow.import_residual(residual, point, &mut parent_state);
        }
        let atom = |storage| Atom {
            coordinate: Coordinate {
                storage,
                path: Vec::new(),
            },
            origin: origin.clone(),
            capture: None,
        };
        assert_eq!(
            flow.transfers[&point].before(&Demand::from_iter([atom(Storage::ResidualCallback(
                key, reference
            ))])),
            Demand::from_iter([atom(Storage::Logical(Local(2)))])
        );
    }
}
