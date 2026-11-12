// TAG [SPARK-114]

use crate::provenance::{
    observations::Sample,
    tasks::{memory::Lane, Copies},
    State, Value,
};
use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::Local;
use spark_types::TypeId;
use std::sync::Arc;

#[derive(PartialEq, Eq)]
pub(in crate::provenance) struct Basis {
    backing: FxHashSet<(Local, TypeId)>,
    copies: Copies,
    owners: crate::provenance::tasks::lifecycle::Owners,
    promises: crate::provenance::channels::protocol::Promises,
    channels: crate::provenance::channels::identity::Copies,
    invalidated: FxHashSet<Local>,
    live: FxHashSet<Local>,
    pending: FxHashMap<Arc<Lane>, Value>,
    messages: FxHashMap<
        crate::provenance::channels::messages::Slot,
        crate::provenance::channels::messages::Contribution,
    >,
    settled: FxHashMap<Sample, Value>,
}

fn data(value: &Value) -> Value {
    let mut value = value.clone();
    value.clear_sources();
    value.clear_task_imports();
    value
}

impl State {
    pub(in crate::provenance) fn activity_basis(&self) -> Basis {
        Basis {
            backing: self.execution_backing_must.clone(),
            copies: self.future_copies.clone(),
            owners: self.future_owners.clone(),
            promises: self.queue_promises.clone(),
            channels: self.channel_copies.clone(),
            invalidated: self.invalidated_copies.clone(),
            live: self.live.clone(),
            messages: self
                .queue_current
                .iter()
                .map(|(slot, message)| {
                    (
                        slot.clone(),
                        crate::provenance::channels::messages::Contribution {
                            targets: message.targets.clone(),
                            births: message.births.clone(),
                            value: data(&message.value),
                        },
                    )
                })
                .collect(),
            pending: self
                .pending_memory
                .iter()
                .map(|(lane, value)| (lane.clone(), data(value)))
                .collect(),
            settled: self
                .settled_memory
                .iter()
                .map(|(sample, value)| (sample.clone(), data(value)))
                .collect(),
        }
    }
}
