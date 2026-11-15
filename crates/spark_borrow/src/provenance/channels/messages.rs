// TAG [SPARK-114]

//! Source-backed send contributions; presence grants no receipt or ownership proof.

mod ownership;

use crate::provenance::{address::OpaqueSite, demand::Storage, Flow, Point, State, Value};
use rustc_hash::FxHashSet;
use spark_intern::{kw, Symbol};
use spark_mir::{Local, Operand};
use spark_types::TypeId;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) struct Slot {
    pub queue: super::identity::Role,
    pub site: OpaqueSite,
    pub ty: TypeId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::provenance) struct Contribution {
    pub targets: FxHashSet<super::Target>,
    pub value: Value,
    /// MAY birth shapes from actual sends, not owner or producer coverage.
    pub births: FxHashSet<crate::provenance::tasks::fresh::Birth>,
}

impl Contribution {
    pub fn join(&mut self, other: &Self) -> bool {
        let before = self.targets.len();
        self.targets.extend(other.targets.iter().cloned());
        let mut changed = before != self.targets.len();
        changed |= self.value.join(&other.value);
        let before = self.births.len();
        self.births.extend(other.births.iter().copied());
        changed |= before != self.births.len();
        changed
    }
}

pub(in crate::provenance) struct Send {
    receiver: Local,
    owner: Option<ownership::Transfer>,
    birth: Option<(crate::provenance::tasks::fresh::Birth, FxHashSet<Local>)>,
}

impl Send {
    #[cfg(test)]
    fn untracked(receiver: Local) -> Self {
        Self {
            receiver,
            owner: None,
            birth: None,
        }
    }

    pub(in crate::provenance) fn prepare(
        &mut self,
        events: &[super::QueueEvent],
        clobbered: &[Local],
        state: &State,
    ) {
        if let Some(owner) = &mut self.owner {
            owner.prepare(events, clobbered, state);
        }
    }
    pub(in crate::provenance) fn reserves_move(&self) -> bool {
        self.owner
            .as_ref()
            .is_some_and(ownership::Transfer::reserves_move)
    }
    fn abandon(&self, state: &mut State) {
        if let Some(owner) = &self.owner {
            owner.abandon(state);
        }
    }

    fn birth(
        &self,
        state: &State,
        payload: &Value,
    ) -> Option<crate::provenance::tasks::fresh::Birth> {
        let (birth, peers) = self.birth.as_ref()?;
        if peers
            .iter()
            .any(|peer| state.invalidated_copies.contains(peer))
            || birth.record(payload).is_none()
        {
            return None;
        }
        Some(*birth)
    }
}

pub(super) fn direct_send(
    flow: &Flow<'_>,
    method: Symbol,
    args: &[Operand],
    state: &State,
) -> Option<Send> {
    let (Operand::Copy(receiver) | Operand::Move(receiver)) = args.first()? else {
        return None;
    };
    if method != kw::SEND || !receiver.projections.is_empty() {
        return None;
    }
    let birth = args.get(1).and_then(|payload| {
        let (Operand::Copy(source) | Operand::Move(source)) = payload else {
            return None;
        };
        if !source.projections.is_empty() {
            return None;
        }
        Some((
            flow.current_leaf_birth(source.local, state)?,
            state
                .future_copies
                .witness(source.local)
                .peers
                .iter()
                .filter_map(crate::provenance::tasks::Role::local)
                .collect(),
        ))
    });
    Some(Send {
        receiver: receiver.local,
        owner: args.get(1).and_then(|payload| {
            ownership::Transfer::snapshot(flow, receiver.local, payload, state)
        }),
        birth,
    })
}

impl State {
    pub(in crate::provenance) fn queue_entries(
        &self,
    ) -> impl Iterator<Item = (&super::Target, TypeId, Storage, &Value)> {
        self.queues
            .iter()
            .map(|((target, ty), value)| (target, *ty, Storage::Queue(target.clone(), *ty), value))
            .chain(self.queue_current.iter().flat_map(|(slot, message)| {
                message.targets.iter().map(move |target| {
                    (
                        target,
                        slot.ty,
                        Storage::QueueCurrent(slot.clone()),
                        &message.value,
                    )
                })
            }))
    }
}

