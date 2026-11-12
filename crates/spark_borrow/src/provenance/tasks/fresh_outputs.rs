// TAG [SPARK-114]

//! Conditional fresh queue outputs; candidates carry data, certificates carry guarantees.

use super::{fresh::Birth, lifecycle::Coverage, Role};
use crate::provenance::{
    address::OpaqueSite,
    aliases::Port,
    channels::{counts::Count, Target},
    demand::Storage,
    Flow, State, Value,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) struct Recipe {
    pub queue: Arc<Port>,
    pub send: OpaqueSite,
    pub birth: Birth,
}

pub(in crate::provenance) type Outputs = FxHashMap<Recipe, Value>;

impl Flow<'_> {
    pub(in crate::provenance) fn fresh_output_candidates(&self, state: &State) -> Outputs {
        let mut outputs = Outputs::default();
        for (slot, message) in &state.queue_current {
            let role = Role::message(slot.clone());
            let Some(Coverage::ThroughQueue(queue)) = state.future_owners.coverage(&role) else {
                continue;
            };
            let witness = state.future_copies.role_witness(&role);
            let Some(birth) = witness.birth else { continue };
            if birth.site.function != self.function.def_id
                || witness.peers != FxHashSet::from_iter([role.clone()])
                || !state.input_owners.tags(&role).is_empty()
                || crate::provenance::value::strip_mutable(queue.pointee, self.table)
                    != birth.future_ty
            {
                continue;
            }
            let Some(record) = birth.record(&message.value) else {
                continue;
            };
            if !self.leaf_task(&record) {
                continue;
            }
            outputs.insert(
                Recipe {
                    queue,
                    send: slot.site,
                    birth,
                },
                crate::provenance::effects::export_value(
                    &message
                        .value
                        .clone()
                        .sourced(&Storage::QueueCurrent(slot.clone())),
                    self.function.def_id,
                ),
            );
        }
        outputs
    }

    pub(in crate::provenance) fn observed_fresh_outputs(&self, state: &State) -> FxHashSet<Recipe> {
        let returned_access = self.operand_queue_accesses(
            &spark_mir::Operand::Copy(spark_mir::Place::from_local(spark_mir::Local::RETURN_PLACE)),
            state,
        );
        self.fresh_output_candidates(state)
            .into_keys()
            .filter(|recipe| {
                let Some(target) = Target::from_port(&recipe.queue) else {
                    return false;
                };
                let targets = self.queue_targets(&target);
                targets.len() == 1
                    && targets.iter().all(|target| {
                        state.queue_send_counts.get(target) == Count::ONE
                            && state.queue_accesses.allows(target)
                            && returned_access.allows(target)
                            && !state.exposed_queues.contains(target)
                            && !state
                                .queues
                                .contains_key(&(target.clone(), recipe.birth.future_ty))
                    })
            })
            .collect()
    }
}

impl crate::provenance::effects::Summary {
    pub(in crate::provenance) fn join_fresh_outputs(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (recipe, value) in &other.fresh_outputs {
            changed |= self
                .fresh_outputs
                .entry(recipe.clone())
                .or_default()
                .join(value);
        }
        changed
    }
}

impl Flow<'_> {
    pub(in crate::provenance) fn export_fresh_outputs(
        &self,
        state: &State,
        summary: &mut crate::provenance::effects::Summary,
    ) {
        for (recipe, value) in self.fresh_output_candidates(state) {
            summary
                .fresh_outputs
                .entry(recipe)
                .or_default()
                .join(&value);
        }
    }
}
