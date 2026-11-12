// TAG [SPARK-114]

//! Entry checks for reserving an exclusive current output, before a producer starts.

use super::{
    counts::Count,
    identity::{Role, Witness},
    Target,
};
use crate::provenance::{Flow, Slot, State, Value};
use rustc_hash::{FxHashMap, FxHashSet};

pub(in crate::provenance) struct Entry {
    pub queue: Role,
    pub witness: Witness,
    pub target: Target,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::provenance) struct Promise {
    pub recipe: crate::provenance::tasks::fresh_outputs::Recipe,
    pub queue: Target,
    pub producer: crate::provenance::tasks::Target,
}

pub(in crate::provenance) type Promises = FxHashMap<super::messages::Slot, Promise>;

pub(in crate::provenance) fn intersect(into: &mut Promises, other: &Promises) -> bool {
    let before = into.len();
    into.retain(|slot, promise| other.get(slot) == Some(promise));
    into.len() != before
}

impl Flow<'_> {
    pub(in crate::provenance) fn empty_queue_entry(
        &self,
        role: &Role,
        state: &State,
    ) -> Option<Entry> {
        let witness = state.channel_copies.witness(role);
        if !witness.fresh
            || witness.output.is_some()
            || !state.live.contains(&role.local())
            || state.invalidated_copies.contains(&role.local())
        {
            return None;
        }
        if witness.peers.iter().any(|peer| {
            !state.live.contains(&peer.local())
                || state.invalidated_copies.contains(&peer.local())
                || !state.channel_copies.witness(peer).fresh
                || state.channel_copies.witness(peer).peers != witness.peers
        }) {
            return None;
        }
        let value = match role {
            Role::Local(local) => state.value(*local),
            Role::Capture {
                local,
                capture,
                callback_ty: None,
            } => state.value(*local).project(&Slot::Capture(
                capture.owner,
                capture.index,
                capture.ty,
            )),
            Role::Capture {
                callback_ty: Some(_),
                ..
            } => return None,
        };
        let targets: FxHashSet<_> = value
            .root_queue_handles()
            .flat_map(|handle| self.queue_targets(&handle.target))
            .collect();
        let target = targets
            .into_iter()
            .next()
            .filter(|_| value.root_queue_handles().count() == 1)?;
        if !matches!(target, Target::Allocation { .. })
            || !state.queue_accesses.allows(&target)
            || state.exposed_queues.contains(&target)
            || state.queue_send_counts.get(&target) != Count::ZERO
            || state
                .queue_entries()
                .any(|(stored, _, _, _)| *stored == target)
        {
            return None;
        }
        if !self.enumerated_queue_aliases(&witness, &target, state) {
            return None;
        }
        Some(Entry {
            queue: role.clone(),
            witness,
            target,
        })
    }

    pub(in crate::provenance) fn enumerated_queue_aliases(
        &self,
        witness: &Witness,
        target: &Target,
        state: &State,
    ) -> bool {
        self.queue_aliases_in(witness, target, None, state)
    }

    fn queue_aliases_in(
        &self,
        witness: &Witness,
        target: &Target,
        output: Option<&super::messages::Slot>,
        state: &State,
    ) -> bool {
        for local in &state.live {
            let value = state.value(*local);
            if self.hidden_queue_reference(self.function.locals[local.index()].ty, &value)
                || self.queue_accesses(&value, state).unknown
                || !self.enumerated_handles(&value, *local, None, witness, target)
            {
                return false;
            }
            for (storage, callback) in
                value.callback_storage_values(&crate::provenance::demand::Storage::Logical(*local))
            {
                if self.queue_accesses(&callback, state).allows(target) {
                    continue;
                }
                let Some((anchor, ty)) =
                    output.and_then(|slot| state.protocol_callback(&storage, slot))
                else {
                    return false;
                };
                if !self.enumerated_handles(&callback, anchor, Some(ty), witness, target) {
                    return false;
                }
            }
        }
        state
            .rooted_activity()
            .into_carriers()
            .all(|(storage, value)| {
                if matches!(storage, crate::provenance::demand::Storage::Logical(_)) {
                    return true;
                }
                if self.hidden_queue_reference(self.storage_type(&storage), &value) {
                    return false;
                }
                if self.queue_accesses(&value, state).allows(target) {
                    return true;
                }
                let Some((anchor, ty)) =
                    output.and_then(|slot| state.protocol_callback(&storage, slot))
                else {
                    return false;
                };
                !self.queue_accesses(&value, state).unknown
                    && self.enumerated_handles(&value, anchor, Some(ty), witness, target)
                    && value
                        .callback_storage_values(&storage)
                        .iter()
                        .all(|(_, callback)| self.queue_accesses(callback, state).allows(target))
            })
    }

    fn enumerated_handles(
        &self,
        value: &Value,
        local: spark_mir::Local,
        callback_ty: Option<spark_types::TypeId>,
        witness: &Witness,
        target: &Target,
    ) -> bool {
        value.queues.iter().all(|(path, handles)| {
            if !handles
                .iter()
                .any(|handle| self.queue_targets(&handle.target).contains(target))
            {
                return true;
            }
            let role = match path.as_slice() {
                [] if callback_ty.is_none() => Role::Local(local),
                [Slot::Capture(owner, index, ty)] => Role::Capture {
                    local,
                    capture: super::Capture {
                        owner: *owner,
                        index: *index,
                        ty: *ty,
                    },
                    callback_ty,
                },
                _ => return false,
            };
            witness.peers.contains(&role)
        })
    }

    fn hidden_queue_reference(&self, ty: spark_types::TypeId, value: &Value) -> bool {
        if matches!(
            self.table
                .resolve(crate::provenance::value::strip_mutable(ty, self.table)),
            spark_types::Ty::Error
        ) {
            // MIR intrinsic function addresses have no data type or closure environment.
            if value.callees.get(&Vec::new()).is_some_and(|targets| {
                targets.known.iter().any(|def| {
                    self.program
                        .and_then(|context| context.definitions)
                        .and_then(|definitions| definitions.get(*def))
                        .is_some_and(|info| {
                            matches!(
                                info.kind,
                                spark_resolve::DefinitionKind::BuiltinFunction
                                    | spark_resolve::DefinitionKind::BuiltinMethod
                            )
                        })
                        && *value == Value::function(*def)
                })
            }) {
                return false;
            }
        }
        super::access::may_reference_queue(ty, self.table, &mut FxHashSet::default())
    }
}

