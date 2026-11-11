// TAG [SPARK-114]

//! Cleanup rights cover current owner roles, never an allocation-site singleton.

mod tokens;
pub(in crate::provenance) use tokens::{InputOwners, Transfer};

use rustc_hash::FxHashSet;
use spark_mir::{Constant, Local, MirFunction, Operand, Place, Rvalue, Statement, Terminator};
use spark_types::{Ty, TypeId, TypeTable};

use super::{Copies, Record, Role, Witness};
use crate::provenance::{value, Flow, State};

#[derive(Clone, Debug, Default)]
pub(in crate::provenance) struct Owners {
    closed: FxHashSet<Role>,
    exposed: FxHashSet<Local>,
}

impl Owners {
    pub fn known(&self, local: Local) -> bool {
        self.known_role(&Role::root(local))
    }
    pub fn known_role(&self, role: &Role) -> bool {
        self.closed.contains(role)
    }
    pub fn grant(&mut self, local: Local, copies: &Copies) {
        self.grant_role(Role::root(local), copies);
    }
    pub fn grant_role(&mut self, role: Role, copies: &Copies) {
        if self.exposed.contains(&role.local()) {
            self.invalidate(role.local(), copies);
        } else {
            self.closed.insert(role);
        }
    }
    pub fn forget(&mut self, local: Local) {
        self.closed.retain(|role| role.local() != local);
    }
    pub fn invalidate(&mut self, local: Local, copies: &Copies) {
        for role in copies.anchored(local) {
            for peer in copies.role_peers(&role) {
                self.closed.remove(&peer);
            }
        }
    }
    pub fn expose(&mut self, local: Local, copies: &Copies) {
        self.exposed.insert(local);
        self.invalidate(local, copies);
    }
    pub fn intersect(&mut self, copies: &Copies, other: &Self, other_copies: &Copies) -> bool {
        let before = self.closed.clone();
        self.closed.retain(|role| {
            other.closed.contains(role) && copies.role_peers(role) == other_copies.role_peers(role)
        });
        let before_exposed = self.exposed.len();
        self.exposed.extend(other.exposed.iter().copied());
        self.closed
            .retain(|role| !self.exposed.contains(&role.local()));
        self.closed != before || before_exposed != self.exposed.len()
    }
}

pub(in crate::provenance) struct CapturedOwner {
    pub(super) role: Role,
    pub(super) witness: Witness,
    pub(super) closed: bool,
}

impl CapturedOwner {
    pub fn install_all(captures: Vec<Self>, state: &mut State) {
        Self::install_excluding(captures, &[], state);
    }

    pub fn install_excluding(mut captures: Vec<Self>, excluded: &[Local], state: &mut State) {
        let peers: Vec<Vec<Role>> = captures
            .iter()
            .map(|capture| {
                captures
                    .iter()
                    .filter(|other| !capture.witness.peers.is_disjoint(&other.witness.peers))
                    .map(|other| other.role.clone())
                    .collect()
            })
            .collect();
        for (capture, peers) in captures.iter_mut().zip(peers) {
            capture.witness.exclude(excluded);
            capture.witness.exclude(&[capture.role.local()]);
            capture.witness.peers.extend(peers);
        }
        for capture in captures {
            capture.install(state);
        }
    }

    fn install(self, state: &mut State) {
        state
            .future_copies
            .assign_role(self.role.clone(), self.witness, &state.live);
        if self.closed {
            state
                .future_owners
                .grant_role(self.role, &state.future_copies);
        }
    }
}

fn may_own_future(ty: TypeId, table: &TypeTable, seen: &mut FxHashSet<TypeId>) -> bool {
    let ty = value::strip_mutable(ty, table);
    if !seen.insert(ty) {
        return false;
    }
    match table.resolve(ty) {
        Ty::Future { .. }
        | Ty::Function { .. }
        | Ty::Param(_)
        | Ty::AsyncState { .. }
        | Ty::Error => true,
        Ty::Ref { inner, .. }
        | Ty::Channel { inner }
        | Ty::List(inner)
        | Ty::Set(inner)
        | Ty::Optional(inner) => may_own_future(*inner, table, seen),
        _ => value::inline_slots(ty, table)
            .into_iter()
            .any(|(_, ty)| may_own_future(ty, table, seen)),
    }
}

