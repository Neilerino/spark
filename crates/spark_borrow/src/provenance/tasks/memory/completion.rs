// TAG [SPARK-114]

//! Actual copy roles reserved for one normal continuation.

use std::rc::Rc;

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::Local;

use crate::provenance::{
    demand::{Coordinate, Storage},
    storage::prefix,
    tasks::{Role, Witness},
    Flow, Point, State, Value,
};

#[derive(Clone)]
pub(in crate::provenance) struct Scope {
    point: Point,
    operation: Rc<()>,
    entries: Vec<Entry>,
    writes: FxHashSet<Coordinate>,
    task_writes: FxHashMap<Coordinate, FxHashSet<Role>>,
    settled_imports: FxHashSet<Coordinate>,
    unknown_write: bool,
    finals: Vec<FinalWrite>,
    new_settled: FxHashMap<super::Sample, Value>,
    independent_imports: FxHashSet<Coordinate>,
    completion_batches: FxHashMap<Coordinate, FxHashSet<Role>>,
    consumed_completions: FxHashSet<(usize, Role)>,
}

#[derive(Clone)]
struct FinalWrite {
    request: usize,
    role: Role,
    location: super::Sample,
    destination: Coordinate,
    support: Vec<Coordinate>,
    value: Value,
}

pub(super) fn overlaps(left: &Coordinate, right: &Coordinate) -> bool {
    left.storage == right.storage && crate::provenance::storage::overlaps(&left.path, &right.path)
}

impl FinalWrite {
    fn conflicts(&self, write: &Coordinate) -> bool {
        overlaps(&self.destination, write) || self.support.iter().any(|read| overlaps(read, write))
    }
}

#[derive(Clone)]
struct Entry {
    witness: Witness,
    phase: Phase,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Reserved,
    Settled,
    Finishing,
    Finished,
}

pub(in crate::provenance) struct Request {
    operation: Rc<()>,
    index: usize,
}

pub(in crate::provenance) struct SettledCompletion(Request);

impl Scope {
    fn new(point: Point) -> Self {
        Self {
            point,
            operation: Rc::new(()),
            entries: Vec::new(),
            writes: FxHashSet::default(),
            task_writes: FxHashMap::default(),
            settled_imports: FxHashSet::default(),
            unknown_write: false,
            finals: Vec::new(),
            new_settled: FxHashMap::default(),
            independent_imports: FxHashSet::default(),
            completion_batches: FxHashMap::default(),
            consumed_completions: FxHashSet::default(),
        }
    }

    pub(in crate::provenance) fn retire_frames(
        &mut self,
        boundary: crate::provenance::address::OpaqueSite,
    ) {
        if self.point != boundary.point {
            return;
        }
        for value in self
            .new_settled
            .values_mut()
            .chain(self.finals.iter_mut().map(|write| &mut write.value))
        {
            value.retire_frames(boundary);
        }
    }

    fn reserve(&mut self, witness: Witness) -> Request {
        let index = self.entries.len();
        self.entries.push(Entry {
            witness,
            phase: Phase::Reserved,
        });
        Request {
            operation: self.operation.clone(),
            index,
        }
    }

    fn entry(&self, request: &Request) -> Option<&Entry> {
        Rc::ptr_eq(&self.operation, &request.operation)
            .then(|| self.entries.get(request.index))
            .flatten()
    }

    fn entry_mut(&mut self, request: &Request) -> Option<&mut Entry> {
        Rc::ptr_eq(&self.operation, &request.operation)
            .then(|| self.entries.get_mut(request.index))
            .flatten()
    }

    pub(in crate::provenance) fn retain_common_sources(&mut self, other: &Self) {
        if !Rc::ptr_eq(&self.operation, &other.operation) {
            self.discard_roles(|_| true);
            self.finals.clear();
            self.consumed_completions.clear();
            self.unknown_write = true;
            return;
        }
        for (destination, roles) in &other.task_writes {
            self.task_writes
                .entry(destination.clone())
                .and_modify(|existing| existing.retain(|role| roles.contains(role)))
                .or_insert_with(|| roles.clone());
        }
        self.writes.extend(other.writes.iter().cloned());
        self.settled_imports
            .extend(other.settled_imports.iter().cloned());
        for (sample, value) in &other.new_settled {
            self.new_settled
                .entry(sample.clone())
                .or_default()
                .join(value);
        }
        self.independent_imports
            .extend(other.independent_imports.iter().cloned());
        for (destination, roles) in &other.completion_batches {
            self.completion_batches
                .entry(destination.clone())
                .and_modify(|existing| {
                    existing.retain(|role| roles.contains(role));
                })
                .or_insert_with(|| roles.clone());
        }
        self.unknown_write |= other.unknown_write;
        self.consumed_completions
            .retain(|source| other.consumed_completions.contains(source));
        // The two prefix replays only merge their effects; neither authorizes a final write here.
        self.finals.clear();
        for (index, entry) in self.entries.iter_mut().enumerate() {
            entry.witness.peers.retain(|role| {
                other.entries.get(index).is_some_and(|other| {
                    other.phase == entry.phase && other.witness.peers.contains(role)
                })
            });
        }
    }