impl Flow<'_> {
    pub(in crate::provenance) fn apply_direct_queue_send(
        &mut self,
        send: &Send,
        events: &[super::QueueEvent],
        point: Point,
        state: &mut State,
    ) {
        let Some(first) = events.first() else {
            send.abandon(state);
            return;
        };
        if events
            .iter()
            .any(|event| event.site != first.site || event.write.ty != first.write.ty)
        {
            send.abandon(state);
            for event in events {
                self.apply_queue_event(event, point, state);
            }
            return;
        }
        self.ensure_transfer(point, state);
        let mut message = Contribution {
            targets: FxHashSet::default(),
            value: Value::default(),
            births: FxHashSet::default(),
        };
        for event in events {
            self.record_queue_event(event);
            state
                .reached_prefix
                .insert(crate::provenance::effects::PrefixOccurrence {
                    event: event.site,
                    returned_at: None,
                });
            message
                .targets
                .extend(self.queue_targets(&event.write.target));
            message.value.join(&event.write.value);
        }
        message.value = self.freeze_sources(point, message.value);
        self.import_task_sources(&mut message.value, point, state);
        let slot = Slot {
            queue: super::identity::Role::Local(send.receiver),
            site: first.site,
            ty: first.write.ty,
        };
        let occupied = state.queue_current.contains_key(&slot);
        self.archive_previous_message(&slot, point, state);
        if occupied {
            send.abandon(state);
        }
        if !state.live.contains(&send.receiver) || state.invalidated_copies.contains(&send.receiver)
        {
            send.abandon(state);
            for target in message.targets {
                self.queue_send_one(target, &message.value, slot.ty, point, state);
            }
            return;
        }
        for target in &message.targets {
            self.record_queue_send(target, &message.value, point, state);
            if target.may_outlive_function() || state.exposed_queues.contains(target) {
                self.expose_queue_handles(&message.value, state);
            }
        }
        self.record_path_store(
            point,
            crate::provenance::demand::Coordinate {
                storage: Storage::QueueCurrent(slot.clone()),
                path: Vec::new(),
            },
            &message.value,
            true,
        );
        message.births.extend(send.birth(state, &message.value));
        message.value.clear_sources();
        let payload = message.value.clone();
        state.queue_current.insert(slot.clone(), message);
        if !occupied {
            if let Some(owner) = &send.owner {
                owner.install(&slot, &payload, state);
            }
        }
    }

    pub(in crate::provenance) fn archive_previous_message(
        &mut self,
        slot: &Slot,
        point: Point,
        state: &mut State,
    ) {
        self.overwrite_completion_sources(
            &crate::provenance::demand::Coordinate {
                storage: Storage::QueueCurrent(slot.clone()),
                path: Vec::new(),
            },
            point,
        );
        state.queue_promises.remove(slot);
        state.channel_copies.forget_output(slot);
        state.future_copies.forget_output(slot);
        let Some(previous) = state.queue_current.get(slot).cloned() else {
            return;
        };
        let mut value = self.freeze_sources(
            point,
            previous.value.sourced(&Storage::QueueCurrent(slot.clone())),
        );
        self.import_task_sources(&mut value, point, state);
        for target in previous.targets {
            self.queue_send_one(target, &value, slot.ty, point, state);
        }
        self.record_path_store(
            point,
            crate::provenance::demand::Coordinate {
                storage: Storage::QueueCurrent(slot.clone()),
                path: Vec::new(),
            },
            &Value::default(),
            true,
        );
        let role = crate::provenance::tasks::Role::message(slot.clone());
        let witness = state.future_copies.role_witness(&role);
        state.future_owners.revoke(&witness, &state.future_copies);
        state.future_owners.forget_role(&role);
        state.future_copies.forget_role(&role);
        state.queue_current.remove(slot);
    }
}

#[cfg(test)]
mod tests;
