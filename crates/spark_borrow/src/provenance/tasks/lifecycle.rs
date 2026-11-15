// TAG [SPARK-114]

//! Cleanup rights describe current owning locals, never an allocation-site singleton.

use rustc_hash::FxHashSet;
use spark_mir::{Local, Operand, Place, Rvalue, Terminator};
use spark_types::{Ty, TypeId, TypeTable};

use super::{Copies, Record};
use crate::provenance::{value, Flow, State};

#[derive(Clone, Debug, Default)]
pub(in crate::provenance) struct Owners {
    closed: FxHashSet<Local>,
    exposed: FxHashSet<Local>,
}

impl Owners {
    pub fn known(&self, local: Local) -> bool {
        self.closed.contains(&local)
    }
    pub fn grant(&mut self, local: Local, copies: &Copies) {
        if self.exposed.contains(&local) {
            self.invalidate(local, copies);
        } else {
            self.closed.insert(local);
        }
    }
    pub fn forget(&mut self, local: Local) {
        self.closed.remove(&local);
    }
    pub fn invalidate(&mut self, local: Local, copies: &Copies) {
        for peer in copies.peers(local) {
            self.closed.remove(&peer);
        }
    }
    pub fn expose(&mut self, local: Local, copies: &Copies) {
        self.exposed.insert(local);
        self.invalidate(local, copies);
    }
    pub fn intersect(&mut self, copies: &Copies, other: &Self, other_copies: &Copies) -> bool {
        let before = self.closed.clone();
        self.closed.retain(|local| {
            other.closed.contains(local) && copies.peers(*local) == other_copies.peers(*local)
        });
        let before_exposed = self.exposed.len();
        self.exposed.extend(other.exposed.iter().copied());
        self.closed.retain(|local| !self.exposed.contains(local));
        self.closed != before || before_exposed != self.exposed.len()
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
                if function
                    .locals
                    .iter()
                    .any(|local| may_own_future(local.ty, self.table, &mut FxHashSet::default()))
                {
                    return false;
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
                    matches!(info.kind, spark_resolve::DefinitionKind::BuiltinFunction)
                        && info.name != spark_intern::kw::SPARK_SPAWN
                        && info.name != spark_intern::kw::SPARK_AWAIT
                })
            {
                return false;
            }
        }
        true
    }

    pub(in crate::provenance) fn owning_rvalue(
        &self,
        rvalue: &Rvalue,
        destination: &Place,
        state: &mut State,
    ) -> bool {
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