    fn discard_roles(&mut self, mut affected: impl FnMut(&Role) -> bool) {
        for entry in &mut self.entries {
            entry.witness.peers.retain(|role| !affected(role));
        }
        for roles in self.task_writes.values_mut() {
            roles.retain(|role| !affected(role));
        }
    }
}

impl Request {
    pub(in crate::provenance) fn witness(&self, flow: &Flow<'_>, point: Point) -> Option<Witness> {
        Some(
            flow.completion_scope_at(point)?
                .entry(self)?
                .witness
                .clone(),
        )
    }

    pub(in crate::provenance) fn retains_role(
        &self,
        flow: &Flow<'_>,
        point: Point,
        role: &Role,
    ) -> bool {
        flow.completion_scope_at(point)
            .and_then(|scope| scope.entry(self))
            .is_some_and(|entry| entry.witness.peers.contains(role))
    }

    pub(super) fn settle(self, flow: &mut Flow<'_>, point: Point) -> Option<SettledCompletion> {
        let entry = flow.completion_scope_mut(point)?.entry_mut(&self)?;
        entry.phase = Phase::Settled;
        Some(SettledCompletion(self))
    }
}

impl SettledCompletion {
    pub(in crate::provenance) fn contains_local(
        &self,
        flow: &Flow<'_>,
        point: Point,
        local: Local,
    ) -> bool {
        flow.completion_scope_at(point)
            .and_then(|scope| scope.entry(&self.0))
            .is_some_and(|entry| entry.witness.contains_local(local))
    }

    pub(in crate::provenance) fn finish(
        self,
        flow: &mut Flow<'_>,
        result: Option<&Value>,
        point: Point,
        state: &mut State,
    ) {
        let Some(entry) = flow
            .completion_scope_mut(point)
            .and_then(|scope| scope.entry_mut(&self.0))
        else {
            return;
        };
        match entry.phase {
            Phase::Settled => {
                entry.phase = Phase::Finishing;
                let witness = entry.witness.clone();
                flow.finish_settled_future(witness, point, state);
                if let Some(entry) = flow
                    .completion_scope_mut(point)
                    .and_then(|scope| scope.entry_mut(&self.0))
                {
                    entry.phase = Phase::Finished;
                }
            }
            Phase::Finished if result.is_some() => {}
            _ => return,
        }
        flow.mark_future_progress(&self.0, result, point, state);
    }
}

