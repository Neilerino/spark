// TAG [SPARK-114]

//! Preserve a sent copy under an explicit queue premise, without cleanup authority.

use super::Slot;
use crate::provenance::{
    aliases::Port,
    channels::identity::{Role as ChannelRole, Witness as ChannelWitness},
    tasks::{fresh::Birth, lifecycle::Coverage, Live, Role, Witness},
    Flow, State, Value,
};
use rustc_hash::FxHashSet;
use spark_mir::{Local, Operand};
use std::sync::Arc;

pub(super) struct Transfer {
    receiver: Local,
    channel: ChannelWitness,
    premise: Arc<Port>,
    source: Local,
    moved: bool,
    birth: Birth,
    witness: Witness,
    prepared: bool,
}

impl Transfer {
    pub fn snapshot(
        flow: &Flow<'_>,
        receiver: Local,
        payload: &Operand,
        state: &State,
    ) -> Option<Self> {
        let (Operand::Copy(source) | Operand::Move(source)) = payload else {
            return None;
        };
        if !source.projections.is_empty() {
            return None;
        }
        let channel = state.channel_copies.witness(&ChannelRole::Local(receiver));
        let premise = channel.input.clone()?;
        let birth = flow.current_leaf_birth(source.local, state)?;
        let witness = state.future_copies.witness(source.local);
        if birth.site.function != flow.function.def_id
            || !state
                .input_owners
                .tags(&Role::root(source.local))
                .is_empty()
            || state.future_owners.group_coverage(&witness.peers) != Some(Coverage::Closed)
        {
            return None;
        }
        Some(Self {
            receiver,
            channel,
            premise,
            source: source.local,
            moved: matches!(payload, Operand::Move(_)),
            birth,
            witness,
            prepared: false,
        })
    }

    fn channel_valid(&self, state: &State) -> bool {
        let current = state
            .channel_copies
            .witness(&ChannelRole::Local(self.receiver));
        current.input.as_ref() == Some(&self.premise)
            && current.peers == self.channel.peers
            && current.peers.iter().all(|peer| {
                state.live.contains(&peer.local())
                    && !state.invalidated_copies.contains(&peer.local())
            })
    }

    pub fn prepare(
        &mut self,
        events: &[super::super::QueueEvent],
        clobbered: &[Local],
        state: &State,
    ) {
        let consistent = events.first().is_some_and(|first| {
            events
                .iter()
                .all(|event| event.site == first.site && event.write.ty == self.birth.future_ty)
        });
        let current = state.future_copies.witness(self.source);
        self.prepared = consistent
            && self.channel_valid(state)
            && !clobbered.contains(&self.receiver)
            && current.birth == Some(self.birth)
            && current.peers == self.witness.peers
            && state.future_owners.group_coverage(&current.peers) == Some(Coverage::Closed)
            && current.peers.iter().all(|peer| {
                state.role_live(peer) && !peer.frame_clobbered(clobbered, &state.invalidated_copies)
            });
    }

    pub const fn reserves_move(&self) -> bool {
        self.prepared && self.moved
    }

    pub fn abandon(&self, state: &mut State) {
        if self.prepared {
            state
                .future_owners
                .revoke(&self.witness, &state.future_copies);
        }
    }

    pub fn install(&self, slot: &Slot, payload: &Value, state: &mut State) {
        if !self.prepared {
            return;
        }
        let survivors: FxHashSet<_> = self
            .witness
            .peers
            .iter()
            .filter(|peer| !(self.moved && peer.as_local() == Some(self.source)))
            .cloned()
            .collect();
        let valid = self.channel_valid(state)
            && self.birth.record(payload).is_some()
            && self.witness.peers.iter().all(|peer| {
                !peer.frame_clobbered(&[], &state.invalidated_copies)
                    && !state.future_owners.exposed_role(peer)
            })
            && survivors.iter().all(|peer| {
                state.role_live(peer) && state.future_copies.role_peers(peer) == survivors
            });
        if !valid {
            self.abandon(state);
            return;
        }
        let role = Role::message(slot.clone());
        state.future_copies.assign_role(
            role.clone(),
            Witness {
                peers: survivors,
                inputs: self.witness.inputs.clone(),
                birth: Some(self.birth),
                produces: self.witness.produces.clone(),
            },
            &Live::new(&state.live, &state.queue_current),
        );
        for peer in state.future_copies.role_peers(&role) {
            state.future_owners.install_role(
                peer,
                Coverage::ThroughQueue(self.premise.clone()),
                &state.future_copies,
            );
        }
    }
}
