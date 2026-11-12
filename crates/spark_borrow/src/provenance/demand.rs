//! Backward demand over individual reaching reference contributions.

// TAG [SPARK-114]

use std::borrow::Cow;

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::Local;

use super::{aliases::Port, Origin, Path, Slot};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Storage {
    Residual(std::sync::Arc<super::tasks::residual::Key>),
    ResidualCallback(
        std::sync::Arc<super::tasks::residual::Key>,
        spark_types::TypeId,
    ),
    PendingWrite(std::sync::Arc<super::tasks::memory::Lane>),
    SettledMemory(super::observations::Sample),
    Logical(Local),
    Cell(Local),
    External(Port),
    Queue(super::channels::Target, spark_types::TypeId),
    Task(super::tasks::Target, spark_types::TypeId),
    ExecutionCallback(Local, spark_types::TypeId),
    Observation(std::sync::Arc<super::observations::Lane>),
    CallbackNode(Box<super::tasks::activity::Location>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Coordinate {
    pub storage: Storage,
    pub path: Path,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Atom {
    pub coordinate: Coordinate,
    pub origin: Origin,
    pub capture: Option<super::value::ReferenceCapture>,
}

pub(super) type Demand = FxHashSet<Atom>;
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Source {
    Current(Atom),
    Entry(Atom),
}

pub(super) type Frontier = FxHashSet<Source>;
pub(super) type Sources = FxHashMap<super::value::Contribution, Frontier>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Definition {
    pub storage: Storage,
    pub prefix: Path,
}

impl Definition {
    fn includes(&self, atom: &Atom) -> bool {
        let nested = matches!(&atom.coordinate.storage, Storage::CallbackNode(location) if location.is_below(&self.storage, &self.prefix));
        nested
            || self.storage == atom.coordinate.storage
                && !self.prefix.contains(&Slot::Tail)
                && atom.coordinate.path.starts_with(&self.prefix)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Transfer {
    support: Demand,
    rows: FxHashMap<Atom, Demand>,
    pub seeds: Demand,
}

impl Transfer {
    pub fn at_entry(support: Demand) -> Self {
        Self {
            support,
            ..Self::default()
        }
    }

    fn current(&self, atom: &Atom) -> Demand {
        if self.support.contains(atom) {
            self.rows
                .get(atom)
                .cloned()
                .unwrap_or_else(|| Demand::from_iter([atom.clone()]))
        } else {
            Demand::default()
        }
    }

    fn resolve(&self, sources: &Frontier) -> Demand {
        sources
            .iter()
            .flat_map(|source| match source {
                Source::Current(atom) => self.current(atom),
                Source::Entry(atom) => Demand::from_iter([atom.clone()]),
            })
            .collect()
    }

    pub(super) fn freeze_annotations(&self, sources: &mut Sources) {
        for frontier in sources.values_mut() {
            *frontier = self
                .resolve(frontier)
                .into_iter()
                .map(Source::Entry)
                .collect();
        }
    }

    pub(super) fn freeze(&self, mut value: super::Value) -> super::Value {
        self.freeze_annotations(&mut value.sources);
        value.visit_callback_sources(|sources| self.freeze_annotations(sources));
        value.visit_task_progress(|history, memory| {
            history.freeze_imports(self);
            memory.freeze_imports(self);
        });
        value
    }

    fn define(&mut self, definition: &Definition) {
        self.support.retain(|atom| !definition.includes(atom));
        self.rows.retain(|atom, _| !definition.includes(atom));
    }

    fn store(
        &mut self,
        definition: &Definition,
        incoming: FxHashMap<Atom, Frontier>,
        strong: bool,
    ) {
        let incoming: Vec<_> = incoming
            .into_iter()
            .map(|(atom, sources)| (atom, self.resolve(&sources)))
            .collect();
        if strong {
            self.define(definition);
        }
        for (atom, sources) in incoming {
            let mut row = self.current(&atom);
            row.extend(sources);
            self.support.insert(atom.clone());
            self.rows.insert(atom, row);
        }
    }

    pub fn seed(&mut self, sources: &Frontier) {
        self.seeds.extend(self.resolve(sources));
    }

    pub fn same_sources(&self, other: &Self) -> bool {
        self.support == other.support && self.rows == other.rows
    }

    pub fn join_alternative(&mut self, other: &Self) {
        let support: Demand = self.support.union(&other.support).cloned().collect();
        self.rows = support
            .iter()
            .map(|atom| {
                let mut row = self.current(atom);
                row.extend(other.current(atom));
                (atom.clone(), row)
            })
            .collect();
        self.support = support;
        self.seeds.extend(other.seeds.iter().cloned());
    }

    pub fn before(&self, after: &Demand) -> Demand {
        let mut before = self.seeds.clone();
        for atom in after {
            before.extend(self.current(atom));
        }
        before
    }
}

impl super::Flow<'_> {
    pub(super) fn ensure_transfer(&mut self, point: super::Point, state: &super::State) {
        self.transfers
            .entry(point)
            .or_insert_with(|| Transfer::at_entry(state.atoms()));
    }

    pub(super) fn freeze_sources(&self, point: super::Point, value: super::Value) -> super::Value {
        self.transfers[&point].freeze(value)
    }

    pub(super) fn record_consumed(&mut self, point: super::Point, local: Local) {
        self.transfers.get_mut(&point).unwrap().define(&Definition {
            storage: Storage::Logical(local),
            prefix: Vec::new(),
        });
    }

    pub(super) fn record_store(
        &mut self,
        point: super::Point,
        place: &spark_mir::Place,
        value: &super::Value,
    ) {
        let value = self.freeze_sources(point, value.clone());
        let root = self.function.locals[place.local.index()].ty;
        let (prefix, _) = self.projection_path(root, &place.projections);
        for storage in [Storage::Logical(place.local), Storage::Cell(place.local)] {
            self.record_path_store(
                point,
                Coordinate {
                    storage,
                    path: prefix.clone(),
                },
                &value,
                place.projections.is_empty(),
            );
        }
    }

    pub(super) fn storage_type(&self, storage: &Storage) -> spark_types::TypeId {
        match storage {
            Storage::CallbackNode(location) => location.ty(),
            Storage::Logical(local) | Storage::Cell(local) => {
                self.function.locals[local.index()].ty
            }
            Storage::External(port) => port.pointee,
            Storage::Queue(_, ty)
            | Storage::Task(_, ty)
            | Storage::ExecutionCallback(_, ty)
            | Storage::ResidualCallback(_, ty) => *ty,
            Storage::Observation(lane) => lane.sample.ty,
            Storage::PendingWrite(lane) => lane.location.ty,
            Storage::SettledMemory(location) => location.ty,
            Storage::Residual(key) => key.ty,
        }
    }

    pub(super) fn record_path_store(
        &mut self,
        point: super::Point,
        destination: Coordinate,
        value: &super::Value,
        strong: bool,
    ) {
        let root = self.storage_type(&destination.storage);
        let prefixed = value.prefixed_path(&destination.path, root, self.table);
        let callbacks = prefixed.callback_storage_values(&destination.storage);
        self.record_flat_path_store(point, destination, value, strong);
        for (storage, callback) in callbacks {
            self.record_flat_path_store(
                point,
                Coordinate {
                    storage,
                    path: Vec::new(),
                },
                &callback,
                strong,
            );
        }
    }

    fn record_flat_path_store(
        &mut self,
        point: super::Point,
        destination: Coordinate,
        value: &super::Value,
        strong: bool,
    ) {
        let Coordinate {
            storage,
            path: prefix,
        } = destination;
        let root = self.storage_type(&storage);
        let mut incoming = FxHashMap::default();
        for (path, origin, capture) in value.leaves.keys() {
            if *origin == Origin::ValidationHole {
                continue;
            }
            let destination = Atom {
                coordinate: Coordinate {
                    storage: storage.clone(),
                    path: super::value::normalize(
                        root,
                        [prefix.clone(), path.clone()].concat(),
                        self.table,
                    ),
                },
                origin: origin.clone(),
                capture: *capture,
            };
            incoming
                .entry(destination)
                .or_insert_with(Frontier::default)
                .extend(
                    value
                        .sources
                        .get(&(path.clone(), origin.clone(), *capture))
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
        }
        self.transfers.get_mut(&point).unwrap().store(
            &Definition { storage, prefix },
            incoming,
            strong,
        );
    }

    fn external_return_frontier(&self, state: &super::State) -> Frontier {
        state
            .external
            .iter()
            .map(|(port, value)| value.clone().sourced(&Storage::External(port.clone())))
            .chain(Self::external_async_values(state))
            .flat_map(|value| {
                self.reachable_queue_payloads(&value, state)
                    .sources
                    .into_values()
                    .flatten()
            })
            .collect()
    }

    pub(super) fn carrier_regions(
        &mut self,
        facts: &crate::facts::BorrowFacts,
    ) -> Vec<crate::regions::Region> {
        use super::Point;
        let mut exits = FxHashMap::default();
        self.transfers.clear();
        for index in 0..self.entries.len() {
            if let Some(mut state) = self.entries[index].clone() {
                let block = spark_mir::BlockId(index as u32);
                self.transfer_block(block, &mut state);
                exits.insert(block, state.atoms());
                if matches!(
                    self.function.blocks[index].terminator,
                    spark_mir::Terminator::Return
                ) {
                    let point =
                        Point::new(block, self.function.blocks[index].statements.len() as u32);
                    let returned = state
                        .value(Local::RETURN_PLACE)
                        .sourced(&Storage::Logical(Local::RETURN_PLACE));
                    let returned = self.reachable_queue_payloads(&returned, &state);
                    let external = self.external_return_frontier(&state);
                    self.transfers.entry(point).or_default().seed(
                        &returned
                            .sources
                            .into_values()
                            .flatten()
                            .chain(external)
                            .collect(),
                    );
                }
            }
        }
        let (before, after) = solve(&self.transfers, facts, &exits);
        let mut regions: Vec<_> = facts
            .loans
            .iter()
            .map(|loan| crate::regions::Region {
                loan: loan.id,
                points: FxHashSet::default(),
            })
            .collect();
        for index in 0..self.entries.len() {
            let Some(mut state) = self.entries[index].clone() else {
                continue;
            };
            let block = spark_mir::BlockId(index as u32);
            for statement_index in 0..=self.function.blocks[index].statements.len() {
                let point = Point::new(block, statement_index as u32);
                let mut active = super::Origins::default();
                for atom in before.get(&point).into_iter().flatten() {
                    if state.contains(atom) {
                        active.extend(state.support(atom));
                    }
                }
                if let Some(statement) = self.function.blocks[index].statements.get(statement_index)
                {
                    self.transfer_statement(statement, point, &mut state);
                } else {
                    self.transfer_terminator(block, &mut state);
                }
                for atom in after.get(&point).into_iter().flatten() {
                    if state.contains(atom) {
                        active.extend(state.support(atom));
                    }
                }
                for access in self.accesses.get(&point).into_iter().flatten() {
                    active.insert(access.origin.clone());
                }
                for (loan, region) in facts.loans.iter().zip(&mut regions) {
                    if loan.issued_at == point
                        || active.iter().any(|origin| {
                            matches!(origin, Origin::Loan(site) if *site == loan.issued_at)
                                || matches!(origin, Origin::Frame { function, point: site } if *function == self.function.def_id && *site == loan.issued_at)
                        })
                    {
                        region.points.insert(point);
                    }
                }
            }
        }
        regions
    }
}

type PointDemands = FxHashMap<super::Point, Demand>;

fn solve(
    transfers: &FxHashMap<super::Point, Transfer>,
    facts: &crate::facts::BorrowFacts,
    exits: &FxHashMap<spark_mir::BlockId, Demand>,
) -> (PointDemands, PointDemands) {
    use super::Point;
    let mut successors: FxHashMap<Point, Vec<Point>> = FxHashMap::default();
    let mut predecessors: FxHashMap<Point, Vec<Point>> = FxHashMap::default();
    for (from, to) in &facts.cfg_edges {
        if transfers.contains_key(from) && transfers.contains_key(to) {
            successors.entry(*from).or_default().push(*to);
            predecessors.entry(*to).or_default().push(*from);
        }
    }
    let mut before: FxHashMap<Point, Demand> = FxHashMap::default();
    let mut after: FxHashMap<Point, Demand> = FxHashMap::default();
    let mut pending: std::collections::VecDeque<_> = transfers.keys().copied().collect();
    let mut queued: FxHashSet<_> = pending.iter().copied().collect();
    while let Some(point) = pending.pop_front() {
        queued.remove(&point);
        let mut outgoing = Demand::default();
        for next in successors.get(&point).into_iter().flatten() {
            for atom in before.get(next).into_iter().flatten() {
                if next.index != 0 || exits[&point.block].contains(atom) {
                    outgoing.insert(atom.clone());
                }
            }
        }
        let incoming = transfers[&point].before(&outgoing);
        after.insert(point, outgoing);
        if before.get(&point) != Some(&incoming) {
            before.insert(point, incoming);
            for previous in predecessors.get(&point).into_iter().flatten() {
                if queued.insert(*previous) {
                    pending.push_back(*previous);
                }
            }
        }
    }
    (before, after)
}

impl super::State {
    fn storage_containers(&self) -> Vec<(Storage, &super::Value)> {
        self.values
            .iter()
            .map(|(local, value)| (Storage::Logical(*local), value))
            .chain(
                self.cells
                    .iter()
                    .map(|(local, value)| (Storage::Cell(*local), value)),
            )
            .chain(
                self.external
                    .iter()
                    .map(|(port, value)| (Storage::External(port.clone()), value)),
            )
            .chain(
                self.queues
                    .iter()
                    .map(|((target, ty), value)| (Storage::Queue(target.clone(), *ty), value)),
            )
            .chain(
                self.executions.iter().map(|((local, ty), callback)| {
                    (Storage::ExecutionCallback(*local, *ty), callback)
                }),
            )
            .chain(self.tasks.iter().map(|(target, record)| {
                (
                    Storage::Task(target.clone(), record.callback_ty),
                    &record.callback,
                )
            }))
            .chain(
                self.observations
                    .iter()
                    .map(|(lane, value)| (Storage::Observation(lane.clone()), value)),
            )
            .chain(
                self.pending_memory
                    .iter()
                    .map(|(lane, value)| (Storage::PendingWrite(lane.clone()), value)),
            )
            .chain(
                self.settled_memory
                    .iter()
                    .map(|(location, value)| (Storage::SettledMemory(location.clone()), value)),
            )
            .chain(
                self.residuals
                    .iter()
                    .map(|(key, value)| (Storage::Residual(key.clone()), value)),
            )
            .chain(
                self.residual_callbacks
                    .iter()
                    .map(|((key, ty), value)| (Storage::ResidualCallback(key.clone(), *ty), value)),
            )
            .collect()
    }

    pub(super) fn atoms(&self) -> Demand {
        let mut result = Demand::default();
        for (storage, value) in self.storage_containers() {
            append_atoms(&mut result, &storage, value);
            for (storage, callback) in value.callback_storage_values(&storage) {
                append_atoms(&mut result, &storage, &callback);
            }
        }
        result
    }

    pub(super) fn stored_value(&self, storage: &Storage) -> Option<Cow<'_, super::Value>> {
        if let Storage::CallbackNode(location) = storage {
            let root = self.stored_terminal_value(location.root())?;
            let callback = root
                .task_views
                .get(location.path())?
                .get(location.actor())?
                .callbacks
                .get(&location.callback_type())?;
            return callback.node_value(location.node()).map(Cow::Owned);
        }
        self.stored_terminal_value(storage).map(Cow::Borrowed)
    }

    fn stored_terminal_value(&self, storage: &Storage) -> Option<&super::Value> {
        match storage {
            Storage::CallbackNode(_) => None,
            Storage::Logical(local) => self.values.get(local),
            Storage::Cell(local) => self.cells.get(local),
            Storage::External(port) => self.external.get(port),
            Storage::Queue(target, ty) => self.queues.get(&(target.clone(), *ty)),
            Storage::Task(target, _) => self.tasks.get(target).map(|record| &record.callback),
            Storage::ExecutionCallback(local, ty) => self.executions.get(&(*local, *ty)),
            Storage::Observation(lane) => self.observations.get(lane),
            Storage::PendingWrite(lane) => self.pending_memory.get(lane),
            Storage::SettledMemory(location) => self.settled_memory.get(location),
            Storage::Residual(key) => self.residuals.get(key),
            Storage::ResidualCallback(key, ty) => self.residual_callbacks.get(&(key.clone(), *ty)),
        }
    }

    fn support(&self, atom: &Atom) -> super::Origins {
        let mut result = super::Origins::from_iter([atom.origin.clone()]);
        if let Some(value) = self.stored_value(&atom.coordinate.storage) {
            if let Some(leaf) = value.leaves.get(&(
                atom.coordinate.path.clone(),
                atom.origin.clone(),
                atom.capture,
            )) {
                result.extend(leaf.parents.iter().cloned());
            }
        }
        result
    }

    fn contains(&self, atom: &Atom) -> bool {
        self.stored_value(&atom.coordinate.storage)
            .is_some_and(|value| {
                value.leaves.contains_key(&(
                    atom.coordinate.path.clone(),
                    atom.origin.clone(),
                    atom.capture,
                ))
            })
    }
}

fn append_atoms(result: &mut Demand, storage: &Storage, value: &super::Value) {
    result.extend(
        value
            .leaves
            .keys()
            .filter(|(_, origin, _)| *origin != Origin::ValidationHole)
            .map(|(path, origin, capture)| Atom {
                coordinate: Coordinate {
                    storage: storage.clone(),
                    path: path.clone(),
                },
                origin: origin.clone(),
                capture: *capture,
            }),
    );
}

#[cfg(test)]
mod tests;
