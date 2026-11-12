// TAG [SPARK-114]

//! Freshness is construction provenance, not a proof of closed queue access or message ownership.

mod transfer;
pub(in crate::provenance) use transfer::Captured;

use crate::provenance::identity::Classes;
use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::Local;
use spark_types::TypeId;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) enum Role {
    Local(Local),
    Capture {
        local: Local,
        capture: super::Capture,
        callback_ty: Option<TypeId>,
    },
}

impl Role {
    pub const fn local(&self) -> Local {
        match self {
            Self::Local(local) | Self::Capture { local, .. } => *local,
        }
    }

    pub const fn callback_type(&self) -> Option<TypeId> {
        match self {
            Self::Local(_) => None,
            Self::Capture { callback_ty, .. } => *callback_ty,
        }
    }

    pub const fn reanchor(&self, local: Local) -> Self {
        match self {
            Self::Local(_) => Self::Local(local),
            Self::Capture {
                capture,
                callback_ty,
                ..
            } => Self::Capture {
                local,
                capture: *capture,
                callback_ty: *callback_ty,
            },
        }
    }

    pub const fn in_execution(&self, local: Local, ty: TypeId) -> Option<Self> {
        match self {
            Self::Local(_) => None,
            Self::Capture { capture, .. } => Some(Self::Capture {
                local,
                capture: *capture,
                callback_ty: Some(ty),
            }),
        }
    }
}

#[derive(Clone, Debug)]
pub(in crate::provenance) struct Witness {
    pub peers: FxHashSet<Role>,
    pub fresh: bool,
    pub output: Option<super::messages::Slot>,
    pub input: Option<std::sync::Arc<crate::provenance::aliases::Port>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct Copies {
    classes: Classes<Role>,
    fresh: FxHashSet<Role>,
    outputs: FxHashMap<Role, super::messages::Slot>,
    inputs: FxHashMap<Role, std::sync::Arc<crate::provenance::aliases::Port>>,
}

impl Copies {
    pub fn witness(&self, source: &Role) -> Witness {
        Witness {
            peers: self.classes.peers(source),
            fresh: self.fresh.contains(source),
            output: self.outputs.get(source).cloned(),
            input: self.inputs.get(source).cloned(),
        }
    }

    pub fn anchored(&self, local: Local) -> impl Iterator<Item = &Role> {
        self.classes
            .roles()
            .filter(move |role| role.local() == local)
    }

    pub fn seed(&mut self, local: Local) {
        self.forget(local);
        let role = Role::Local(local);
        self.classes.assign(role.clone(), FxHashSet::default());
        self.fresh.insert(role);
    }

    pub fn seed_input(&mut self, local: Local, port: crate::provenance::aliases::Port) {
        self.forget(local);
        let peers = self
            .inputs
            .iter()
            .filter(|(_, found)| ***found == port)
            .map(|(role, _)| role.clone())
            .collect();
        let role = Role::Local(local);
        self.classes.assign(role.clone(), peers);
        self.inputs.insert(role, std::sync::Arc::new(port));
    }

    fn retain(&mut self, keep: impl Fn(&Role) -> bool) {
        self.classes.retain(&keep);
        self.fresh.retain(&keep);
        self.inputs.retain(|role, _| keep(role));
        self.outputs.retain(|role, _| keep(role));
    }

    pub fn forget(&mut self, local: Local) {
        self.retain(|role| role.local() != local);
    }

    pub fn forget_execution(&mut self, local: Local, ty: TypeId) {
        self.retain(|role| role.local() != local || role.callback_type() != Some(ty));
    }

    pub fn assign_role(&mut self, destination: Role, witness: Witness, live: &FxHashSet<Local>) {
        let peers = witness
            .peers
            .into_iter()
            .filter(|peer| *peer != destination && live.contains(&peer.local()))
            .collect();
        self.fresh.remove(&destination);
        self.inputs.remove(&destination);
        self.outputs.remove(&destination);
        if let Some(output) = witness.output {
            self.outputs.insert(destination.clone(), output);
        }
        if let Some(input) = witness.input {
            self.inputs.insert(destination.clone(), input);
        }
        if witness.fresh {
            self.fresh.insert(destination.clone());
        }
        self.classes.assign(destination, peers);
    }

    pub fn reserve_output(&mut self, role: &Role, output: &super::messages::Slot) {
        for peer in self.classes.peers(role) {
            self.outputs.insert(peer, output.clone());
        }
    }

    pub fn output_roles(&self, output: &super::messages::Slot) -> FxHashSet<Role> {
        self.outputs
            .iter()
            .filter(|(_, found)| *found == output)
            .map(|(role, _)| role.clone())
            .collect()
    }

    pub fn forget_output(&mut self, output: &super::messages::Slot) {
        self.outputs.retain(|_, found| found != output);
    }

    pub fn intersect(&mut self, other: &Self) -> bool {
        let mut changed = self.classes.intersect(&other.classes);
        let before = self.fresh.len();
        self.fresh.retain(|role| other.fresh.contains(role));
        changed |= before != self.fresh.len();
        let before = self.inputs.len();
        self.inputs
            .retain(|role, input| other.inputs.get(role) == Some(input));
        changed |= before != self.inputs.len();
        let before = self.outputs.len();
        self.outputs
            .retain(|role, output| other.outputs.get(role) == Some(output));
        changed |= before != self.outputs.len();
        changed
    }
}

#[cfg(test)]
mod tests;
