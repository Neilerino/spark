// TAG [SPARK-114]

//! Bind candidates as MAY data; only a checked normal/prefix certificate transfers a copy.

use crate::provenance::tasks::fresh_outputs::Recipe;
use crate::provenance::{
    channels::{
        counts::Count,
        identity::{Role as QueueRole, Witness as QueueWitness},
        messages::{Contribution, Slot},
        QueueEvent, Target, Write,
    },
    demand::{Coordinate, Storage},
    effects::{CallActuals, Summary},
    tasks::{lifecycle::Coverage, Live, Role, Witness},
    Flow, Point, State, Value,
};
use rustc_hash::FxHashSet;
use spark_mir::{Local, MirFunction, Operand};

pub(in crate::provenance) struct Bound {
    pub recipe: Recipe,
    queue: QueueRole,
    witness: QueueWitness,
    moved: FxHashSet<Local>,
    target: Target,
    pub value: Value,
    pub events: Vec<QueueEvent>,
    snapshots: Vec<Write>,
}

impl Bound {
    pub fn partition(&mut self, events: &mut Vec<QueueEvent>, snapshots: &mut Vec<Write>) {
        let matches =
            |target: &Target, ty| *target == self.target && ty == self.recipe.birth.future_ty;
        events.retain(|event| {
            if event.site == self.recipe.send && matches(&event.write.target, event.write.ty) {
                self.events.push(event.clone());
                false
            } else {
                true
            }
        });
        snapshots.retain(|write| {
            if matches(&write.target, write.ty) {
                self.snapshots.push(write.clone());
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
        for write in &mut self.snapshots {
            visit(&mut write.value);
        }
    }

    fn current_queue(&self, state: &State) -> Option<QueueRole> {
        if self
            .witness
            .peers
            .iter()
            .any(|peer| state.invalidated_copies.contains(&peer.local()))
        {
            return None;
        }
        let live: FxHashSet<_> = self
            .witness
            .peers
            .iter()
            .filter(|peer| state.live.contains(&peer.local()))
            .cloned()
            .collect();
        let valid = |role: &QueueRole| {
            let current = state.channel_copies.witness(role);
            current.input.as_ref() == Some(&self.recipe.queue) && current.peers == live
        };
        if live.contains(&self.queue) && valid(&self.queue) {
            return Some(self.queue.clone());
        }
        live.iter().find(|role| valid(role)).cloned().or_else(|| {
            (live.is_empty()
                && self
                    .witness
                    .peers
                    .iter()
                    .all(|role| self.moved.contains(&role.local())))
            .then(|| self.queue.clone())
        })
    }

    pub fn install(
        mut self,
        flow: &mut Flow<'_>,
        point: Point,
        clobbered: &[Local],
        state: &mut State,
    ) {
        if self
            .witness
            .peers
            .iter()
            .any(|peer| clobbered.contains(&peer.local()))
        {
            self.demote(flow, point, state);
            return;
        }
        flow.import_task_sources(&mut self.value, point, state);
        let Some(queue) = self.current_queue(state) else {
            self.demote(flow, point, state);
            return;
        };
        let slot = Slot {
            queue,
            site: self.recipe.send,
            ty: self.recipe.birth.future_ty,
        };
        let occupied = state.queue_current.contains_key(&slot);
        flow.archive_previous_message(&slot, point, state);
        if self.current_queue(state).is_none() || self.recipe.birth.record(&self.value).is_none() {
            self.demote(flow, point, state);
            return;
        }
        flow.record_queue_send(&self.target, &self.value, point, state);
        flow.record_path_store(
            point,
            Coordinate {
                storage: Storage::QueueCurrent(slot.clone()),
                path: Vec::new(),
            },
            &self.value,
            true,
        );
        self.value.clear_sources();
        state.queue_current.insert(
            slot.clone(),
            Contribution {
                targets: FxHashSet::from_iter([self.target]),
                value: self.value,
                births: FxHashSet::from_iter([self.recipe.birth]),
            },
        );
        if !occupied {
            let role = Role::message(slot);
            state.future_copies.assign_role(
                role.clone(),
                Witness {
                    peers: FxHashSet::default(),
                    inputs: FxHashSet::default(),
                    birth: Some(self.recipe.birth),
                    produces: FxHashSet::default(),
                },
                &Live::new(&state.live, &state.queue_current),
            );
            state.future_owners.install_role(
                role,
                Coverage::ThroughQueue(self.recipe.queue),
                &state.future_copies,
            );
        }
    }

    fn demote(self, flow: &mut Flow<'_>, point: Point, state: &mut State) {
        for event in self.events {
            flow.apply_queue_event(&event, point, state);
        }
        for write in self.snapshots {
            flow.import_queue_contents(&write.target, &write.value, write.ty, point, state);
        }
        flow.import_queue_contents(
            &self.target,
            &self.value,
            self.recipe.birth.future_ty,
            point,
            state,
        );
    }
}

impl Flow<'_> {
    pub(super) fn bind_fresh_outputs(
        &mut self,
        summary: &Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        key: &crate::provenance::aliases::Key,
        args: &[Operand],
    ) -> Vec<Bound> {
        let mut bound = Vec::new();
        for (recipe, payload) in &summary.fresh_outputs {
            let Some(queue) = output_queue(recipe, function, actuals, args) else {
                continue;
            };
            let witness = actuals.read.state.channel_copies.witness(&queue);
            let Some(premise) = witness.input.clone() else {
                continue;
            };
            let Some(source) = Target::from_port(&recipe.queue) else {
                continue;
            };
            let targets: FxHashSet<_> = self
                .bind_queue_target(&source, function, actuals)
                .iter()
                .flat_map(|target| self.queue_targets(target))
                .collect();
            let Some(target) = targets
                .iter()
                .next()
                .cloned()
                .filter(|_| targets.len() == 1)
            else {
                continue;
            };
            let value = self.bind(
                payload,
                function,
                CallActuals {
                    ty: recipe.birth.future_ty,
                    ..actuals
                },
            );
            let mapped = Recipe {
                queue: premise,
                send: recipe.send,
                birth: recipe.birth,
            };
            self.fresh_output_candidates
                .entry(mapped.clone())
                .or_default()
                .join(&value);
            let proven = self
                .program
                .and_then(|context| context.proofs)
                .and_then(|proofs| proofs.get(key))
                .and_then(crate::provenance::tasks::proofs::Proof::certificate)
                .is_some_and(|certificate| {
                    certificate.fresh_messages.contains(recipe)
                        && certificate.prefix_messages.contains(recipe)
                });
            if proven
                && self
                    .bind_normal_queue_sends(summary, function, actuals)
                    .get(&target)
                    == Count::ONE
                && recipe.birth.record(&value).is_some()
            {
                bound.push(Bound {
                    recipe: mapped,
                    queue,
                    witness,
                    moved: args
                        .iter()
                        .filter_map(|arg| {
                            if let Operand::Move(place) = arg {
                                Some(place.local)
                            } else {
                                None
                            }
                        })
                        .collect(),
                    target,
                    value,
                    events: Vec::new(),
                    snapshots: Vec::new(),
                });
            }
        }
        bound
    }
}

pub(super) fn output_queue(
    recipe: &Recipe,
    function: &MirFunction,
    actuals: CallActuals<'_>,
    args: &[Operand],
) -> Option<QueueRole> {
    if !recipe.queue.selection.is_empty()
        || recipe.queue.queue.is_some()
        || recipe.queue.channel_capture.is_some()
    {
        return None;
    }
    Some(if recipe.queue.input.0 <= function.param_count {
        let Some(Operand::Copy(place) | Operand::Move(place)) = recipe
            .queue
            .input
            .index()
            .checked_sub(1)
            .and_then(|index| args.get(index))
        else {
            return None;
        };
        if !place.projections.is_empty() {
            return None;
        }
        QueueRole::Local(place.local)
    } else {
        let place = actuals.callee_place?;
        if !place.projections.is_empty() {
            return None;
        }
        QueueRole::Capture {
            local: place.local,
            capture: crate::provenance::channels::Capture {
                owner: function.def_id,
                index: recipe.queue.input.index() - function.param_count as usize - 1,
                ty: function.locals[recipe.queue.input.index()].ty,
            },
            callback_ty: None,
        }
    })
}