impl Flow<'_> {
    pub(in crate::provenance) fn cancel_queue_promise(
        &mut self,
        slot: &super::messages::Slot,
        point: crate::provenance::Point,
        state: &mut State,
    ) {
        if state.queue_promises.remove(slot).is_none() {
            return;
        }
        state.channel_copies.forget_output(slot);
        state.future_copies.forget_output(slot);
        self.archive_previous_message(slot, point, state);
    }

    pub(in crate::provenance) fn poison_queue_promises(
        &mut self,
        access: &super::access::Accesses,
        allowed: &FxHashSet<super::messages::Slot>,
        sends: &super::counts::Sends,
        point: crate::provenance::Point,
        state: &mut State,
    ) {
        let slots: Vec<_> = state
            .queue_promises
            .iter()
            .filter(|(slot, promise)| {
                (!allowed.contains(*slot) && !access.allows(&promise.queue))
                    || sends.get(&promise.queue) != Count::ZERO
            })
            .map(|(slot, _)| slot.clone())
            .collect();
        for slot in slots {
            self.cancel_queue_promise(&slot, point, state);
        }
    }
}

impl Flow<'_> {
    pub(in crate::provenance) fn route_producer_queue_event(
        &mut self,
        storage: &crate::provenance::demand::Storage,
        path: &[Slot],
        handle: &crate::provenance::tasks::Handle,
        event: &mut super::QueueEvent,
        point: crate::provenance::Point,
        state: &mut State,
    ) -> bool {
        use crate::provenance::demand::{Coordinate, Storage};
        let (Storage::Logical(local) | Storage::Cell(local)) = storage else {
            return false;
        };
        if !path.is_empty() || handle.capture.is_some() {
            return false;
        }
        event.write.value = self.freeze_sources(point, std::mem::take(&mut event.write.value));
        self.import_task_sources(&mut event.write.value, point, state);
        let current = state.value(*local);
        if state.current_execution_record(*local).is_none()
            || !current
                .futures
                .get(&Vec::new())
                .is_some_and(|handles| handles.len() == 1 && handles.contains(handle))
        {
            return false;
        }
        for slot in state.future_copies.produces(*local) {
            let Some(promise) = state.queue_promises.get(&slot).cloned() else {
                continue;
            };
            if promise.producer != handle.target
                || promise.recipe.send != event.site
                || slot.ty != event.write.ty
                || promise.queue != event.write.target
                || state.current_output_queue(&slot).is_none()
                || promise.recipe.birth.record(&event.write.value).is_none()
                || !state.queue_current.contains_key(&slot)
            {
                continue;
            }
            self.record_queue_event(event);
            self.record_path_store(
                point,
                Coordinate {
                    storage: Storage::QueueCurrent(slot.clone()),
                    path: Vec::new(),
                },
                &event.write.value,
                false,
            );
            let Some(message) = state.queue_current.get_mut(&slot) else {
                return false;
            };
            message.value.join(&event.write.value);
            message.value.clear_sources();
            state
                .reached_prefix
                .insert(crate::provenance::effects::PrefixOccurrence {
                    event: event.site,
                    returned_at: None,
                });
            return true;
        }
        false
    }
}

