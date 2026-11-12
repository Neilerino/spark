// TAG [SPARK-114]

//! Counts of actual sends in a returning function invocation, excluding sampled task effects.
//! Source target labels address bounds; they prove neither current identity nor closed queue access.

use super::Target;
use rustc_hash::FxHashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::provenance) struct Count(u8);

impl Count {
    pub const ZERO: Self = Self(1);
    pub const ONE: Self = Self(2);
    pub const MANY: Self = Self(4);
    pub const UNKNOWN: Self = Self(7);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn plus(self, other: Self) -> Self {
        let mut bits = 0;
        let buckets = [Self::ZERO, Self::ONE, Self::MANY];
        for left in 0..=2 {
            for right in 0..=2 {
                if self.0 & (1 << left) != 0 && other.0 & (1 << right) != 0 {
                    bits |= buckets[(left + right).min(2)].0;
                }
            }
        }
        Self(bits)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct Sends {
    pub targets: FxHashMap<Target, Count>,
    pub unknown: bool,
}

impl Sends {
    pub fn get(&self, target: &Target) -> Count {
        if self.unknown {
            Count::UNKNOWN
        } else {
            self.targets.get(target).copied().unwrap_or(Count::ZERO)
        }
    }

    pub fn add(&mut self, target: Target, count: Count) {
        let old = self.targets.get(&target).copied().unwrap_or(Count::ZERO);
        self.targets.insert(target, old.plus(count));
    }

    pub fn remap(&mut self, bindings: &FxHashMap<Target, Target>) {
        for (target, count) in std::mem::take(&mut self.targets) {
            self.add(bindings.get(&target).cloned().unwrap_or(target), count);
        }
    }

    pub fn then(&mut self, other: &Self) {
        self.unknown |= other.unknown;
        for (target, count) in &other.targets {
            self.add(target.clone(), *count);
        }
    }

    pub fn join(&mut self, other: &Self) -> bool {
        let before = self.clone();
        self.unknown |= other.unknown;
        for (target, count) in &mut self.targets {
            *count = count.union(other.targets.get(target).copied().unwrap_or(Count::ZERO));
        }
        for (target, count) in &other.targets {
            self.targets
                .entry(target.clone())
                .or_insert_with(|| Count::ZERO.union(*count));
        }
        *self != before
    }
}

pub(in crate::provenance) fn join_returns(into: &mut Option<Sends>, other: Option<&Sends>) -> bool {
    let Some(other) = other else {
        return false;
    };
    if let Some(current) = into {
        current.join(other)
    } else {
        *into = Some(other.clone());
        true
    }
}

impl crate::provenance::Flow<'_> {
    pub(in crate::provenance) fn actual_queue_send_counts(
        &self,
        events: &[super::QueueEvent],
    ) -> Sends {
        let targets: rustc_hash::FxHashSet<_> = events
            .iter()
            .flat_map(|event| self.queue_targets(&event.write.target))
            .collect();
        let mut sends = Sends {
            unknown: !events.is_empty() && targets.is_empty(),
            ..Sends::default()
        };
        let count = if targets.len() == 1 {
            Count::ONE
        } else {
            Count::ZERO.union(Count::ONE)
        };
        for target in targets {
            sends.unknown |= matches!(target, Target::Unknown { .. } | Target::Validation { .. });
            sends.add(target, count);
        }
        sends
    }
}