// TAG [SPARK-151]
fn plain_function_reference(function: &MirFunction, local: Local) -> Option<spark_resolve::DefId> {
    let declaration = &function.locals[local.index()];
    if declaration.is_user_variable || declaration.mutable || declaration.name.is_some() {
        return None;
    }
    let mut target = None;
    for block in &function.blocks {
        if matches!(&block.terminator, Terminator::Call { destination, .. } if destination.local == local)
        {
            return None;
        }
        for statement in &block.statements {
            if let Statement::Assign { place, rvalue, .. } = statement {
                if place.local != local {
                    continue;
                }
                if target.is_some() || !place.projections.is_empty() {
                    return None;
                }
                let Rvalue::Use(Operand::Constant(Constant::FunctionRef(def))) = rvalue else {
                    return None;
                };
                target = Some(*def);
            }
        }
    }
    target
}

impl Flow<'_> {
    pub(in crate::provenance) fn leaf_task(&self, record: &Record) -> bool {
        if !record.callback.futures.is_empty() {
            return false;
        }
        let Some(context) = self.program else {
            return false;
        };
        let targets = self.callee_targets(&record.callback, record.callback_ty);
        if targets.open || targets.unresolved || targets.known.is_empty() {
            return false;
        }
        let mut pending: Vec<_> = targets.known.into_iter().collect();
        let mut seen = FxHashSet::default();
        while let Some(def) = pending.pop() {
            if !seen.insert(def) {
                continue;
            }
            if let Some(function) = context.mir.functions.get(&def) {
                for local in &function.locals {
                    if matches!(
                        self.table
                            .resolve(value::strip_mutable(local.ty, self.table)),
                        Ty::Function { .. }
                    ) {
                        let Some(callee) = plain_function_reference(function, local.local) else {
                            return false;
                        };
                        pending.push(callee);
                    } else if may_own_future(local.ty, self.table, &mut FxHashSet::default()) {
                        return false;
                    }
                }
                for block in &function.blocks {
                    if let Terminator::Call { func, .. } = &block.terminator {
                        let Some(callee) = function.resolve_callee_def_id(func) else {
                            return false;
                        };
                        pending.push(callee);
                    }
                }
            } else if !context
                .definitions
                .and_then(|definitions| definitions.get(def))
                .is_some_and(|info| {
                    use spark_intern::kw;
                    use spark_resolve::DefinitionKind;
                    match info.kind {
                        DefinitionKind::BuiltinFunction => {
                            info.name != kw::SPARK_SPAWN && info.name != kw::SPARK_AWAIT
                        }
                        DefinitionKind::BuiltinMethod => matches!(info.name, kw::SEND | kw::RECV),
                        _ => false,
                    }
                })
            {
                return false;
            }
        }
        true
    }

    fn captured_future(&self, operand: &Operand) -> Option<(Local, TypeId)> {
        let (Operand::Copy(place) | Operand::Move(place)) = operand else {
            return None;
        };
        if !place.projections.is_empty() {
            return None;
        }
        let ty = self.place_type(place);
        matches!(
            self.table.resolve(value::strip_mutable(ty, self.table)),
            Ty::Future { .. }
        )
        .then_some((place.local, ty))
    }

    fn moving_environment(&self, rvalue: &Rvalue, destination: &Place) -> Option<Local> {
        let Rvalue::Use(Operand::Move(source)) = rvalue else {
            return None;
        };
        if !source.projections.is_empty() || !destination.projections.is_empty() {
            return None;
        }
        let source_ty = value::strip_mutable(self.place_type(source), self.table);
        let destination_ty = value::strip_mutable(self.place_type(destination), self.table);
        (source_ty == destination_ty
            && matches!(self.table.resolve(source_ty), Ty::Function { .. }))
        .then_some(source.local)
    }

    pub(in crate::provenance) fn captured_future_owners(
        &self,
        rvalue: &Rvalue,
        destination: &Place,
        state: &State,
    ) -> Vec<CapturedOwner> {
        if let Some(source) = self.moving_environment(rvalue, destination) {
            return state
                .future_copies
                .anchored(source)
                .iter()
                .filter_map(|role| {
                    Some(CapturedOwner {
                        role: role.capture_at(destination.local)?,
                        witness: state.future_copies.role_witness(role),
                        closed: state.future_owners.known_role(role),
                    })
                })
                .collect();
        }
        let Rvalue::Aggregate(spark_mir::AggregateKind::Closure { func_def_id }, operands) = rvalue
        else {
            return Vec::new();
        };
        if !destination.projections.is_empty() {
            return Vec::new();
        }
        operands
            .iter()
            .enumerate()
            .filter_map(|(index, operand)| {
                let (source, ty) = self.captured_future(operand)?;
                state.live.contains(&source).then(|| CapturedOwner {
                    role: Role::capture(destination.local, *func_def_id, index, ty),
                    witness: state.future_copies.witness(source),
                    closed: state.future_owners.known(source),
                })
            })
            .collect()
    }

    pub(in crate::provenance) fn owning_rvalue(
        &self,
        rvalue: &Rvalue,
        destination: &Place,
        state: &mut State,
    ) -> bool {
        if self.moving_environment(rvalue, destination).is_some() {
            return false;
        }
        let root_destination = destination.projections.is_empty()
            && matches!(
                self.table.resolve(value::strip_mutable(
                    self.place_type(destination),
                    self.table
                )),
                Ty::Future { .. }
            );
        if root_destination {
            if let Rvalue::Use(Operand::Copy(source) | Operand::Move(source)) = rvalue {
                if source.projections.is_empty() {
                    return state.future_owners.known(source.local);
                }
            }
        }
        match rvalue {
            Rvalue::Ref { place, .. } => state
                .future_owners
                .expose(place.local, &state.future_copies),
            Rvalue::Use(operand) | Rvalue::Cast { operand, .. } | Rvalue::UnaryOp(_, operand) => {
                Self::lose_owning_operand(operand, state);
            }
            Rvalue::Aggregate(spark_mir::AggregateKind::Closure { .. }, operands)
                if destination.projections.is_empty() =>
            {
                for operand in operands {
                    if self.captured_future(operand).is_none() {
                        Self::lose_owning_operand(operand, state);
                    }
                }
            }
            Rvalue::Aggregate(_, operands) | Rvalue::StringInterp(operands) => {
                for operand in operands {
                    Self::lose_owning_operand(operand, state);
                }
            }
            _ => {}
        }
        false
    }

    pub(in crate::provenance) fn lose_owning_operand(operand: &Operand, state: &mut State) {
        if let Operand::Copy(place) | Operand::Move(place) = operand {
            state
                .future_owners
                .invalidate(place.local, &state.future_copies);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_callable_signature_does_not_prove_its_owned_environment_is_future_free() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(spark_types::IntWidth::I64));
        let callback = table.intern(Ty::Function {
            params: vec![int],
            ret: int,
            is_async: false,
        });
        assert!(!may_own_future(int, &table, &mut FxHashSet::default()));
        assert!(
            may_own_future(callback, &table, &mut FxHashSet::default()),
            "an opaque closure can own a Future without exposing it in its signature"
        );
    }

    fn copies() -> Copies {
        let mut copies = Copies::default();
        copies.assign(
            Local(2),
            copies.witness(Local(1)),
            &FxHashSet::from_iter([Local(1), Local(2)]),
        );
        copies
    }

    #[test]
    fn an_owned_copy_survives_the_other_current_owner_being_consumed() {
        let mut copies = copies();
        let mut owners = Owners::default();
        owners.grant(Local(1), &copies);
        owners.grant(Local(2), &copies);
        owners.forget(Local(1));
        copies.forget(Local(1));
        assert!(owners.known(Local(2)));
        assert!(!owners.known(Local(1)));
    }

    #[test]
    fn a_join_cannot_close_an_owner_group_when_its_alias_members_disagree() {
        let left = copies();
        let mut right = left.clone();
        right.forget(Local(2));
        let mut owners = Owners::default();
        for local in [Local(1), Local(2)] {
            owners.grant(local, &left);
        }
        let other = owners.clone();
        assert!(owners.intersect(&left, &other, &right));
        assert!(!owners.known(Local(1)));
        assert!(!owners.known(Local(2)));
    }

    #[test]
    fn exposing_one_alias_revokes_cleanup_rights_for_the_entire_group() {
        let copies = copies();
        let mut owners = Owners::default();
        owners.grant(Local(1), &copies);
        owners.grant(Local(2), &copies);
        owners.expose(Local(1), &copies);
        assert!(!owners.known(Local(1)));
        assert!(!owners.known(Local(2)));
        owners.grant(Local(1), &copies);
        assert!(!owners.known(Local(1)));
    }

    #[test]
    fn later_peer_loss_does_not_restore_revoked_cleanup_rights() {
        let mut copies = copies();
        let mut owners = Owners::default();
        owners.grant(Local(1), &copies);
        owners.grant(Local(2), &copies);
        owners.invalidate(Local(1), &copies);
        owners.forget(Local(1));
        copies.forget(Local(1));
        assert!(!owners.known(Local(2)));
    }
}
