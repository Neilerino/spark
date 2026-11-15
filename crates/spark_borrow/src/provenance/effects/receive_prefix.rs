// TAG [SPARK-114]

//! Bind synchronous receives against publications that can precede the receive.

use rustc_hash::FxHashSet;

use crate::provenance::{
    address::{Dependency, Endpoint, FrameLifetime, OpaqueSite},
    observations::ReadView,
    read::{Access, Accesses},
};

use super::{prefix::Prefix, CallActuals, Flow, MirFunction, Summary, Value};

fn receive_context(access: &Access) -> Option<&super::Receive> {
    match &access.target {
        Dependency::InputValue(selector, site)
        | Dependency::Address(Endpoint::Input(selector, site))
            if selector.queue.is_some() =>
        {
            site.as_deref()
        }
        _ => None,
    }
}

fn returned_owners(
    event: OpaqueSite,
    value: &Value,
    reached: &[super::PrefixOccurrence],
    returns: &mut FxHashSet<OpaqueSite>,
) {
    for leaf in value.leaves.values() {
        let addresses = leaf
            .endpoints
            .iter()
            .chain(
                leaf.dependencies
                    .iter()
                    .filter_map(|dependency| match dependency {
                        Dependency::Address(endpoint) => Some(endpoint),
                        Dependency::InputValue(..) => None,
                    }),
            );
        for endpoint in addresses {
            if let Endpoint::Frame {
                lifetime:
                    FrameLifetime::DuringCall {
                        boundary,
                        owner_return,
                    },
                ..
            } = endpoint
            {
                if boundary != owner_return
                    && reached.contains(&super::PrefixOccurrence {
                        event,
                        returned_at: Some(super::prefix::Completion::Owner(*owner_return)),
                    })
                {
                    returns.insert(*owner_return);
                }
            }
        }
    }
}

impl Flow<'_> {
    pub(super) fn check_prefix_receives(
        &mut self,
        summary: &Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        prefix: &Prefix,
    ) {
        for access in &summary.observed {
            let Some(receive) = receive_context(access) else {
                continue;
            };
            let snapshot = prefix.snapshots.get(receive);
            let memory_events =
                snapshot.map_or(prefix.memory.as_slice(), |writes| writes.memory.as_slice());
            let queue_events =
                snapshot.map_or(prefix.queues.as_slice(), |writes| writes.queues.as_slice());
            let reached = &receive.before;
            let mut returns = FxHashSet::default();
            let mut memory = Vec::new();
            let mut queues = Vec::new();
            for event in memory_events {
                if reached
                    .iter()
                    .any(|occurrence| occurrence.event == event.site)
                {
                    returned_owners(event.site, &event.write.value, reached, &mut returns);
                    memory.push(event.clone());
                }
            }
            for event in queue_events {
                if reached
                    .iter()
                    .any(|occurrence| occurrence.event == event.site)
                {
                    returned_owners(event.site, &event.write.value, reached, &mut returns);
                    queues.push(event.clone());
                }
            }
            if returns.is_empty() {
                continue;
            }
            let point = actuals.point;
            let mut state = actuals.read.state.clone();
            let mut replay = Self::new(self.function, self.table, self.program);
            replay.aliases = self.aliases.clone();
            replay.callables = self.callables.clone();
            replay
                .transfers
                .insert(point, self.transfers[&point].clone());
            replay.import_prefix_observations(&mut memory, &mut queues, point, &mut state);
            replay.settle_pending_tasks_in(point, &mut state, &memory, &queues, &[]);
            for event in &queues {
                replay.apply_queue_event(event, point, &mut state);
            }
            replay.invalid_reads.clear();
            let mut accesses = Accesses::default();
            replay.bind_access(
                access,
                function,
                CallActuals {
                    read: ReadView::current(&state),
                    ..actuals
                },
                &mut accesses,
            );
            replay.record_accesses(point, accesses.after_owner_returns(&returns), &state);
            self.invalid_reads.extend(replay.invalid_reads);
            self.requests.extend(replay.requests);
            self.transfers
                .get_mut(&point)
                .unwrap()
                .seeds
                .extend(replay.transfers.remove(&point).unwrap().seeds);
        }
    }
}
