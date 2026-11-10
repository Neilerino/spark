// TAG [SPARK-114]

//! All-normal-return guarantees, kept separate from unioned provenance summaries.

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::{BlockId, Local, MirFunction, Operand, Terminator};
use spark_types::{Ty, TypeTable};

use super::{value, Witness};
use crate::provenance::{aliases::Key, Flow, State};

pub(in crate::provenance) type Proofs = FxHashMap<Key, Guarantee>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct Certificate {
    pub completed: FxHashSet<Local>,
    pub returned: FxHashSet<Local>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::provenance) enum Guarantee {
    NoReturn,
    Returns(Certificate),
}

impl Guarantee {
    pub fn universe(function: &MirFunction, table: &TypeTable, normal_return: bool) -> Self {
        if !normal_return {
            return Self::NoReturn;
        }
        let completed: FxHashSet<_> = function
            .locals
            .iter()
            .skip(1)
            .take(function.param_count as usize)
            .filter(|decl| {
                matches!(
                    table.resolve(value::strip_mutable(decl.ty, table)),
                    Ty::Future { .. }
                )
            })
            .map(|decl| decl.local)
            .collect();
        let returned = completed
            .iter()
            .copied()
            .filter(|input| {
                value::strip_mutable(function.locals[input.index()].ty, table)
                    == value::strip_mutable(function.return_type, table)
            })
            .collect();
        Self::Returns(Certificate {
            completed,
            returned,
        })
    }

    pub fn has_effect(&self) -> bool {
        matches!(self, Self::Returns(certificate) if !certificate.completed.is_empty() || !certificate.returned.is_empty())
    }

    pub fn intersect(&mut self, other: &Self) -> bool {
        let before = self.clone();
        match (&mut *self, other) {
            (Self::Returns(left), Self::Returns(right)) => {
                left.completed
                    .retain(|input| right.completed.contains(input));
                left.returned.retain(|input| right.returned.contains(input));
            }
            (_, Self::NoReturn) => *self = Self::NoReturn,
            (Self::NoReturn, Self::Returns(_)) => {}
        }
        *self != before
    }
}

#[derive(Default)]
pub(in crate::provenance) struct CallProof {
    pub completed: Vec<Witness>,
    pub returned: Option<Witness>,
}

impl CallProof {
    pub fn exclude(&mut self, clobbered: &[Local]) {
        for witness in self.completed.iter_mut().chain(self.returned.iter_mut()) {
            witness.exclude(clobbered);
        }
    }
}

impl Flow<'_> {
    pub(in crate::provenance) fn future_guarantee(&mut self) -> Guarantee {
        let mut result: Option<Guarantee> = None;
        for index in 0..self.entries.len() {
            if !matches!(self.function.blocks[index].terminator, Terminator::Return) {
                continue;
            }
            let Some(mut state) = self.entries[index].clone() else {
                continue;
            };
            if !self.transfer_block(BlockId(index as u32), &mut state) {
                continue;
            }
            let candidate = Guarantee::Returns(Certificate {
                completed: state.joined_inputs,
                returned: state.future_copies.inputs(Local::RETURN_PLACE),
            });
            if let Some(result) = &mut result {
                result.intersect(&candidate);
            } else {
                result = Some(candidate);
            }
        }
        result.unwrap_or(Guarantee::NoReturn)
    }

    pub(in crate::provenance) fn call_future_proof(
        &self,
        key: &Key,
        args: &[Operand],
        state: &State,
    ) -> CallProof {
        let Some(Guarantee::Returns(certificate)) = self
            .program
            .and_then(|context| context.proofs)
            .and_then(|proofs| proofs.get(key))
        else {
            return CallProof::default();
        };
        let witness = |input: &Local| {
            let (Operand::Copy(place) | Operand::Move(place)) =
                args.get(input.index().checked_sub(1)?)?
            else {
                return None;
            };
            place
                .projections
                .is_empty()
                .then(|| state.future_copies.witness(place.local))
        };
        CallProof {
            completed: certificate.completed.iter().filter_map(witness).collect(),
            returned: certificate.returned.iter().find_map(witness),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_normal_return_without_a_join_removes_the_guarantee() {
        let mut proof = Guarantee::Returns(Certificate {
            completed: FxHashSet::from_iter([Local(1)]),
            returned: FxHashSet::from_iter([Local(1)]),
        });
        let different_return = Guarantee::Returns(Certificate {
            completed: FxHashSet::default(),
            returned: FxHashSet::from_iter([Local(2)]),
        });
        assert!(proof.intersect(&different_return));
        assert!(!proof.has_effect());
        assert!(!proof.intersect(&different_return));
    }

    #[test]
    fn a_nonreturning_function_does_not_publish_vacuous_completion() {
        let mut proof = Guarantee::NoReturn;
        assert!(!proof.intersect(&Guarantee::Returns(Certificate {
            completed: FxHashSet::from_iter([Local(1)]),
            returned: FxHashSet::default(),
        })));
        assert_eq!(proof, Guarantee::NoReturn);
        assert!(!proof.has_effect());
    }
}
