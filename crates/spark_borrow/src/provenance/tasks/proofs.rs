// TAG [SPARK-114]

//! All-normal-return guarantees, kept separate from unioned provenance summaries.

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::{BlockId, Local, MirFunction, Operand, Terminator};
use spark_types::{Ty, TypeTable};

use super::{value, Witness};
use crate::provenance::{aliases::Key, effects::EventKey, Flow, State};

pub(in crate::provenance) type Proofs = FxHashMap<Key, Proof>;
pub(in crate::provenance) type EventGuards = FxHashMap<EventKey, FxHashSet<Local>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::provenance) struct Proof {
    returns: Guarantee,
    pub events: EventGuards,
}

impl Proof {
    pub fn universe(
        function: &MirFunction,
        table: &TypeTable,
        normal_return: bool,
        events: Option<&FxHashSet<EventKey>>,
    ) -> Self {
        let inputs = future_inputs(function, table);
        Self {
            returns: Guarantee::universe(function, table, normal_return, inputs.clone()),
            events: events
                .into_iter()
                .flatten()
                .map(|event| (event.clone(), inputs.clone()))
                .collect(),
        }
    }

    pub fn seed_owner_outputs(
        &mut self,
        function: &MirFunction,
        table: &TypeTable,
        output: Option<super::outputs::CallbackRecipe>,
    ) {
        if let Guarantee::Returns(certificate) = &mut self.returns {
            certificate.released_provided = future_inputs(function, table);
            certificate.retains_no_owner = certificate.released_provided.clone();
            certificate.returned_owners = output.map(Box::new);
            certificate.forwarded_provided = certificate.returned.clone();
        }
    }

    pub fn has_effect(&self) -> bool {
        self.returns.has_effect() || self.events.values().any(|inputs| !inputs.is_empty())
    }

    pub fn restrict(&mut self, returns: &Guarantee, events: &EventGuards) -> bool {
        let mut changed = self.returns.intersect(returns);
        for (event, inputs) in &mut self.events {
            let before = inputs.len();
            inputs.retain(|input| {
                events
                    .get(event)
                    .is_some_and(|observed| observed.contains(input))
            });
            changed |= before != inputs.len();
        }
        changed
    }
}

#[derive(Clone, Default)]
pub(in crate::provenance) struct EventFence {
    pub inputs: FxHashSet<Local>,
    pub peers: FxHashSet<Local>,
}

fn future_inputs(function: &MirFunction, table: &TypeTable) -> FxHashSet<Local> {
    function
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
        .collect()
}

