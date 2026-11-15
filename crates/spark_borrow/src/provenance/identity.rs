// TAG [SPARK-114]

//! Equality between current storage roles, independent of allocation-site labels.

use rustc_hash::FxHashSet;
use std::hash::Hash;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Classes<R: Eq + Hash> {
    roles: FxHashSet<R>,
    equal: FxHashSet<(R, R)>,
}

impl<R: Eq + Hash> Default for Classes<R> {
    fn default() -> Self {
        Self {
            roles: FxHashSet::default(),
            equal: FxHashSet::default(),
        }
    }
}

impl<R: Clone + Eq + Hash> Classes<R> {
    pub fn insert(&mut self, role: R) {
        self.roles.insert(role);
    }

    pub fn roles(&self) -> impl Iterator<Item = &R> {
        self.roles.iter()
    }

    pub fn peers(&self, source: &R) -> FxHashSet<R> {
        self.equal
            .iter()
            .filter_map(|(left, right)| (left == source).then_some(right.clone()))
            .chain(std::iter::once(source.clone()))
            .collect()
    }

    pub fn retain(&mut self, keep: impl Fn(&R) -> bool) {
        self.roles.retain(keep);
        self.equal
            .retain(|(left, right)| self.roles.contains(left) && self.roles.contains(right));
    }

    pub fn assign(&mut self, destination: R, mut peers: FxHashSet<R>) {
        self.retain(|role| *role != destination);
        peers.insert(destination);
        self.roles.extend(peers.iter().cloned());
        for left in &peers {
            for right in &peers {
                if left != right {
                    self.equal.insert((left.clone(), right.clone()));
                }
            }
        }
    }

    pub fn intersect(&mut self, other: &Self) -> bool {
        let before = self.roles.len();
        self.roles.retain(|role| other.roles.contains(role));
        let mut changed = before != self.roles.len();
        let before = self.equal.len();
        self.equal.retain(|pair| other.equal.contains(pair));
        changed |= before != self.equal.len();
        changed
    }
}

pub(super) fn transferred_peers<'a, R: Clone + Eq + Hash + 'a>(
    transfers: impl Iterator<Item = (&'a R, &'a FxHashSet<R>)>,
) -> Vec<FxHashSet<R>> {
    let transfers: Vec<_> = transfers.collect();
    transfers
        .iter()
        .map(|(_, peers)| {
            transfers
                .iter()
                .filter(|(_, other)| !peers.is_disjoint(other))
                .map(|(destination, _)| (*destination).clone())
                .collect()
        })
        .collect()
}
