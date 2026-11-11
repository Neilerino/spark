// TAG [SPARK-114]

//! Typed publication facts that can reach a synchronous continuation.

use rustc_hash::FxHashMap;

use crate::provenance::{address::OpaqueSite, channels::QueueKey};

use super::{
    export_value, CallActuals, Flow, Key, MirFunction, Operand, PrefixMemory, PrefixWrites, Value,
};

#[derive(Clone, Debug, Default)]
pub(in crate::provenance) struct PublicationValues {
    pub memory: PrefixMemory,
    pub queues: FxHashMap<QueueKey, Value>,
}

impl PublicationValues {
    pub fn join_memory(&mut self, memory: &PrefixMemory) -> bool {
        let mut changed = false;
        for (event, value) in memory {
            let missing = !self.memory.contains_key(event);
            changed |= self.memory.entry(event.clone()).or_default().join(value) || missing;
        }
        changed
    }

    pub fn join(&mut self, other: &Self) -> bool {
        let mut changed = self.join_memory(&other.memory);
        for (event, value) in &other.queues {
            let missing = !self.queues.contains_key(event);
            changed |= self.queues.entry(event.clone()).or_default().join(value) || missing;
        }
        changed
    }

    pub fn events(&self) -> impl Iterator<Item = (OpaqueSite, &Value)> {
        self.memory
            .iter()
            .map(|((site, _), value)| (*site, value))
            .chain(
                self.queues
                    .iter()
                    .map(|((site, _, _), value)| (*site, value)),
            )
    }

    pub fn exported(&self, function: spark_resolve::DefId) -> Self {
        Self {
            memory: self
                .memory
                .iter()
                .map(|(event, value)| (event.clone(), export_value(value, function)))
                .collect(),
            queues: self
                .queues
                .iter()
                .map(|(event, value)| (event.clone(), export_value(value, function)))
                .collect(),
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.memory.is_empty() && self.queues.is_empty()
    }
}

impl Flow<'_> {
    pub(super) fn bind_publications(
        &mut self,
        publications: &PublicationValues,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        key: &Key,
        args: Option<&[Operand]>,
    ) -> PrefixWrites {
        PrefixWrites {
            memory: self.bind_memory_events(&publications.memory, function, actuals, key, args),
            queues: self.bind_queue_writes(&publications.queues, function, actuals, key, args),
        }
    }

    pub(super) fn remember_publications(
        &self,
        writes: &PrefixWrites,
        publications: &mut PublicationValues,
    ) {
        for event in &writes.memory {
            self.remember_memory_publication(event, &mut publications.memory);
        }
        for event in &writes.queues {
            for target in self.queue_targets(&event.write.target) {
                let mut value = event.write.value.clone();
                value.sources.clear();
                publications
                    .queues
                    .entry((event.site, target, event.write.ty))
                    .or_default()
                    .join(&value);
            }
        }
    }
}
