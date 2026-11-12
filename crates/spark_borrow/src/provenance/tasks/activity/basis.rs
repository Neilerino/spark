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
    invalidated: FxHashSet<Local>,
    live: FxHashSet<Local>,
    pending: FxHashMap<Arc<Lane>, Value>,
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
            invalidated: self.invalidated_copies.clone(),
            live: self.live.clone(),
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
