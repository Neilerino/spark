// TAG [SPARK-114]

//! Freshness is construction provenance, not a proof of closed queue access or message ownership.

mod transfer;
pub(in crate::provenance) use transfer::Captured;

use crate::provenance::identity::Classes;
use rustc_hash::FxHashSet;
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
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct Copies {
    classes: Classes<Role>,
    fresh: FxHashSet<Role>,
}

impl Copies {
    pub fn witness(&self, source: &Role) -> Witness {
        Witness {
            peers: self.classes.peers(source),
            fresh: self.fresh.contains(source),
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

    fn retain(&mut self, keep: impl Fn(&Role) -> bool) {
        self.classes.retain(&keep);
        self.fresh.retain(keep);
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
        if witness.fresh {
            self.fresh.insert(destination.clone());
        }
        self.classes.assign(destination, peers);
    }

    pub fn intersect(&mut self, other: &Self) -> bool {
        let mut changed = self.classes.intersect(&other.classes);
        let before = self.fresh.len();
        self.fresh.retain(|role| other.fresh.contains(role));
        changed |= before != self.fresh.len();
        changed
    }
}

#[cfg(test)]
mod tests;
