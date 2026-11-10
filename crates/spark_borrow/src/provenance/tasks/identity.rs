// TAG [SPARK-114]

use rustc_hash::FxHashSet;
use spark_mir::Local;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct Copies {
    equal: FxHashSet<(Local, Local)>,
}

pub(in crate::provenance) struct Witness(FxHashSet<Local>);

impl Copies {
    pub fn witness(&self, source: Local) -> Witness {
        Witness(self.peers(source))
    }

    pub fn peers(&self, source: Local) -> FxHashSet<Local> {
        self.equal
            .iter()
            .filter_map(|(left, right)| (*left == source).then_some(*right))
            .chain(std::iter::once(source))
            .collect()
    }

    pub fn forget(&mut self, local: Local) {
        self.equal
            .retain(|(left, right)| *left != local && *right != local);
    }

    pub fn assign(&mut self, destination: Local, witness: Witness, live: &FxHashSet<Local>) {
        self.forget(destination);
        let mut peers: FxHashSet<_> = witness
            .0
            .into_iter()
            .filter(|peer| *peer != destination && live.contains(peer))
            .collect();
        peers.insert(destination);
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
        before != self.equal.len()
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
}
