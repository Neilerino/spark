// TAG [SPARK-114]

//! Finite task capabilities; source coordinates do not imply unique task instances.

use rustc_hash::FxHashSet;
use spark_mir::Local;
use spark_resolve::DefId;
use spark_types::{Ty, TypeId, TypeTable};

use super::{address::OpaqueSite, value, Path, Point, Slot, Value};

mod identity;
pub(super) mod proofs;
pub(super) use identity::{Copies, Witness};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Capture {
    pub owner: DefId,
    pub index: usize,
    pub ty: TypeId,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Target {
    Input {
        input: Local,
        path: Path,
        ty: TypeId,
        capture: Option<Capture>,
        queue: Option<super::address::QueueSelection>,
    },
    Birth {
        site: OpaqueSite,
        constructor: OpaqueSite,
        ty: TypeId,
    },
    Unknown {
        site: OpaqueSite,
        ty: TypeId,
    },
    Validation {
        site: OpaqueSite,
        path: Path,
        ty: TypeId,
    },
}

impl Target {
    pub const fn ty(&self) -> TypeId {
        match self {
            Self::Input { ty, .. }
            | Self::Birth { ty, .. }
            | Self::Unknown { ty, .. }
            | Self::Validation { ty, .. } => *ty,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Handle {
    pub target: Target,
    pub pending: bool,
    pub capture: Option<Capture>,
}

#[derive(Clone, Debug)]
pub(super) struct Record {
    pub callback: Value,
    pub callback_ty: TypeId,
}

impl Record {
    pub fn join(&mut self, other: &Self) -> bool {
        self.callback.join(&other.callback)
    }
}

pub(super) fn slots(ty: TypeId, table: &TypeTable) -> Vec<(Path, TypeId)> {
    fn visit(
        ty: TypeId,
        path: &mut Path,
        seen: &mut FxHashSet<TypeId>,
        table: &TypeTable,
        out: &mut Vec<(Path, TypeId)>,
    ) {
        let ty = value::strip_mutable(ty, table);
        if !seen.insert(ty) {
            return;
        }
        let children = match table.resolve(ty) {
            Ty::Future { .. } => {
                out.push((path.clone(), ty));
                Vec::new()
            }
            Ty::List(inner) | Ty::Set(inner) => vec![(Slot::Element, *inner)],
            Ty::Map(key, payload) => vec![(Slot::Key, *key), (Slot::Value, *payload)],
            _ => value::inline_slots(ty, table),
        };
        for (slot, child) in children {
            path.push(slot);
            visit(child, path, seen, table, out);
            path.pop();
        }
        seen.remove(&ty);
    }
    let mut result = Vec::new();
    visit(
        ty,
        &mut Vec::new(),
        &mut FxHashSet::default(),
        table,
        &mut result,
    );
    result
}

pub(super) fn seed_inputs(
    value: &mut Value,
    ty: TypeId,
    input: Local,
    source_ty: TypeId,
    prefix: &[Slot],
    table: &TypeTable,
) {
    let capture = prefix.last().and_then(|slot| match slot {
        Slot::Capture(owner, index, ty)
            if matches!(
                table.resolve(value::strip_mutable(*ty, table)),
                Ty::Future { .. }
            ) =>
        {
            Some(Capture {
                owner: *owner,
                index: *index,
                ty: *ty,
            })
        }
        _ => None,
    });
    for (path, ty) in slots(ty, table) {
        value
            .futures
            .entry(path.clone())
            .or_default()
            .insert(Handle {
                target: Target::Input {
                    input,
                    path: value::normalize(source_ty, [prefix.to_vec(), path].concat(), table),
                    ty,
                    capture,
                    queue: None,
                },
                pending: true,
                capture,
            });
    }
}

pub(super) fn opaque_handles(
    ty: TypeId,
    site: OpaqueSite,
    table: &TypeTable,
    validation: bool,
) -> Value {
    let mut value = Value::default();
    for (path, ty) in slots(ty, table) {
        let target = if validation {
            Target::Validation {
                site,
                path: path.clone(),
                ty,
            }
        } else {
            Target::Unknown { site, ty }
        };
        value.futures.entry(path).or_default().insert(Handle {
            target,
            pending: true,
            capture: None,
        });
    }
    value
}

pub(super) fn birth(function: DefId, point: Point, ty: TypeId) -> (Target, Value) {
    let site = OpaqueSite { function, point };
    let target = Target::Birth {
        site,
        constructor: site,
        ty,
    };
    let mut value = Value::default();
    value.futures.entry(Vec::new()).or_default().insert(Handle {
        target: target.clone(),
        pending: true,
        capture: None,
    });
    (target, value)
}

impl super::Flow<'_> {
    pub(super) fn future_copy_witness(
        &self,
        rvalue: &spark_mir::Rvalue,
        state: &super::State,
    ) -> Option<Witness> {
        let operand = match rvalue {
            spark_mir::Rvalue::Use(operand) => operand,
            spark_mir::Rvalue::Cast { operand, target_ty } => {
                let (spark_mir::Operand::Copy(place) | spark_mir::Operand::Move(place)) = operand
                else {
                    return None;
                };
                if value::strip_mutable(self.place_type(place), self.table)
                    != value::strip_mutable(*target_ty, self.table)
                {
                    return None;
                }
                operand
            }
            _ => return None,
        };
        let (spark_mir::Operand::Copy(place) | spark_mir::Operand::Move(place)) = operand else {
            return None;
        };
        (place.projections.is_empty()
            && state.live.contains(&place.local)
            && matches!(
                self.table
                    .resolve(value::strip_mutable(self.place_type(place), self.table)),
                Ty::Future { .. }
            ))
        .then(|| state.future_copies.witness(place.local))
    }
    pub(super) fn store_future_copy(
        &self,
        place: &spark_mir::Place,
        witness: Option<Witness>,
        state: &mut super::State,
    ) {
        if place.projections.is_empty()
            && matches!(
                self.table
                    .resolve(value::strip_mutable(self.place_type(place), self.table)),
                Ty::Future { .. }
            )
        {
            if let Some(witness) = witness {
                state
                    .future_copies
                    .assign(place.local, witness, &state.live);
            }
        }
    }

    pub(super) fn complete_future(&self, local: Local, result: &Value, state: &mut super::State) {
        self.complete_future_witness(state.future_copies.witness(local), Some(result), state);
    }

    pub(super) fn complete_future_witness(
        &self,
        witness: Witness,
        result: Option<&Value>,
        state: &mut super::State,
    ) {
        state.joined_inputs.extend(witness.inputs);
        for peer in witness.peers {
            if !state.live.contains(&peer) {
                continue;
            }
            let mut current = state.value(peer);
            for (path, handles) in &mut current.futures {
                if path.is_empty() || *path == [Slot::Tail] {
                    *handles = handles
                        .iter()
                        .map(|handle| Handle {
                            pending: false,
                            ..handle.clone()
                        })
                        .collect();
                }
            }
            current.leaves.retain(|(path, ..), _| *path != [Slot::Tail]);
            current
                .sources
                .retain(|(path, ..), _| *path != [Slot::Tail]);
            let ty = self.function.locals[peer.index()].ty;
            if let Some(result) = result {
                current.join(&result.prefixed(Slot::Element, ty, self.table));
            }
            state.set(peer, current);
        }
    }

    pub(super) fn apply_call_future_proof(
        &self,
        mut proof: proofs::CallProof,
        clobbered: &[Local],
        state: &mut super::State,
    ) -> Option<Witness> {
        proof.exclude(clobbered);
        for local in clobbered {
            state.future_copies.forget(*local);
        }
        for witness in proof.completed {
            self.complete_future_witness(witness, None, state);
        }
        proof.returned
    }

    pub(super) fn observe_pending_tasks(&mut self, point: Point, state: &super::State) {
        let mut pending = FxHashSet::default();
        let mut unknown = Vec::new();
        for value in state
            .values
            .values()
            .chain(state.external.values())
            .chain(state.queues.values())
            .chain(state.tasks.values().map(|record| &record.callback))
        {
            for handle in value
                .futures
                .values()
                .flatten()
                .filter(|handle| handle.pending)
            {
                if state.tasks.contains_key(&handle.target) {
                    pending.insert(handle.target.clone());
                } else if !matches!(handle.target, Target::Validation { .. }) {
                    unknown.push(value.task_lineage());
                }
            }
        }
        for target in pending {
            let record = &state.tasks[&target];
            let mut record = record.clone();
            record.callback = record.callback.sourced(&super::demand::Storage::Task(
                target.clone(),
                record.callback_ty,
            ));
            let Some(inner) = value::child_type(target.ty(), &Slot::Element, self.table) else {
                continue;
            };
            let facts = self.task_facts(&record, point, state, inner);
            self.observe_prefix(&facts.prefix, point, state);
        }
        for reference in unknown {
            let mut accesses = super::Accesses::default();
            accesses.observe_reference(&reference, None, &[], super::AccessKind::Value);
            self.record_accesses(point, accesses, state);
        }
    }
}

impl Value {
    pub(super) fn future_result(&self) -> Self {
        let mut result = self.clone();
        result.leaves.retain(|(path, ..), _| *path != [Slot::Tail]);
        result.sources.retain(|(path, ..), _| *path != [Slot::Tail]);
        result.futures.retain(|path, _| *path != [Slot::Tail]);
        result.queues.retain(|path, _| *path != [Slot::Tail]);
        result.callees.retain(|path, _| *path != [Slot::Tail]);
        result.project(&Slot::Element)
    }

    pub(super) fn with_future_capture(&self, capture: Capture) -> Self {
        let mut result = self.clone();
        for (path, handles) in &mut result.futures {
            if path.is_empty() || *path == [Slot::Tail] {
                *handles = handles
                    .iter()
                    .map(|handle| Handle {
                        capture: Some(capture),
                        ..handle.clone()
                    })
                    .collect();
            }
        }
        result
    }
}

#[cfg(test)]
mod tests;

impl super::Flow<'_> {
    pub(super) fn import_tasks(
        &mut self,
        records: Vec<(Target, Record)>,
        point: Point,
        state: &mut super::State,
    ) {
        for (target, mut record) in records {
            self.record_path_store(
                point,
                super::demand::Coordinate {
                    storage: super::demand::Storage::Task(target.clone(), record.callback_ty),
                    path: Vec::new(),
                },
                &record.callback,
                false,
            );
            record.callback.sources.clear();
            if let Some(current) = state.tasks.get_mut(&target) {
                current.join(&record);
            } else {
                state.tasks.insert(target, record);
            }
        }
    }
}
