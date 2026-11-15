// TAG [SPARK-114]

//! Reserve source-backed pending outputs before importing a spawned producer's prefix.

use super::{Aliases, CallActuals, Flow, Key, Operand, Point, State, Value};
use crate::provenance::{
    channels::{
        counts::Count,
        identity::Role as QueueRole,
        messages::{Contribution, Slot},
        protocol::{Entry, Promise},
        QueueEvent, Target,
    },
    demand::{Coordinate, Storage},
    tasks::{fresh_outputs::Recipe, Record},
};
use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::{Local, Place};
use spark_types::TypeId;

pub(in crate::provenance) struct Reservation {
    entry: Entry,
    recipe: Recipe,
    producer: crate::provenance::tasks::Target,
    value: Value,
    pub events: Vec<QueueEvent>,
    reserved: bool,
}

impl Reservation {
    fn slot(&self) -> Slot {
        Slot {
            queue: self.entry.queue.clone(),
            site: self.recipe.send,
            ty: self.recipe.birth.future_ty,
        }
    }

    pub fn partition(&mut self, events: &mut Vec<QueueEvent>) {
        events.retain(|event| {
            if event.site == self.recipe.send
                && event.write.target == self.entry.target
                && event.write.ty == self.recipe.birth.future_ty
            {
                self.value.join(&event.write.value);
                self.events.push(event.clone());
                false
            } else {
                true
            }
        });
    }

    pub fn visit_values(&mut self, visit: &mut impl FnMut(&mut Value)) {
        visit(&mut self.value);
        for event in &mut self.events {
            visit(&mut event.write.value);
        }
    }

    pub fn reserve(&mut self, flow: &mut Flow<'_>, point: Point, state: &mut State) -> bool {
        let current = state.channel_copies.witness(&self.entry.queue);
        if self.events.is_empty()
            || current.peers != self.entry.witness.peers
            || !current.fresh
            || current.output.is_some()
            || current
                .peers
                .iter()
                .any(|peer| state.invalidated_copies.contains(&peer.local()))
            || state.queue_promises.contains_key(&self.slot())
            || state.exposed_queues.contains(&self.entry.target)
            || state
                .queue_entries()
                .any(|(target, _, _, _)| *target == self.entry.target)
            || !flow.enumerated_queue_aliases(&current, &self.entry.target, state)
            || self.recipe.birth.record(&self.value).is_none()
        {
            flow.import_queue_contents(
                &self.entry.target,
                &self.value,
                self.recipe.birth.future_ty,
                point,
                state,
            );
            return false;
        }
        let slot = self.slot();
        flow.record_path_store(
            point,
            Coordinate {
                storage: Storage::QueueCurrent(slot.clone()),
                path: Vec::new(),
            },
            &self.value,
            true,
        );
        let mut value = self.value.clone();
        value.clear_sources();
        state.queue_current.insert(
            slot.clone(),
            Contribution {
                targets: FxHashSet::from_iter([self.entry.target.clone()]),
                value,
                births: FxHashSet::from_iter([self.recipe.birth]),
            },
        );
        state.queue_promises.insert(
            slot.clone(),
            Promise {
                recipe: self.recipe.clone(),
                queue: self.entry.target.clone(),
                producer: self.producer.clone(),
                present: false,
            },
        );
        state
            .channel_copies
            .reserve_output(&self.entry.queue, &slot);
        state
            .reached_prefix
            .extend(self.events.iter().map(|event| super::PrefixOccurrence {
                event: event.site,
                returned_at: None,
            }));
        self.reserved = true;
        true
    }

    pub fn finish(
        self,
        flow: &mut Flow<'_>,
        destination: &Place,
        clobbered: &[Local],
        point: Point,
        state: &mut State,
    ) {
        if !self.reserved {
            return;
        }
        let slot = self.slot();
        let current = state.channel_copies.witness(&self.entry.queue);
        let valid = destination.projections.is_empty()
            && state.live.contains(&self.entry.queue.local())
            && !clobbered.contains(&destination.local)
            && !state.invalidated_copies.contains(&destination.local)
            && !self.entry.witness.peers.iter().any(|peer| {
                clobbered.contains(&peer.local())
                    || state.invalidated_copies.contains(&peer.local())
            })
            && current.fresh
            && current.output.as_ref() == Some(&slot)
            && state.current_execution_record(destination.local).is_some()
            && state
                .value(destination.local)
                .futures
                .get(&Vec::new())
                .is_some_and(|handles| {
                    handles.len() == 1
                        && handles.iter().all(|handle| {
                            handle.pending
                                && handle.capture.is_none()
                                && handle.target == self.producer
                        })
                })
            && state
                .queue_promises
                .get(&slot)
                .is_some_and(|promise| promise.producer == self.producer)
            && state
                .queue_current
                .get(&slot)
                .is_some_and(|message| self.recipe.birth.record(&message.value).is_some());
        if valid && !state.exposed_queues.contains(&self.entry.target) {
            // Execution adoption transports the captured queue after the reservation snapshot.
            state
                .channel_copies
                .reserve_output(&self.entry.queue, &slot);
            state.future_copies.reserve_output(destination.local, &slot);
        } else {
            flow.cancel_queue_promise(&slot, point, state);
        }
    }
}

