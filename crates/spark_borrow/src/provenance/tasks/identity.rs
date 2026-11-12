// TAG [SPARK-114]

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::Local;
use spark_resolve::DefId;
use spark_types::TypeId;

use crate::provenance::{identity::Classes, Path, Slot};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) enum Anchor {
    Local(Local),
    Message(crate::provenance::channels::messages::Slot),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) struct Role {
    anchor: Anchor,
    path: Path,
    callback_ty: Option<TypeId>,
}

impl Role {
    pub const fn root(local: Local) -> Self {
        Self {
            anchor: Anchor::Local(local),
            path: Vec::new(),
            callback_ty: None,
        }
    }
    pub const fn message(slot: crate::provenance::channels::messages::Slot) -> Self {
        Self {
            anchor: Anchor::Message(slot),
            path: Vec::new(),
            callback_ty: None,
        }
    }
    pub fn capture(local: Local, owner: DefId, index: usize, ty: TypeId) -> Self {
        Self {
            anchor: Anchor::Local(local),
            path: vec![Slot::Capture(owner, index, ty)],
            callback_ty: None,
        }
    }
    pub fn reanchor(&self, local: Local) -> Self {
        Self {
            anchor: Anchor::Local(local),
            path: self.path.clone(),
            callback_ty: self.callback_ty,
        }
    }
    pub fn capture_at(&self, local: Local) -> Option<Self> {
        matches!(self.path.as_slice(), [Slot::Capture(..)]).then(|| self.reanchor(local))
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
    pub const fn anchor(&self) -> &Anchor {
        &self.anchor
    }
    pub const fn local(&self) -> Option<Local> {
        match self.anchor {
            Anchor::Local(local) => Some(local),
            Anchor::Message(_) => None,
        }
    }
    pub fn as_anchor(&self) -> Option<&Anchor> {
        (self.callback_ty.is_none() && self.path.is_empty()).then_some(&self.anchor)
    }
    pub fn as_local(&self) -> Option<Local> {
        self.as_anchor().and_then(|_| self.local())
    }
    pub fn frame_clobbered(&self, clobbered: &[Local], invalidated: &FxHashSet<Local>) -> bool {
        self.local()
            .is_some_and(|local| clobbered.contains(&local) || invalidated.contains(&local))
    }
    pub fn storage(&self) -> crate::provenance::demand::Storage {
        use crate::provenance::demand::Storage;
        match &self.anchor {
            Anchor::Message(slot) => Storage::QueueCurrent(slot.clone()),
            Anchor::Local(local) => self.callback_ty.map_or(Storage::Logical(*local), |ty| {
                Storage::ExecutionCallback(*local, ty)
            }),
        }
    }
}

pub(in crate::provenance) trait RoleLiveness {
    fn contains_role(&self, role: &Role) -> bool;
}

pub(in crate::provenance) struct Live<'a> {
    locals: &'a FxHashSet<Local>,
    messages: &'a FxHashMap<
        crate::provenance::channels::messages::Slot,
        crate::provenance::channels::messages::Contribution,
    >,
}

impl<'a> Live<'a> {
    pub const fn new(
        locals: &'a FxHashSet<Local>,
        messages: &'a FxHashMap<
            crate::provenance::channels::messages::Slot,
            crate::provenance::channels::messages::Contribution,
        >,
    ) -> Self {
        Self { locals, messages }
    }
    pub fn contains(&self, role: &Role) -> bool {
        match role.anchor() {
            Anchor::Local(local) => self.locals.contains(local),
            Anchor::Message(slot) => self.messages.contains_key(slot),
        }
    }
}
impl RoleLiveness for Live<'_> {
    fn contains_role(&self, role: &Role) -> bool {
        self.contains(role)
    }
}
#[cfg(test)]
impl RoleLiveness for FxHashSet<Local> {
    fn contains_role(&self, role: &Role) -> bool {
        role.local().is_some_and(|local| self.contains(&local))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct Copies {
    classes: Classes<Role>,
    inputs: FxHashMap<Role, FxHashSet<Local>>,
    births: FxHashMap<Role, super::fresh::Birth>,
}

pub(in crate::provenance) struct Witness {
    pub peers: FxHashSet<Role>,
    pub inputs: FxHashSet<Local>,
    pub birth: Option<super::fresh::Birth>,
}

impl Witness {
    pub fn exclude(&mut self, clobbered: &[Local]) {
        if self
            .peers
            .iter()
            .any(|role| role.local().is_some_and(|local| clobbered.contains(&local)))
        {
            self.birth = None;
        }
        self.peers
            .retain(|role| !role.local().is_some_and(|local| clobbered.contains(&local)));
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
            birth: self.births.get(source).copied(),
        }
    }

    pub fn seed_birth(&mut self, local: Local, birth: super::fresh::Birth) {
        self.forget(local);
        let role = Role::root(local);
        self.classes.insert(role.clone());
        self.births.insert(role, birth);
    }

    pub fn seed_input(&mut self, input: Local) {
        let role = Role::root(input);
        self.classes.insert(role.clone());
        self.births.remove(&role);
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
        self.anchored_at(&Anchor::Local(local))
    }
    pub fn anchored_at(&self, anchor: &Anchor) -> FxHashSet<Role> {
        self.classes
            .roles()
            .filter(|role| role.anchor() == anchor)
            .cloned()
            .chain(std::iter::once(Role {
                anchor: anchor.clone(),
                path: Vec::new(),
                callback_ty: None,
            }))
            .collect()
    }

    pub fn forget(&mut self, local: Local) {
        self.classes.retain(|role| role.local() != Some(local));
        self.inputs.retain(|role, _| role.local() != Some(local));
        self.births.retain(|role, _| role.local() != Some(local));
    }

    pub fn forget_role(&mut self, role: &Role) {
        self.classes.retain(|other| other != role);
        self.inputs.remove(role);
        self.births.remove(role);
    }

    pub fn forget_execution(&mut self, local: Local, ty: TypeId) {
        let roles: Vec<_> = self
            .classes
            .roles()
            .filter(|role| role.local() == Some(local) && role.callback_ty == Some(ty))
            .cloned()
            .collect();
        for role in roles {
            self.forget_role(&role);
        }
    }

    pub fn assign(&mut self, destination: Local, witness: Witness, live: &impl RoleLiveness) {
        self.forget(destination);
        self.assign_role(Role::root(destination), witness, live);
    }

    pub fn assign_role(&mut self, destination: Role, witness: Witness, live: &impl RoleLiveness) {
        self.inputs.remove(&destination);
        self.births.remove(&destination);
        if let Some(birth) = witness.birth {
            self.births.insert(destination.clone(), birth);
        }
        let peers = witness
            .peers
            .into_iter()
            .filter(|peer| *peer != destination && live.contains_role(peer))
            .collect();
        if !witness.inputs.is_empty() {
            self.inputs.insert(destination.clone(), witness.inputs);
        }
        self.classes.assign(destination, peers);
    }

    pub fn intersect(&mut self, other: &Self) -> bool {
        let mut changed = self.classes.intersect(&other.classes);
        let before = self.births.len();
        self.births
            .retain(|role, birth| other.births.get(role) == Some(birth));
        changed |= before != self.births.len();
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