fn actual_witness(input: Local, args: &[Operand], state: &State) -> Option<Witness> {
    let (Operand::Copy(place) | Operand::Move(place)) = args.get(input.index().checked_sub(1)?)?
    else {
        return None;
    };
    place
        .projections
        .is_empty()
        .then(|| state.future_copies.witness(place.local))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct Certificate {
    pub returned_owners: Option<Box<super::outputs::CallbackRecipe>>,
    pub forwarded_provided: FxHashSet<Local>,
    pub completed: FxHashSet<Local>,
    pub returned: FxHashSet<Local>,
    pub released_provided: FxHashSet<Local>,
    /// No owner from this formal's lineage remains; aliased actuals must be bound together.
    pub retains_no_owner: FxHashSet<Local>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::provenance) enum Guarantee {
    NoReturn,
    Returns(Certificate),
}

impl Guarantee {
    fn universe(
        function: &MirFunction,
        table: &TypeTable,
        normal_return: bool,
        completed: FxHashSet<Local>,
    ) -> Self {
        if !normal_return {
            return Self::NoReturn;
        }
        let returned: FxHashSet<_> = completed
            .iter()
            .copied()
            .filter(|input| {
                value::strip_mutable(function.locals[input.index()].ty, table)
                    == value::strip_mutable(function.return_type, table)
            })
            .collect();
        Self::Returns(Certificate {
            returned_owners: None,
            forwarded_provided: returned.clone(),
            released_provided: completed.clone(),
            retains_no_owner: completed.clone(),
            completed,
            returned,
        })
    }

    pub fn has_effect(&self) -> bool {
        matches!(self, Self::Returns(certificate) if certificate.returned_owners.is_some() || !certificate.completed.is_empty() || !certificate.returned.is_empty()
            || !certificate.released_provided.is_empty() || !certificate.retains_no_owner.is_empty() || !certificate.forwarded_provided.is_empty())
    }

    pub fn intersect(&mut self, other: &Self) -> bool {
        let before = self.clone();
        match (&mut *self, other) {
            (Self::Returns(left), Self::Returns(right)) => {
                if left.returned_owners != right.returned_owners {
                    left.returned_owners = None;
                }
                left.completed
                    .retain(|input| right.completed.contains(input));
                left.returned.retain(|input| right.returned.contains(input));
                left.forwarded_provided
                    .retain(|input| right.forwarded_provided.contains(input));
                left.released_provided
                    .retain(|input| right.released_provided.contains(input));
                left.retains_no_owner
                    .retain(|input| right.retains_no_owner.contains(input));
            }
            (_, Self::NoReturn) => *self = Self::NoReturn,
            (Self::NoReturn, Self::Returns(_)) => {}
        }
        *self != before
    }
}

#[derive(Default)]
pub(in crate::provenance) struct CallProof {
    pub forwarded_provided: FxHashMap<usize, Witness>,
    pub forwarded: Option<super::forwarding::ForwardInput>,
    pub output_owners: Option<super::outputs::BoundOutput>,
    pub completed: Vec<Witness>,
    pub returned: Option<Witness>,
    pub released_owners: FxHashMap<usize, Witness>,
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
            let returned = state.future_copies.inputs(Local::RETURN_PLACE);
            let output_role = super::Role::root(Local::RETURN_PLACE);
            let forwarded_provided = future_inputs(self.function, self.table)
                .into_iter()
                .filter(|input| {
                    returned.contains(input)
                        && value::strip_mutable(self.function.locals[input.index()].ty, self.table)
                            == value::strip_mutable(self.function.return_type, self.table)
                        && state.input_owners.tags(&output_role)
                            == FxHashSet::from_iter([super::lifecycle::Token::Provided(*input)])
                        && state
                            .input_owners
                            .retains_only(*input, &FxHashSet::from_iter([output_role.clone()]))
                })
                .collect();
            let candidate = Guarantee::Returns(Certificate {
                forwarded_provided,
                returned_owners: self.observed_callback_recipe(&state).map(Box::new),
                completed: state.joined_inputs,
                returned,
                released_provided: future_inputs(self.function, self.table)
                    .into_iter()
                    .filter(|input| state.input_owners.released_provided(*input))
                    .collect(),
                retains_no_owner: future_inputs(self.function, self.table)
                    .into_iter()
                    .filter(|input| state.input_owners.retains_no_owner(*input))
                    .collect(),
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
            .map(|proof| &proof.returns)
        else {
            return CallProof::default();
        };
        let witness = |input: &Local| actual_witness(*input, args, state);
        CallProof {
            forwarded: None,
            forwarded_provided: certificate
                .forwarded_provided
                .iter()
                .filter_map(|input| Some((input.index().checked_sub(1)?, witness(input)?)))
                .collect(),
            output_owners: certificate
                .returned_owners
                .as_ref()
                .and_then(|recipe| self.bind_output_recipe(recipe, args, state)),
            completed: certificate.completed.iter().filter_map(witness).collect(),
            returned: certificate.returned.iter().find_map(witness),
            released_owners: certificate
                .retains_no_owner
                .iter()
                .filter_map(|input| {
                    let index = input.index().checked_sub(1)?;
                    Some((index, actual_witness(*input, args, state)?))
                })
                .collect(),
        }
    }

    pub(in crate::provenance) fn record_event_guard(
        &mut self,
        event: EventKey,
        inputs: &FxHashSet<Local>,
    ) {
        self.event_guards
            .entry(event)
            .and_modify(|joined| joined.retain(|input| inputs.contains(input)))
            .or_insert_with(|| inputs.clone());
    }

    pub(in crate::provenance) fn event_fence(
        &self,
        key: &Key,
        event: &EventKey,
        args: Option<&[Operand]>,
        state: &State,
    ) -> EventFence {
        let Some(args) = args else {
            return EventFence::default();
        };
        let mut fence = EventFence {
            inputs: state.joined_inputs.clone(),
            ..EventFence::default()
        };
        let guarded = self
            .program
            .and_then(|context| context.proofs)
            .and_then(|proofs| proofs.get(key))
            .and_then(|proof| proof.events.get(event));
        for input in guarded.into_iter().flatten() {
            if let Some(witness) = actual_witness(*input, args, state) {
                fence.peers.extend(witness.locals());
                fence.inputs.extend(witness.inputs);
            }
        }
        fence
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
            ..Certificate::default()
        });
        let different_return = Guarantee::Returns(Certificate {
            completed: FxHashSet::default(),
            returned: FxHashSet::from_iter([Local(2)]),
            ..Certificate::default()
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
            ..Certificate::default()
        })));
        assert_eq!(proof, Guarantee::NoReturn);
        assert!(!proof.has_effect());
    }
    #[test]
    fn dropping_a_moved_parameter_releases_its_owner_without_completing_the_task() {
        use spark_mir::{BasicBlock, Constant, LocalDecl, Place, Rvalue, Statement};
        use spark_resolve::DefId;
        use spark_span::Span;
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(spark_types::IntWidth::I64));
        let future = table.intern(Ty::Future { inner: int });
        let function = MirFunction {
            def_id: DefId::new(9950),
            param_count: 1,
            return_type: int,
            locals: [int, future, future]
                .into_iter()
                .enumerate()
                .map(|(index, ty)| LocalDecl {
                    local: Local(index as u32),
                    ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Use(Operand::Move(Place::from_local(Local(1)))),
                        span: Span::DUMMY,
                    },
                    Statement::Drop {
                        place: Place::from_local(Local(2)),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(0))),
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };
        let mut flow = Flow::new(&function, &table, None);
        flow.solve(&crate::facts::extract_facts(&function));
        let Guarantee::Returns(certificate) = flow.future_guarantee() else {
            panic!("expected a normal return")
        };
        assert!(
            certificate.completed.is_empty(),
            "owner release does not prove a join"
        );
        assert_eq!(
            certificate.released_provided,
            FxHashSet::from_iter([Local(1)])
        );
        assert_eq!(
            certificate.retains_no_owner,
            FxHashSet::from_iter([Local(1)])
        );
    }
}

#[cfg(test)]
mod owner_tests;