impl Flow<'_> {
    pub(in crate::provenance) fn validate_queue_promises(
        &mut self,
        point: crate::provenance::Point,
        state: &mut State,
    ) {
        let invalid: Vec<_> = state
            .queue_promises
            .iter()
            .filter(|(slot, promise)| {
                let Some(role) = state.current_output_queue(slot) else {
                    return true;
                };
                let witness = state.channel_copies.witness(&role);
                !state.future_copies.producer_roles(slot).iter().any(|role| {
                    state.role_live(role) && !role.frame_clobbered(&[], &state.invalidated_copies)
                }) || state
                    .queue_current
                    .get(*slot)
                    .is_none_or(|message| promise.recipe.birth.record(&message.value).is_none())
                    || !self.queue_aliases_in(&witness, &promise.queue, Some(slot), state)
            })
            .map(|(slot, _)| slot.clone())
            .collect();
        for slot in invalid {
            self.cancel_queue_promise(&slot, point, state);
        }
    }
}

impl State {
    fn protocol_producer(
        &self,
        local: spark_mir::Local,
        output: &super::messages::Slot,
    ) -> Option<(spark_mir::Local, spark_types::TypeId)> {
        use crate::provenance::demand::Storage;
        if !self.future_copies.produces(local).contains(output) {
            return None;
        }
        let promise = self.queue_promises.get(output)?;
        let current = self.value(local);
        if !current.futures.get(&Vec::new()).is_some_and(|handles| {
            handles.len() == 1
                && handles
                    .iter()
                    .all(|handle| handle.target == promise.producer && handle.capture.is_none())
        }) {
            return None;
        }
        let (Storage::ExecutionCallback(anchor, ty), _) = self.current_execution(local)? else {
            return None;
        };
        Some((anchor, ty))
    }

    fn protocol_callback(
        &self,
        storage: &crate::provenance::demand::Storage,
        output: &super::messages::Slot,
    ) -> Option<(spark_mir::Local, spark_types::TypeId)> {
        use crate::provenance::demand::Storage;
        match storage {
            Storage::ExecutionCallback(local, ty) => self
                .protocol_producer(*local, output)
                .filter(|(anchor, found)| anchor == local && found == ty),
            Storage::CallbackNode(location)
                if location.node().is_none()
                    && location.path().is_empty()
                    && location.actor().capture.is_none() =>
            {
                let (Storage::Logical(local) | Storage::Cell(local)) = location.root() else {
                    return None;
                };
                let promise = self.queue_promises.get(output)?;
                if location.actor().target != promise.producer {
                    return None;
                }
                self.protocol_producer(*local, output)
                    .filter(|(_, ty)| *ty == location.callback_type())
            }
            _ => None,
        }
    }

    pub(in crate::provenance) fn current_output_queue(
        &self,
        slot: &super::messages::Slot,
    ) -> Option<Role> {
        let promise = self.queue_promises.get(slot)?;
        if self.exposed_queues.contains(&promise.queue)
            || self
                .queues
                .keys()
                .any(|(target, _)| *target == promise.queue)
        {
            return None;
        }
        let roles = self.channel_copies.output_roles(slot);
        let role = roles
            .iter()
            .filter(|role| matches!(role, Role::Local(_)))
            .min_by_key(|role| role.local().index())?
            .clone();
        let witness = self.channel_copies.witness(&role);
        if witness.peers != roles
            || roles.iter().any(|peer| {
                !self.live.contains(&peer.local())
                    || self.invalidated_copies.contains(&peer.local())
                    || !self.channel_copies.witness(peer).fresh
            })
        {
            return None;
        }
        let value = self.value(role.local());
        let handles: Vec<_> = value.root_queue_handles().collect();
        (handles.len() == 1 && handles[0].target == promise.queue).then_some(role)
    }
}
