// TAG [SPARK-114]

//! Blocking receipt consumes one proven present message, never queue history.

use super::{identity::Role, messages::Slot, protocol::Promise};
use crate::provenance::{
    demand::{Coordinate, Storage},
    tasks, value, Flow, Point, State, Value,
};
use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::{Local, Operand, Place};
use spark_types::TypeId;

pub(in crate::provenance) struct Receipt {
    slot: Slot,
    queue: Role,
    peers: FxHashSet<Role>,
    promise: Promise,
    pub value: Value,
}

impl Receipt {
    pub fn snapshot(flow: &Flow<'_>, operand: &Operand, ty: TypeId, state: &State) -> Option<Self> {
        let Operand::Copy(place) = operand else {
            return None;
        };
        if !place.projections.is_empty() {
            return None;
        }
        let queue = Role::Local(place.local);
        let witness = state.channel_copies.witness(&queue);
        let slot = witness.output?;
        if value::strip_mutable(ty, flow.table) != slot.ty {
            return None;
        }
        let receipt = Self {
            promise: state.queue_promises.get(&slot)?.clone(),
            value: state
                .queue_current
                .get(&slot)?
                .value
                .clone()
                .sourced(&Storage::QueueCurrent(slot.clone())),
            slot,
            queue,
            peers: witness.peers,
        };
        receipt.valid(flow, &[], state).then_some(receipt)
    }

    fn valid(&self, flow: &Flow<'_>, clobbered: &[Local], state: &State) -> bool {
        let role = tasks::Role::message(self.slot.clone());
        let witness = state.future_copies.role_witness(&role);
        self.promise.present
            && state.queue_promises.get(&self.slot) == Some(&self.promise)
            && state.current_output_queue(&self.slot).is_some()
            && state.channel_copies.witness(&self.queue).peers == self.peers
            && self.peers.iter().all(|peer| {
                state.live.contains(&peer.local())
                    && !clobbered.contains(&peer.local())
                    && !state.invalidated_copies.contains(&peer.local())
            })
            && state.input_owners.value_tags(&role).is_empty()
            && state.future_owners.known_role(&role)
            && witness.birth == Some(self.promise.recipe.birth)
            && witness.peers == FxHashSet::from_iter([role])
            && flow.queue_aliases_in(
                &state.channel_copies.witness(&self.queue),
                &self.promise.queue,
                Some(&self.slot),
                state,
            )
            && state.queue_current.get(&self.slot).is_some_and(|message| {
                message.targets == FxHashSet::from_iter([self.promise.queue.clone()])
                    && self
                        .promise
                        .recipe
                        .birth
                        .record(&message.value)
                        .is_some_and(|record| flow.leaf_task(&record))
            })
    }

    pub fn poison_access(&self, access: &mut super::access::Accesses) {
        access.blocked.remove(&self.promise.queue);
    }

    pub fn finish(
        self,
        flow: &mut Flow<'_>,
        destination: &Place,
        clobbered: &[Local],
        point: Point,
        state: &mut State,
    ) {
        if !self.destination_valid(flow, destination, clobbered, state)
            || !self.valid(flow, clobbered, state)
        {
            flow.cancel_queue_promise(&self.slot, point, state);
            return;
        }
        let Some(message) = state.queue_current.get(&self.slot) else {
            return;
        };
        let payload = message
            .value
            .clone()
            .sourced(&Storage::QueueCurrent(self.slot.clone()));
        if !flow.store_role_value(&tasks::Role::root(destination.local), payload, point, state) {
            flow.cancel_queue_promise(&self.slot, point, state);
            return;
        }
        let stored = state
            .value(destination.local)
            .sourced(&Storage::Logical(destination.local));
        let Some(record) = self.promise.recipe.birth.record(&stored) else {
            flow.cancel_queue_promise(&self.slot, point, state);
            return;
        };
        let mut expected = record.callback.clone();
        expected.clear_sources();
        flow.install_callback_outputs(
            FxHashMap::from_iter([(
                record.callback_ty,
                tasks::outputs::CallbackOutput {
                    callback: record.callback,
                    ..Default::default()
                },
            )]),
            destination,
            point,
            state,
        );
        if !self.destination_valid(flow, destination, clobbered, state)
            || !self.valid(flow, clobbered, state)
            || state
                .executions
                .iter()
                .filter(|((local, _), _)| *local == destination.local)
                .count()
                != 1
            || state
                .executions
                .get(&(destination.local, record.callback_ty))
                != Some(&expected)
        {
            flow.cancel_queue_promise(&self.slot, point, state);
            return;
        }
        let message = tasks::Role::message(self.slot.clone());
        let witness = state.future_copies.role_witness(&message);
        state.future_copies.assign(
            destination.local,
            witness,
            &tasks::Live::new(&state.live, &state.queue_current),
        );
        state.queue_promises.remove(&self.slot);
        state.channel_copies.forget_output(&self.slot);
        state.future_copies.forget_output(&self.slot);
        flow.record_path_store(
            point,
            Coordinate {
                storage: Storage::QueueCurrent(self.slot.clone()),
                path: Vec::new(),
            },
            &Value::default(),
            true,
        );
        state.future_owners.forget_role(&message);
        state.future_copies.forget_role(&message);
        state.queue_current.remove(&self.slot);
        state.install_execution_callback(destination.local, record.callback_ty, expected, true);
        state
            .future_owners
            .grant(destination.local, &state.future_copies);
    }

    fn destination_valid(
        &self,
        flow: &Flow<'_>,
        destination: &Place,
        clobbered: &[Local],
        state: &State,
    ) -> bool {
        destination.projections.is_empty()
            && state.live.contains(&destination.local)
            && !clobbered.contains(&destination.local)
            && !state.invalidated_copies.contains(&destination.local)
            && value::strip_mutable(flow.place_type(destination), flow.table) == self.slot.ty
            && self
                .promise
                .recipe
                .birth
                .record(&state.value(destination.local))
                .is_some_and(|record| flow.leaf_task(&record))
    }
}
