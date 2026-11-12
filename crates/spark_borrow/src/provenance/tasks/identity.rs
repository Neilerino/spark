// TAG [SPARK-114]

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::Local;
use spark_resolve::DefId;
use spark_types::TypeId;

use crate::provenance::{identity::Classes, Path, Slot};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) struct Role {
    local: Local,
    path: Path,
    callback_ty: Option<TypeId>,
}

impl Role {
    pub const fn root(local: Local) -> Self {
        Self {
            local,
            path: Vec::new(),
            callback_ty: None,
        }
    }

    pub fn capture(local: Local, owner: DefId, index: usize, ty: TypeId) -> Self {
        Self {
            local,
            path: vec![Slot::Capture(owner, index, ty)],
            callback_ty: None,
        }
    }

    pub fn reanchor(&self, local: Local) -> Self {
        Self {
            local,
            path: self.path.clone(),
            callback_ty: self.callback_ty,
        }
    }

    pub fn capture_at(&self, local: Local) -> Option<Self> {
        matches!(self.path.as_slice(), [Slot::Capture(..)]).then(|| Self {
            local,
            path: self.path.clone(),
            callback_ty: self.callback_ty,
        })
    }

    pub fn selection(&self) -> &[Slot] {
        &self.path
    }

    pub fn in_execution(&self, local: Local, callback_ty: TypeId) -> Option<Self> {
        let mut role = self.capture_at(local)?;
        role.callback_ty = Some(callback_ty);
        Some(role)
    }

    pub const fn callback_type(&self) -> Option<TypeId> {
        self.callback_ty
    }

    pub const fn local(&self) -> Local {
        self.local
    }

    pub fn as_local(&self) -> Option<Local> {
        (self.callback_ty.is_none() && self.path.is_empty()).then_some(self.local)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct Copies {
    classes: Classes<Role>,
    inputs: FxHashMap<Role, FxHashSet<Local>>,
}

pub(in crate::provenance) struct Witness {
    pub peers: FxHashSet<Role>,
    pub inputs: FxHashSet<Local>,
}

impl Witness {
    pub fn exclude(&mut self, clobbered: &[Local]) {
        self.peers.retain(|role| !clobbered.contains(&role.local));
    }

    pub fn locals(&self) -> impl Iterator<Item = Local> + '_ {
        self.peers.iter().filter_map(Role::as_local)
    }

    pub fn contains_local(&self, local: Local) -> bool {
        self.peers.contains(&Role::root(local))
    }
}

impl Copies {
    pub fn witness(&self, source: Local) -> Witness {
        self.role_witness(&Role::root(source))
    }

    pub fn role_witness(&self, source: &Role) -> Witness {
        Witness {
            peers: self.role_peers(source),
            inputs: self.inputs.get(source).cloned().unwrap_or_default(),
        }
    }

    pub fn seed_input(&mut self, input: Local) {
        let role = Role::root(input);
        self.classes.insert(role.clone());
        self.inputs.insert(role, FxHashSet::from_iter([input]));
    }

