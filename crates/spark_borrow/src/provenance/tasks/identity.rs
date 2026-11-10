// TAG [SPARK-114]

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::Local;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct Copies {
    equal: FxHashSet<(Local, Local)>,
    inputs: FxHashMap<Local, FxHashSet<Local>>,
}

pub(in crate::provenance) struct Witness {
    pub peers: FxHashSet<Local>,
    pub inputs: FxHashSet<Local>,
}

impl Witness {
    pub fn exclude(&mut self, clobbered: &[Local]) {
        self.peers.retain(|local| !clobbered.contains(local));
    }
}

impl Copies {
    pub fn witness(&self, source: Local) -> Witness {
        Witness {
            peers: self.peers(source),
            inputs: self.inputs(source),
        }
    }

    pub fn seed_input(&mut self, input: Local) {
        self.inputs.insert(input, FxHashSet::from_iter([input]));
    }

    pub fn inputs(&self, local: Local) -> FxHashSet<Local> {
        self.inputs.get(&local).cloned().unwrap_or_default()
    }

    pub fn peers(&self, source: Local) -> FxHashSet<Local> {
        self.equal
            .iter()
            .filter_map(|(left, right)| (*left == source).then_some(*right))
            .chain(std::iter::once(source))
            .collect()
    }

    pub fn forget(&mut self, local: Local) {
        self.inputs.remove(&local);
        self.equal
            .retain(|(left, right)| *left != local && *right != local);
    }

    pub fn assign(&mut self, destination: Local, witness: Witness, live: &FxHashSet<Local>) {
        self.forget(destination);
        let mut peers: FxHashSet<_> = witness
            .peers
            .into_iter()
            .filter(|peer| *peer != destination && live.contains(peer))
            .collect();
        peers.insert(destination);
        if !witness.inputs.is_empty() {
            self.inputs.insert(destination, witness.inputs);
        }
        for left in &peers {
            for right in &peers {
                if left != right {
                    self.equal.insert((*left, *right));
                }
            }
        }
    }

    pub fn intersect(&mut self, other: &Self) -> bool {
        let before = self.equal.len();
        self.equal.retain(|pair| other.equal.contains(pair));
        let mut changed = before != self.equal.len();
        self.inputs.retain(|local, inputs| {
            let before = inputs.len();
            inputs.retain(|input| {
                other
                    .inputs
                    .get(local)
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