impl Flow<'_> {
    pub(super) fn spawn_queue_outputs(
        &mut self,
        argument: &Operand,
        record: &Record,
        producer: &crate::provenance::tasks::Target,
        ty: TypeId,
        point: Point,
        state: &State,
    ) -> Vec<Reservation> {
        let (Operand::Copy(place) | Operand::Move(place)) = argument else {
            return Vec::new();
        };
        if !place.projections.is_empty() {
            return Vec::new();
        }
        let targets = self.callee_targets(&record.callback, record.callback_ty);
        if targets.open || targets.unresolved || targets.known.len() != 1 {
            return Vec::new();
        }
        let Some(context) = self.program else {
            return Vec::new();
        };
        let Some(function) = targets
            .known
            .iter()
            .next()
            .and_then(|def| context.mir.functions.get(def))
        else {
            return Vec::new();
        };
        if function.param_count != 0 {
            return Vec::new();
        }
        let actuals = CallActuals {
            callee: &record.callback,
            callee_place: Some(place),
            arguments: &[],
            ty,
            point,
            instantiation_site: producer.instantiation_site(),
            frame_caller: None,
            read: crate::provenance::observations::ReadView::current(state),
            aliases: None,
        };
        let roots = &context.ports[&function.def_id];
        let locations = self.call_locations(roots, function, actuals);
        let mut common: Option<FxHashMap<Recipe, Reservation>> = None;
        for aliases in Aliases::call_variants(roots, self.table, &locations, &self.aliases) {
            let key = Key {
                function: function.def_id,
                aliases,
                callables: self.callback_inputs(function, actuals),
            };
            let Some(summary) = context.summaries.get(&key) else {
                self.requests.insert(key);
                return Vec::new();
            };
            let Some(certificate) = context
                .proofs
                .and_then(|proofs| proofs.get(&key))
                .and_then(crate::provenance::tasks::proofs::Proof::certificate)
            else {
                return Vec::new();
            };
            let actuals = CallActuals {
                aliases: Some(&key.aliases),
                ..actuals
            };
            let candidates =
                self.bind_spawn_recipes(summary, function, actuals, certificate, producer);
            if let Some(common) = &mut common {
                common.retain(|recipe, output| {
                    candidates.get(recipe).is_some_and(|candidate| {
                        if candidate.entry.target != output.entry.target
                            || candidate.entry.witness.peers != output.entry.witness.peers
                        {
                            return false;
                        }
                        output.value.join(&candidate.value);
                        true
                    })
                });
            } else {
                common = Some(candidates);
            }
        }
        common
            .into_iter()
            .flat_map(FxHashMap::into_values)
            .collect()
    }
    fn bind_spawn_recipes(
        &mut self,
        summary: &super::Summary,
        function: &spark_mir::MirFunction,
        actuals: CallActuals<'_>,
        certificate: &crate::provenance::tasks::proofs::Certificate,
        producer: &crate::provenance::tasks::Target,
    ) -> FxHashMap<Recipe, Reservation> {
        let counts = self.bind_normal_queue_sends(summary, function, actuals);
        let mut candidates = FxHashMap::default();
        for (recipe, payload) in &summary.fresh_outputs {
            if !certificate.fresh_messages.contains(recipe)
                || !certificate.prefix_messages.contains(recipe)
            {
                continue;
            }
            let Some(queue) =
                super::fresh_output_binding::output_queue(recipe, function, actuals, &[])
            else {
                continue;
            };
            let Some(mut entry) = self.empty_queue_entry(&queue, actuals.read.state) else {
                continue;
            };
            let Some(root) = entry
                .witness
                .peers
                .iter()
                .filter(|peer| matches!(peer, QueueRole::Local(_)))
                .min_by_key(|peer| peer.local().index())
                .cloned()
            else {
                continue;
            };
            entry.queue = root;
            if counts.get(&entry.target) != Count::ONE {
                continue;
            }
            let Some(source) = Target::from_port(&recipe.queue) else {
                continue;
            };
            let bound: FxHashSet<_> = self
                .bind_queue_target(&source, function, actuals)
                .iter()
                .flat_map(|target| self.queue_targets(target))
                .collect();
            if bound != FxHashSet::from_iter([entry.target.clone()]) {
                continue;
            }
            let value = self.bind(
                payload,
                function,
                CallActuals {
                    ty: recipe.birth.future_ty,
                    ..actuals
                },
            );
            if recipe.birth.record(&value).is_none() {
                continue;
            }
            candidates.insert(
                recipe.clone(),
                Reservation {
                    entry,
                    recipe: recipe.clone(),
                    producer: producer.clone(),
                    value,
                    events: Vec::new(),
                    reserved: false,
                },
            );
        }
        candidates
    }
}