    pub fn inputs(&self, local: Local) -> FxHashSet<Local> {
        self.inputs
            .get(&Role::root(local))
            .cloned()
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn peers(&self, source: Local) -> FxHashSet<Local> {
        self.role_peers(&Role::root(source))
            .iter()
            .filter_map(Role::as_local)
            .collect()
    }

    pub fn role_peers(&self, source: &Role) -> FxHashSet<Role> {
        self.classes.peers(source)
    }

    pub fn anchored(&self, local: Local) -> FxHashSet<Role> {
        self.classes
            .roles()
            .filter(|role| role.local == local)
            .cloned()
            .chain(std::iter::once(Role::root(local)))
            .collect()
    }

    pub fn forget(&mut self, local: Local) {
        self.classes.retain(|role| role.local != local);
        self.inputs.retain(|role, _| role.local != local);
    }

    pub fn forget_role(&mut self, role: &Role) {
        self.classes.retain(|other| other != role);
        self.inputs.remove(role);
    }

    pub fn forget_execution(&mut self, local: Local, ty: TypeId) {
        let roles: Vec<_> = self
            .classes
            .roles()
            .filter(|role| role.local == local && role.callback_ty == Some(ty))
            .cloned()
            .collect();
        for role in roles {
            self.forget_role(&role);
        }
    }

    pub fn assign(&mut self, destination: Local, witness: Witness, live: &FxHashSet<Local>) {
        self.forget(destination);
        self.assign_role(Role::root(destination), witness, live);
    }

    pub fn assign_role(&mut self, destination: Role, witness: Witness, live: &FxHashSet<Local>) {
        self.inputs.remove(&destination);
        let peers = witness
            .peers
            .into_iter()
            .filter(|peer| *peer != destination && live.contains(&peer.local))
            .collect();
        if !witness.inputs.is_empty() {
            self.inputs.insert(destination.clone(), witness.inputs);
        }
        self.classes.assign(destination, peers);
    }

    pub fn intersect(&mut self, other: &Self) -> bool {
        let mut changed = self.classes.intersect(&other.classes);
        self.inputs.retain(|role, inputs| {
            let before = inputs.len();
            inputs.retain(|input| {
                other
                    .inputs
                    .get(role)
                    .is_some_and(|right| right.contains(input))
            });
            changed |= before != inputs.len();
            !inputs.is_empty()
        });
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn live() -> FxHashSet<Local> {
        (1..=5).map(Local).collect()
    }

    #[test]
    fn replacing_a_current_coordinate_preserves_its_old_peers() {
        let mut copies = Copies::default();
        copies.assign(Local(2), copies.witness(Local(1)), &live());
        copies.assign(Local(3), copies.witness(Local(2)), &live());
        copies.forget(Local(2));
        copies.assign(Local(4), copies.witness(Local(2)), &live());
        assert_eq!(
            copies.peers(Local(1)),
            FxHashSet::from_iter([Local(1), Local(3)])
        );
        assert_eq!(
            copies.peers(Local(2)),
            FxHashSet::from_iter([Local(2), Local(4)])
        );
    }

    #[test]
    fn a_move_keeps_a_snapshot_of_surviving_copy_peers() {
        let mut copies = Copies::default();
        copies.assign(Local(2), copies.witness(Local(1)), &live());
        let witness = copies.witness(Local(1));
        copies.forget(Local(1));
        let mut survivors = live();
        survivors.remove(&Local(1));
        copies.assign(Local(3), witness, &survivors);
        assert_eq!(
            copies.peers(Local(2)),
            FxHashSet::from_iter([Local(2), Local(3)])
        );
        assert_eq!(copies.peers(Local(1)), FxHashSet::from_iter([Local(1)]));
    }

    #[test]
    fn branch_meet_retains_only_equality_proved_on_both_paths() {
        let mut left = Copies::default();
        left.assign(Local(2), left.witness(Local(1)), &live());
        left.assign(Local(3), left.witness(Local(2)), &live());
        let mut right = Copies::default();
        right.assign(Local(3), right.witness(Local(2)), &live());
        assert!(left.intersect(&right));
        assert_eq!(left.peers(Local(1)), FxHashSet::from_iter([Local(1)]));
        assert_eq!(
            left.peers(Local(2)),
            FxHashSet::from_iter([Local(2), Local(3)])
        );
        assert!(!left.intersect(&right));
    }

    #[test]
    fn moved_input_identity_survives_replacing_its_old_coordinate() {
        let mut copies = Copies::default();
        let live = FxHashSet::from_iter([Local(1), Local(2), Local(3)]);
        copies.seed_input(Local(1));
        let witness = copies.witness(Local(1));
        copies.forget(Local(1));
        copies.assign(Local(2), witness, &live);
        copies.assign(Local(1), copies.witness(Local(3)), &live);
        assert_eq!(copies.inputs(Local(2)), FxHashSet::from_iter([Local(1)]));
        assert!(copies.inputs(Local(1)).is_empty());
        let mut alternative = copies.clone();
        alternative.forget(Local(2));
        assert!(copies.intersect(&alternative));
        assert!(copies.inputs(Local(2)).is_empty());
    }
}
