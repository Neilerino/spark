// TAG [SPARK-114]

//! Rehome an input execution independently from proving its root owner disposition.

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::{Local, Operand, Place, Rvalue};
use spark_types::Ty;

use super::{execution::ExecutionTransfer, lifecycle::Tags, proofs::CallProof, Role, Witness};
use crate::provenance::{value, Flow, Point, State};

pub(in crate::provenance) struct ForwardInput {
    witness: Witness,
    covered: FxHashSet<usize>,
    tokens: Tags,
    graphs: Vec<ExecutionTransfer>,
    coverage: Option<super::lifecycle::Coverage>,
}

impl ForwardInput {
    fn root_tokens(
        args: &[Operand],
        covered: &FxHashSet<usize>,
        roots: &FxHashSet<Local>,
        state: &State,
    ) -> Tags {
        let mut tokens = Tags::default();
        for (index, argument) in args.iter().enumerate() {
            let (Operand::Copy(place) | Operand::Move(place)) = argument else {
                continue;
            };
            if !place.projections.is_empty() || !roots.contains(&place.local) {
                continue;
            }
            for token in state.input_owners.value_tags(&Role::root(place.local)) {
                if covered.contains(&index) {
                    tokens.insert(if matches!(argument, Operand::Copy(_)) {
                        token.derived()
                    } else {
                        token
                    });
                } else {
                    tokens.extend([token, token.derived()]);
                }
            }
        }
        tokens
    }

    pub fn covers(&self, index: usize) -> bool {
        self.covered.contains(&index)
    }

    pub fn reserves(&self, local: Local) -> bool {
        self.witness.contains_local(local)
    }

    pub fn detach(&mut self, flow: &mut Flow<'_>, point: Point, state: &mut State) {
        self.coverage = self.coverage.take().and_then(|coverage| {
            coverage.meet(state.future_owners.group_coverage(&self.witness.peers)?)
        });
        for graph in &mut self.graphs {
            graph.restrict_coverage(state);
        }
        for graph in &self.graphs {
            graph.detach(flow, point, state);
        }
    }

    pub fn install(self, flow: &mut Flow<'_>, destination: Local, point: Point, state: &mut State) {
        state
            .input_owners
            .add_output(Role::root(destination), self.tokens);
        for graph in self.graphs {
            graph.install(flow, destination, point, state);
        }
        let valid = !state.invalidated_copies.contains(&destination)
            && self
                .witness
                .peers
                .iter()
                .all(|peer| !peer.frame_clobbered(&[], &state.invalidated_copies));
        if !valid {
            state
                .future_owners
                .revoke(&self.witness, &state.future_copies);
            state.future_owners.forget_role(&Role::root(destination));
        } else if let Some(coverage) = self.coverage {
            state.future_owners.install_role(
                Role::root(destination),
                coverage,
                &state.future_copies,
            );
        }
    }
}

impl Flow<'_> {
    pub(in crate::provenance) fn forward_input(
        &self,
        proof: &CallProof,
        args: &[Operand],
        destination: &Place,
        clobbered: &[Local],
        point: Point,
        state: &State,
    ) -> Option<ForwardInput> {
        if !destination.projections.is_empty() {
            return None;
        }
        let parent_ty = value::strip_mutable(self.place_type(destination), self.table);
        if !matches!(self.table.resolve(parent_ty), Ty::Future { .. }) {
            return None;
        }
        let returned = proof.returned.as_ref()?;
        let valid = |witness: &Witness| {
            witness
                .peers
                .iter()
                .all(|peer| !peer.frame_clobbered(clobbered, &state.invalidated_copies))
        };
        if !valid(returned) {
            return None;
        }
        let roots: FxHashSet<_> = returned
            .locals()
            .filter(|local| {
                state.live.contains(local)
                    && value::strip_mutable(self.function.locals[local.index()].ty, self.table)
                        == parent_ty
            })
            .collect();
        if roots.is_empty() {
            return None;
        }
        let covered: FxHashSet<_> = proof
            .forwarded_provided
            .iter()
            .filter(|(_, witness)| valid(witness))
            .filter_map(|(index, witness)| {
                let (Operand::Copy(place) | Operand::Move(place)) = args.get(*index)? else {
                    return None;
                };
                (place.projections.is_empty()
                    && roots.contains(&place.local)
                    && witness.contains_local(place.local))
                .then_some(*index)
            })
            .collect();
        let tokens = ForwardInput::root_tokens(args, &covered, &roots, state);
        let completed = proof
            .completed
            .iter()
            .any(|witness| roots.iter().any(|root| witness.contains_local(*root)));
        let mut sources = FxHashMap::default();
        if !completed {
            for (local, ty) in state
                .executions
                .keys()
                .filter(|(local, _)| roots.contains(local))
            {
                if sources.insert(*ty, *local).is_some() {
                    return None;
                }
            }
        }
        let mut graphs = Vec::new();
        let sources: FxHashSet<_> = sources.into_values().collect();
        for source in sources {
            graphs.extend(self.moving_executions(
                &Rvalue::Use(Operand::Move(Place::from_local(source))),
                destination,
                point,
                state,
            ));
        }
        if !graphs.iter().all(|graph| graph.valid(clobbered, state)) {
            return None;
        }
        Some(ForwardInput {
            coverage: (!covered.is_empty())
                .then(|| state.future_owners.group_coverage(&returned.peers))
                .flatten(),
            witness: Witness {
                peers: returned.peers.clone(),
                inputs: returned.inputs.clone(),
                birth: returned.birth,
                produces: returned.produces.clone(),
            },
            covered,
            tokens,
            graphs,
        })
    }
}
