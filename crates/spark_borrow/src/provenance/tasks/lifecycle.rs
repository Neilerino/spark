// TAG [SPARK-114]

//! Current owner coverage separates direct execution references from descendant cleanup.

mod tokens;
pub(in crate::provenance) use tokens::{InputOwners, Tags, Token, Transfer};

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::{Constant, Local, MirFunction, Operand, Place, Rvalue, Statement, Terminator};
use spark_types::{Ty, TypeId, TypeTable};

use super::{Anchor, Copies, Record, Role, Witness};
use crate::provenance::{value, Flow, State};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::provenance) enum Coverage {
    Closed,
    /// All direct strong references are represented; descendant cleanup is unproved.
    Direct,
    ThroughQueue(std::sync::Arc<crate::provenance::aliases::Port>),
}

impl Coverage {
    pub fn meet(self, other: Self) -> Option<Self> {
        match (self, other) {
            (Self::Closed, coverage) | (coverage, Self::Closed) => Some(coverage),
            (Self::Direct, Self::Direct) => Some(Self::Direct),
            (Self::ThroughQueue(left), Self::ThroughQueue(right)) if left == right => {
                Some(Self::ThroughQueue(left))
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct Owners {
    coverage: FxHashMap<Role, Coverage>,
    exposed: FxHashSet<Anchor>,
}

impl Owners {
    pub fn known(&self, local: Local) -> bool {
        self.known_role(&Role::root(local))
    }
    pub fn known_role(&self, role: &Role) -> bool {
        self.coverage.get(role) == Some(&Coverage::Closed)
    }
    pub fn coverage(&self, role: &Role) -> Option<Coverage> {
        self.coverage.get(role).cloned()
    }
    pub fn group_coverage(&self, peers: &FxHashSet<Role>) -> Option<Coverage> {
        if peers.is_empty() {
            return None;
        }
        peers.iter().try_fold(Coverage::Closed, |current, peer| {
            current.meet(self.coverage(peer)?)
        })
    }
    pub fn exposed_role(&self, role: &Role) -> bool {
        self.exposed.contains(role.anchor())
    }
    pub fn grant(&mut self, local: Local, copies: &Copies) {
        self.grant_role(Role::root(local), copies);
    }
    pub fn grant_role(&mut self, role: Role, copies: &Copies) {
        self.install_role(role, Coverage::Closed, copies);
    }
    pub fn install_role(&mut self, role: Role, coverage: Coverage, copies: &Copies) {
        if self.exposed_role(&role) {
            self.invalidate_at(role.anchor().clone(), copies);
        } else {
            self.coverage.insert(role, coverage);
        }
    }
    pub fn forget(&mut self, local: Local) {
        self.coverage.retain(|role, _| role.local() != Some(local));
    }
    pub fn revoke(&mut self, witness: &Witness, copies: &Copies) {
        for role in &witness.peers {
            self.coverage.remove(role);
            for peer in copies.role_peers(role) {
                self.coverage.remove(&peer);
            }
        }
    }
    pub fn forget_execution(&mut self, local: Local, ty: TypeId) {
        self.coverage
            .retain(|role, _| role.local() != Some(local) || role.callback_type() != Some(ty));
    }
    pub fn forget_role(&mut self, role: &Role) {
        self.coverage.remove(role);
    }
    pub fn invalidate(&mut self, local: Local, copies: &Copies) {
        self.invalidate_at(Anchor::Local(local), copies);
    }
    fn invalidate_at(&mut self, anchor: Anchor, copies: &Copies) {
        let mut visited = FxHashSet::default();
        let mut pending = vec![anchor];
        while let Some(root) = pending.pop() {
            if !visited.insert(root.clone()) {
                continue;
            }
            for role in copies.anchored_at(&root) {
                for peer in copies.role_peers(&role) {
                    self.coverage.remove(&peer);
                    if let Some(anchor) = peer.as_anchor() {
                        pending.push(anchor.clone());
                    }
                }
            }
        }
    }
    pub fn expose(&mut self, local: Local, copies: &Copies) {
        self.exposed.insert(Anchor::Local(local));
        self.invalidate(local, copies);
    }
    pub fn intersect(&mut self, copies: &Copies, other: &Self, other_copies: &Copies) -> bool {
        let before = self.clone();
        self.coverage.retain(|role, coverage| {
            if copies.role_peers(role) != other_copies.role_peers(role) {
                return false;
            }
            let Some(next) = other
                .coverage(role)
                .and_then(|other| coverage.clone().meet(other))
            else {
                return false;
            };
            *coverage = next;
            true
        });
        self.exposed.extend(other.exposed.iter().cloned());
        self.coverage
            .retain(|role, _| !self.exposed.contains(role.anchor()));
        *self != before
    }
}

pub(in crate::provenance) struct CapturedOwner {
    pub(super) role: Role,
    pub(super) witness: Witness,
    pub(super) coverage: Option<Coverage>,
}

impl CapturedOwner {
    pub fn install_all(captures: Vec<Self>, state: &mut State) {
        Self::install_excluding(captures, &[], state);
    }

    pub fn install_excluding(mut captures: Vec<Self>, excluded: &[Local], state: &mut State) {
        let peers = crate::provenance::identity::transferred_peers(
            captures
                .iter()
                .map(|capture| (&capture.role, &capture.witness.peers)),
        );
        for (capture, peers) in captures.iter_mut().zip(peers) {
            capture.witness.exclude(excluded);
            if let Some(local) = capture.role.local() {
                capture.witness.exclude(&[local]);
            }
            capture.witness.peers.extend(peers);
        }
        for capture in captures {
            capture.install(state);
        }
    }

    fn install(mut self, state: &mut State) {
        if self.role.frame_clobbered(&[], &state.invalidated_copies)
            || self
                .witness
                .peers
                .iter()
                .any(|peer| peer.frame_clobbered(&[], &state.invalidated_copies))
        {
            self.witness.birth = None;
            self.witness.produces.clear();
            self.coverage = None;
        }
        state.future_owners.forget_role(&self.role);
        state.future_copies.assign_role(
            self.role.clone(),
            self.witness,
            &crate::provenance::tasks::Live::new(&state.live, &state.queue_current),
        );
        if let Some(coverage) = self.coverage {
            state
                .future_owners
                .install_role(self.role, coverage, &state.future_copies);
        }
    }
}

pub(in crate::provenance) fn may_own_future(
    ty: TypeId,
    table: &TypeTable,
    seen: &mut FxHashSet<TypeId>,
) -> bool {
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
        Ty::Map(key, value) => {
            may_own_future(*key, table, seen) || may_own_future(*value, table, seen)
        }
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
        self.environment_source(source, destination)
    }

    fn environment_source(&self, source: &Place, destination: &Place) -> Option<Local> {
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
                        coverage: state.future_owners.coverage(role),
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
                    coverage: state.future_owners.coverage(&Role::root(source)),
                })
            })
            .collect()
    }

    pub(in crate::provenance) fn owning_rvalue(
        &self,
        rvalue: &Rvalue,
        destination: &Place,
        state: &mut State,
    ) -> Option<Coverage> {
        if self.moving_environment(rvalue, destination).is_some() {
            return None;
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
                    return state.future_owners.coverage(&Role::root(source.local));
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
        None
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
    #[test]
    fn an_opaque_outer_capture_revokes_nested_execution_owner_coverage() {
        use spark_resolve::DefId;
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(spark_types::IntWidth::I64));
        let future = table.intern(Ty::Future { inner: int });
        let callback = table.intern(Ty::Function {
            params: vec![],
            ret: int,
            is_async: false,
        });
        let mut copies = Copies::default();
        let live = FxHashSet::from_iter([Local(1), Local(2), Local(3)]);
        let parent = Role::capture(Local(3), DefId::new(9980), 0, future);
        copies.assign_role(parent, copies.witness(Local(1)), &live);
        let child = Role::capture(Local(1), DefId::new(9981), 0, future)
            .in_execution(Local(1), callback)
            .unwrap();
        copies.assign_role(child.clone(), copies.witness(Local(2)), &live);
        let mut owners = Owners::default();
        owners.grant(Local(2), &copies);
        owners.grant_role(child.clone(), &copies);
        owners.invalidate(Local(3), &copies);
        assert!(!owners.known(Local(2)));
        assert!(!owners.known_role(&child));
    }
}
