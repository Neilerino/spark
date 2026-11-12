// TAG [SPARK-114]

//! Carrier contents changed by collection builtins.

use crate::provenance::tasks::proofs::CallProof;
use spark_intern::kw;
use spark_mir::Operand;
use spark_resolve::DefinitionKind;
use spark_types::Ty;

use super::{value, Flow, Slot, State, Value};

impl Flow<'_> {
    pub(super) fn collection_call(
        &self,
        func: &Operand,
        args: &[Operand],
        state: &State,
    ) -> Option<super::effects::CallOutcome> {
        let def = self.function.resolve_callee_def_id(func)?;
        let info = self.program?.definitions?.get(def)?;
        if !matches!(info.kind, DefinitionKind::BuiltinMethod) || info.name != kw::APPEND {
            return None;
        }
        let receiver @ (Operand::Copy(place) | Operand::Move(place)) = args.first()? else {
            return None;
        };
        let ty = self.place_type(place);
        if !matches!(
            self.table.resolve(value::strip_mutable(ty, self.table)),
            Ty::List(_)
        ) {
            return None;
        }
        let mut contents = self.operand(receiver, state);
        contents.join(
            &self
                .operand(args.get(1)?, state)
                .prefixed(Slot::Element, ty, self.table),
        );
        Some(super::effects::CallOutcome {
            queue_send_counts: crate::provenance::channels::counts::Sends::default(),
            direct_queue_send: None,
            fresh_birth: None,
            fresh_leaf_task: false,
            spawn_owners: None,
            returned_owner_tags: crate::provenance::tasks::lifecycle::Tags::default(),
            retained_captures: Vec::new(),
            returned_callbacks: crate::provenance::tasks::outputs::CallbackOutputs::default(),
            residuals: Vec::new(),
            async_memory: Vec::new(),
            tasks: Vec::new(),
            completed_task: None,
            clobbered_copies: Vec::new(),
            future_proof: CallProof::default(),
            returned_prefix: rustc_hash::FxHashSet::default(),
            publications: crate::provenance::effects::PrefixWrites::default(),
            returned: Value::default(),
            place_writes: vec![(place.clone(), contents)],
            prefix_memory: Vec::new(),
            prefix_residuals: Vec::new(),
            memory: Vec::new(),
            queues: Vec::new(),
            queue_contents: Vec::new(),
        })
    }
}