impl Flow<'_> {
    fn completion_scope_at(&self, point: Point) -> Option<&Scope> {
        self.completion_scope
            .as_ref()
            .filter(|scope| scope.point == point)
    }

    fn completion_scope_mut(&mut self, point: Point) -> Option<&mut Scope> {
        self.completion_scope
            .as_mut()
            .filter(|scope| scope.point == point)
    }

    pub(in crate::provenance) fn reserve_completions(
        &mut self,
        witnesses: Vec<Witness>,
        point: Point,
    ) -> Vec<Request> {
        if self.completion_scope_at(point).is_none() {
            self.completion_scope = Some(Scope::new(point));
        }
        witnesses
            .into_iter()
            .filter_map(|witness| {
                self.completion_scope_mut(point)
                    .map(|scope| scope.reserve(witness))
            })
            .collect()
    }

    pub(super) fn restrict_completion_requests(
        &mut self,
        requests: &[Request],
        clobbered: &[Local],
        point: Point,
    ) {
        if let Some(scope) = self.completion_scope_mut(point) {
            for request in requests {
                if let Some(entry) = scope.entry_mut(request) {
                    entry.witness.exclude(clobbered);
                }
            }
        }
    }

    pub(in crate::provenance) fn finish_preselected_capture(
        &mut self,
        role: &Role,
        point: Point,
        state: &mut State,
    ) -> bool {
        let Some(scope) = self.completion_scope_at(point) else {
            return false;
        };
        let Some(index) = scope
            .entries
            .iter()
            .position(|entry| entry.phase != Phase::Reserved && entry.witness.peers.contains(role))
        else {
            return false;
        };
        SettledCompletion(Request {
            operation: scope.operation.clone(),
            index,
        })
        .finish(self, None, point, state);
        true
    }

    pub(super) fn protect_previous_final_writes(
        &mut self,
        destinations: &[Coordinate],
        point: Point,
    ) {
        if let Some(scope) = self.completion_scope_mut(point) {
            scope.finals.retain(|candidate| {
                !destinations
                    .iter()
                    .any(|destination| candidate.conflicts(destination))
            });
        }
    }

    pub(super) fn record_batch_source(
        &mut self,
        destination: Coordinate,
        peers: Option<&FxHashSet<Role>>,
        point: Point,
    ) {
        if let Some(scope) = self.completion_scope_mut(point) {
            scope
                .completion_batches
                .entry(destination)
                .and_modify(|roles| {
                    roles.retain(|existing| peers.is_some_and(|peers| peers.contains(existing)));
                })
                .or_insert_with(|| peers.cloned().unwrap_or_default());
        }
    }

    pub(super) fn remember_new_settled(
        &mut self,
        sample: &super::Sample,
        value: &Value,
        point: Point,
    ) {
        if let Some(scope) = self.completion_scope_mut(point) {
            scope
                .new_settled
                .entry(sample.clone())
                .or_default()
                .join(value);
        }
    }

    pub(super) fn clear_new_settled(
        &mut self,
        sample: &super::Sample,
        path: &[crate::provenance::Slot],
        point: Point,
    ) {
        if let Some(scope) = self.completion_scope_mut(point) {
            if path.is_empty() {
                scope.new_settled.remove(sample);
            } else if let Some(value) = scope.new_settled.get_mut(sample) {
                value.clear_path(path);
            }
        }
    }

    pub(super) fn new_settled_contribution(
        &self,
        sample: &super::Sample,
        point: Point,
    ) -> Option<&Value> {
        let scope = self.completion_scope_at(point)?;
        if scope.unknown_write {
            return None;
        }
        scope.new_settled.get(sample)
    }

    pub(super) fn settlement_unchanged_since_entry(
        &self,
        sample: &Coordinate,
        point: Point,
    ) -> bool {
        self.completion_scope_at(point).is_some_and(|scope| {
            !scope.unknown_write
                && !scope
                    .settled_imports
                    .iter()
                    .any(|write| overlaps(sample, write))
        })
    }

    pub(in crate::provenance) fn helper_clear_covers_settlement(
        &self,
        destination: &Coordinate,
        peers: &FxHashSet<Role>,
        point: Point,
    ) -> bool {
        let Some(scope) = self.completion_scope_at(point) else {
            return false;
        };
        let finished = |role: &Role| {
            peers.contains(role)
                && scope.entries.iter().enumerate().any(|(index, entry)| {
                    entry.phase == Phase::Finished
                        && (entry.witness.peers.contains(role)
                            || scope.consumed_completions.contains(&(index, role.clone())))
                })
        };
        peers.iter().any(finished)
            && !scope.unknown_write
            && !scope
                .independent_imports
                .iter()
                .any(|written| overlaps(destination, written))
            && scope.completion_batches.iter().all(|(written, roles)| {
                !overlaps(destination, written) || roles.iter().any(finished)
            })
    }

    pub(super) fn remember_completion_batch(
        &mut self,
        destinations: Vec<Coordinate>,
        point: Point,
    ) {
        if let Some(scope) = self.completion_scope_mut(point) {
            scope.settled_imports.extend(destinations);
        }
    }

    pub(super) fn stage_final_write(
        &mut self,
        role: Role,
        location: super::Sample,
        destination: Coordinate,
        support: Vec<Coordinate>,
        value: Value,
        point: Point,
    ) {
        let Some(scope) = self.completion_scope_mut(point) else {
            return;
        };
        let Some(request) = scope.entries.iter().position(|entry| {
            entry.phase == Phase::Reserved && entry.witness.peers.contains(&role)
        }) else {
            return;
        };
        let candidate = FinalWrite {
            request,
            role,
            location,
            destination,
            support,
            value,
        };
        if !scope.unknown_write
            && !scope.task_writes.iter().any(|(destination, own)| {
                candidate.conflicts(destination) && !own.contains(&candidate.role)
            })
            && !scope
                .writes
                .iter()
                .chain(&scope.settled_imports)
                .any(|write| candidate.conflicts(write))
        {
            scope.finals.push(candidate);
        }
    }

    pub(in crate::provenance) fn finish_final_writes(&mut self, point: Point, state: &mut State) {
        let Some(scope) = self.completion_scope_mut(point) else {
            return;
        };
        let mut writes = std::mem::take(&mut scope.finals);
        writes.retain(|write| {
            scope.entries.get(write.request).is_some_and(|entry| {
                entry.phase == Phase::Finished
                    && (entry.witness.peers.contains(&write.role)
                        || scope
                            .consumed_completions
                            .contains(&(write.request, write.role.clone())))
            })
        });
        let mut conflicts = FxHashSet::default();
        for (index, left) in writes.iter().enumerate() {
            for (other, right) in writes.iter().enumerate().skip(index + 1) {
                if left.conflicts(&right.destination) || right.conflicts(&left.destination) {
                    conflicts.extend([index, other]);
                }
            }
        }
        for (index, write) in writes.into_iter().enumerate() {
            if !conflicts.contains(&index) {
                self.clear_settled_memory(&write.destination, point, state);
                self.store_settled_sample(write.location, write.value, point, state);
            }
        }
    }

    pub(super) fn trace_task_write(
        &mut self,
        destination: Coordinate,
        emitter: Option<&FxHashSet<Role>>,
        point: Point,
    ) {
        if let Some(scope) = self.completion_scope_mut(point) {
            scope.finals.retain(|write| {
                !write.conflicts(&destination)
                    || emitter.is_some_and(|roles| roles.contains(&write.role))
            });
            scope
                .task_writes
                .entry(destination)
                .and_modify(|roles| {
                    roles.retain(|role| emitter.is_some_and(|own| own.contains(role)));
                })
                .or_insert_with(|| emitter.cloned().unwrap_or_default());
        }
    }

    pub(in crate::provenance) fn trace_completion_write(
        &mut self,
        destination: Coordinate,
        point: Point,
    ) {
        if let Some(scope) = self.completion_scope_mut(point) {
            scope.finals.retain(|write| !write.conflicts(&destination));
            scope.writes.insert(destination);
        }
    }

    pub(super) fn trace_completion_import(&mut self, destination: Coordinate, point: Point) {
        if let Some(scope) = self.completion_scope_mut(point) {
            scope.finals.retain(|write| !write.conflicts(&destination));
            scope.independent_imports.insert(destination.clone());
            scope.settled_imports.insert(destination);
        }
    }

    pub(in crate::provenance) fn trace_unknown_completion_write(&mut self, point: Point) {
        if let Some(scope) = self.completion_scope_mut(point) {
            scope.unknown_write = true;
            scope.finals.clear();
        }
    }

    pub(in crate::provenance) fn overwrite_completion_sources(
        &mut self,
        destination: &Coordinate,
        point: Point,
    ) {
        let Some(scope) = self.completion_scope_mut(point) else {
            return;
        };
        let affected = |role: &Role| {
            let storage = role.storage();
            if storage == destination.storage {
                return prefix(&destination.path, role.selection())
                    || prefix(role.selection(), &destination.path);
            }
            match destination.storage {
                Storage::Logical(local) | Storage::Cell(local) => {
                    role.local() == Some(local) && destination.path.is_empty()
                }
                _ => false,
            }
        };
        scope
            .consumed_completions
            .retain(|(_, role)| !affected(role));
        for peers in scope.completion_batches.values_mut() {
            peers.retain(|role| !affected(role));
        }
        scope.discard_roles(affected);
    }

    pub(in crate::provenance) fn consume_completion_local(&mut self, local: Local, point: Point) {
        if let Some(scope) = self.completion_scope_mut(point) {
            for (index, entry) in scope.entries.iter().enumerate() {
                let role = Role::root(local);
                if matches!(entry.phase, Phase::Settled | Phase::Finished)
                    && entry.witness.peers.contains(&role)
                {
                    scope.consumed_completions.insert((index, role));
                }
            }
            scope.discard_roles(|role| role.local() == Some(local));
        }
    }

    pub(in crate::provenance) fn clobber_completion_local(&mut self, local: Local, point: Point) {
        if let Some(scope) = self.completion_scope_mut(point) {
            scope
                .consumed_completions
                .retain(|(_, role)| role.local() != Some(local));
            for peers in scope.completion_batches.values_mut() {
                peers.retain(|role| role.local() != Some(local));
            }
            scope.discard_roles(|role| role.local() == Some(local));
        }
    }

    pub(in crate::provenance) fn forget_completion_sources(&mut self, point: Point) {
        if let Some(scope) = self.completion_scope_mut(point) {
            scope.consumed_completions.clear();
            for peers in scope.completion_batches.values_mut() {
                peers.clear();
            }
            scope.discard_roles(|_| true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provenance::Slot;

    #[test]
    fn a_widened_write_conflicts_with_a_concrete_descendant() {
        let left = Coordinate {
            storage: Storage::Cell(Local(1)),
            path: vec![Slot::Tuple(0), Slot::Tail],
        };
        let mut right = Coordinate {
            storage: left.storage.clone(),
            path: vec![Slot::Tuple(0), Slot::Tuple(1)],
        };
        assert!(overlaps(&left, &right));
        right.path[0] = Slot::Tuple(1);
        assert!(!overlaps(&left, &right));
    }
}
