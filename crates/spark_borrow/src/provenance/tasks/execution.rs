// TAG [SPARK-114]

//! Function-typed backing for the current callback of a task execution.

mod cleanup;

use spark_mir::{Local, Operand, Place, Rvalue};
use spark_types::TypeId;

use super::lifecycle::{CapturedOwner, Transfer};
use crate::provenance::{
    demand::{Coordinate, Storage},
    value, Flow, Point, State, Value,
};

impl State {
    pub(in crate::provenance) fn current_execution_record(
        &self,
        local: Local,
    ) -> Option<super::Record> {
        self.current_execution(local).map(|(_, record)| record)
    }

    pub(in crate::provenance) fn current_execution(
        &self,
        local: Local,
    ) -> Option<(Storage, super::Record)> {
        if !self.live.contains(&local) || self.invalidated_copies.contains(&local) {
            return None;
        }
        let peers = self.future_copies.witness(local);
        let mut facets = self.executions.iter().filter(|((anchor, _), _)| {
            peers.contains_local(*anchor)
                && self.live.contains(anchor)
                && !self.invalidated_copies.contains(anchor)
        });
        let (&(anchor, ty), callback) = facets.next()?;
        if facets.next().is_some() || !self.execution_backing_must.contains(&(anchor, ty)) {
            return None;
        }
        let storage = Storage::ExecutionCallback(anchor, ty);
        Some((
            storage.clone(),
            super::Record {
                callback: callback.clone().sourced(&storage),
                callback_ty: ty,
            },
        ))
    }

    pub(in crate::provenance) fn revoke_execution_backing(&mut self, local: Local) {
        let witness = self.future_copies.witness(local);
        self.execution_backing_must
            .retain(|(anchor, _)| !witness.contains_local(*anchor));
    }

    pub(in crate::provenance) fn install_execution_callback(
        &mut self,
        local: Local,
        ty: TypeId,
        callback: Value,
        complete: bool,
    ) {
        self.channel_copies.forget_execution(local, ty);
        if complete {
            self.execution_backing_must.insert((local, ty));
        } else {
            self.execution_backing_must.remove(&(local, ty));
        }
        self.executions.insert((local, ty), callback);
    }

    fn remove_execution_callback(&mut self, local: Local, ty: TypeId) -> Option<Value> {
        self.execution_backing_must.remove(&(local, ty));
        self.channel_copies.forget_execution(local, ty);
        self.executions.remove(&(local, ty))
    }

    pub(in crate::provenance) fn clobber_execution_callback(
        &mut self,
        local: Local,
        ty: TypeId,
        callback: Value,
    ) {
        self.revoke_execution_backing(local);
        self.input_owners.escape_execution(local, ty);
        self.future_owners.invalidate(local, &self.future_copies);
        self.future_owners.forget_execution(local, ty);
        self.future_copies.forget_execution(local, ty);
        self.invalidated_copies.insert(local);
        self.channel_copies.forget_execution(local, ty);
        self.executions.insert((local, ty), callback);
    }
}

pub(in crate::provenance) struct ExecutionTransfer {
    pub source: Local,
    callback_ty: TypeId,
    callback: Value,
    captures: Vec<CapturedOwner>,
    channels: Vec<crate::provenance::channels::identity::Captured>,
    tokens: Transfer,
    backing_complete: bool,
}

impl ExecutionTransfer {
    fn prepare(
        source: Local,
        destination: Local,
        ty: TypeId,
        callback: Value,
        state: &State,
        adopting: bool,
    ) -> Self {
        let captures = state
            .future_copies
            .anchored(source)
            .iter()
            .filter(|role| {
                if adopting {
                    role.callback_type().is_none()
                } else {
                    role.callback_type() == Some(ty)
                }
            })
            .filter_map(|role| {
                Some(CapturedOwner {
                    role: role.in_execution(destination, ty)?,
                    witness: state.future_copies.role_witness(role),
                    coverage: state.future_owners.coverage(role),
                })
            })
            .collect();
        Self {
            source,
            backing_complete: adopting || state.execution_backing_must.contains(&(source, ty)),
            callback_ty: ty,
            callback,
            captures,
            channels: crate::provenance::channels::identity::Captured::execution(
                source,
                destination,
                ty,
                adopting,
                state,
            ),
            tokens: Transfer::execution(source, destination, ty, &state.input_owners, adopting),
        }
    }

    pub fn restrict_coverage(&mut self, state: &State) {
        self.backing_complete &= state
            .execution_backing_must
            .contains(&(self.source, self.callback_ty));
        for capture in &mut self.captures {
            capture.coverage = capture.coverage.take().and_then(|coverage| {
                coverage.meet(state.future_owners.group_coverage(&capture.witness.peers)?)
            });
        }
    }

    pub fn visit_callback(&mut self, visit: &mut impl FnMut(&mut Value)) {
        visit(&mut self.callback);
    }

    pub fn detach(&self, flow: &mut Flow<'_>, point: Point, state: &mut State) {
        self.tokens.detach(&mut state.input_owners);
        flow.overwrite_completion_sources(
            &Coordinate {
                storage: Storage::ExecutionCallback(self.source, self.callback_ty),
                path: Vec::new(),
            },
            point,
        );
        if state
            .remove_execution_callback(self.source, self.callback_ty)
            .is_some()
        {
            flow.record_path_store(
                point,
                Coordinate {
                    storage: Storage::ExecutionCallback(self.source, self.callback_ty),
                    path: vec![],
                },
                &Value::default(),
                true,
            );
        }
        state
            .future_owners
            .forget_execution(self.source, self.callback_ty);
        state
            .future_copies
            .forget_execution(self.source, self.callback_ty);
    }

    pub fn valid(&self, clobbered: &[Local], state: &State) -> bool {
        !clobbered.contains(&self.source)
            && !state.invalidated_copies.contains(&self.source)
            && self.captures.iter().all(|capture| {
                capture
                    .witness
                    .peers
                    .iter()
                    .all(|peer| !peer.frame_clobbered(clobbered, &state.invalidated_copies))
            })
    }

    pub fn install(
        mut self,
        flow: &mut Flow<'_>,
        destination: Local,
        point: Point,
        state: &mut State,
    ) {
        for capture in &mut self.captures {
            capture.role = capture.role.reanchor(destination);
        }
        self.tokens.reanchor(destination);
        let backing_inputs: Vec<_> = self
            .captures
            .iter()
            .flat_map(|capture| capture.witness.peers.iter().filter_map(super::Role::local))
            .chain([self.source, destination])
            .collect();
        CapturedOwner::install_excluding(self.captures, &[self.source], state);
        self.tokens.install(&mut state.input_owners);
        if let Some(existing) = state.executions.get(&(destination, self.callback_ty)) {
            self.callback.join(
                &flow.freeze_sources(
                    point,
                    existing
                        .clone()
                        .sourced(&Storage::ExecutionCallback(destination, self.callback_ty)),
                ),
            );
        }
        flow.import_task_sources(&mut self.callback, point, state);
        flow.record_path_store(
            point,
            Coordinate {
                storage: Storage::ExecutionCallback(destination, self.callback_ty),
                path: vec![],
            },
            &self.callback,
            true,
        );
        self.callback.clear_sources();
        let complete = self.backing_complete
            && backing_inputs
                .iter()
                .all(|local| !state.invalidated_copies.contains(local));
        flow.overwrite_completion_sources(
            &Coordinate {
                storage: Storage::ExecutionCallback(destination, self.callback_ty),
                path: Vec::new(),
            },
            point,
        );
        state.install_execution_callback(destination, self.callback_ty, self.callback, complete);
        crate::provenance::channels::identity::Captured::install_all(
            self.channels,
            destination,
            &[self.source],
            state,
        );
    }
}

impl Flow<'_> {
    pub(in crate::provenance) fn current_role_value(
        &self,
        role: &super::Role,
        state: &State,
    ) -> Option<Value> {
        if !state.role_live(role) {
            return None;
        }
        let storage = role.storage();
        let ty = self.selected_type(self.storage_type(&storage), role.selection());
        if !matches!(
            self.table.resolve(value::strip_mutable(ty, self.table)),
            spark_types::Ty::Future { .. }
        ) {
            return None;
        }
        let mut current = state.stored_value(&storage)?.into_owned().sourced(&storage);
        for slot in role.selection() {
            current = current.project(slot);
        }
        Some(current)
    }

    pub(in crate::provenance) fn store_role_value(
        &mut self,
        role: &super::Role,
        mut value: Value,
        point: Point,
        state: &mut State,
    ) -> bool {
        value = self.freeze_sources(point, value);
        self.import_task_sources(&mut value, point, state);
        self.install_role_value(role, &value, point, state)
    }

    pub(super) fn install_role_value(
        &mut self,
        role: &super::Role,
        value: &Value,
        point: Point,
        state: &mut State,
    ) -> bool {
        let storage = role.storage();
        let Some(mut carrier) = state
            .stored_value(&storage)
            .map(std::borrow::Cow::into_owned)
        else {
            return false;
        };
        if let Some(local) = role.as_local() {
            self.record_store(point, &Place::from_local(local), value);
        } else {
            self.record_path_store(
                point,
                Coordinate {
                    storage: storage.clone(),
                    path: role.selection().to_vec(),
                },
                value,
                true,
            );
        }
        carrier.clear_path(role.selection());
        carrier.join(&value.prefixed_path(
            role.selection(),
            self.storage_type(&storage),
            self.table,
        ));
        carrier.clear_sources();
        Self::replace_task_carrier(&storage, carrier, state)
    }

    #[cfg(test)]
    pub(in crate::provenance) fn complete_owned_role(
        &mut self,
        role: &super::Role,
        result: Option<&Value>,
        point: Point,
        state: &mut State,
    ) {
        let Some(mut child) = self.current_role_value(role, state) else {
            return;
        };
        let ty = self.selected_type(self.storage_type(&role.storage()), role.selection());
        child.mark_future_completed();
        if let Some(result) = result {
            child.join(&result.prefixed(crate::provenance::Slot::Element, ty, self.table));
        }
        self.store_role_value(role, child, point, state);
    }

    pub(in crate::provenance) fn spawn_owner_transfer(
        &self,
        args: &[Operand],
        fresh_leaf_task: bool,
        state: &State,
    ) -> Option<ExecutionTransfer> {
        let Operand::Move(source) = args.first()? else {
            return None;
        };
        if !source.projections.is_empty() {
            return None;
        }
        let ty = self.place_type(source);
        let callback = state
            .value(source.local)
            .sourced(&Storage::Logical(source.local));
        let targets = self.callee_targets(&callback, ty);
        if targets.open || targets.unresolved || targets.known.len() != 1 {
            return None;
        }
        let transfer =
            ExecutionTransfer::prepare(source.local, source.local, ty, callback, state, true);
        (!fresh_leaf_task || !transfer.captures.is_empty()).then_some(transfer)
    }

    pub(in crate::provenance) fn moving_executions(
        &self,
        rvalue: &Rvalue,
        destination: &Place,
        point: Point,
        state: &State,
    ) -> Vec<ExecutionTransfer> {
        let Rvalue::Use(Operand::Move(source)) = rvalue else {
            return vec![];
        };
        if !source.projections.is_empty()
            || !destination.projections.is_empty()
            || value::strip_mutable(self.place_type(source), self.table)
                != value::strip_mutable(self.place_type(destination), self.table)
        {
            return vec![];
        }
        state
            .executions
            .iter()
            .filter(|((local, _), _)| *local == source.local)
            .map(|((_, ty), callback)| {
                let callback = self.freeze_sources(
                    point,
                    callback
                        .clone()
                        .sourced(&Storage::ExecutionCallback(source.local, *ty)),
                );
                ExecutionTransfer::prepare(
                    source.local,
                    destination.local,
                    *ty,
                    callback,
                    state,
                    false,
                )
            })
            .collect()
    }

    pub(in crate::provenance) fn retire_rvalue_executions(
        &mut self,
        rvalue: &Rvalue,
        point: Point,
        state: &mut State,
    ) {
        match rvalue {
            Rvalue::Use(Operand::Move(place))
            | Rvalue::Cast {
                operand: Operand::Move(place),
                ..
            }
            | Rvalue::UnaryOp(_, Operand::Move(place)) => {
                self.retire_execution_owner(place.local, point, state);
            }
            Rvalue::Aggregate(_, operands) | Rvalue::StringInterp(operands) => {
                for operand in operands {
                    if let Operand::Move(place) = operand {
                        self.retire_execution_owner(place.local, point, state);
                    }
                }
            }
            _ => {}
        }
    }

    pub(in crate::provenance) fn retire_execution_owner(
        &mut self,
        local: Local,
        point: Point,
        state: &mut State,
    ) {
        let peer = state
            .future_copies
            .witness(local)
            .locals()
            .filter(|peer| *peer != local && state.live.contains(peer))
            .min_by_key(|peer| peer.0);
        let callbacks: Vec<_> = state
            .executions
            .iter()
            .filter(|((anchor, _), _)| *anchor == local)
            .map(|((_, ty), callback)| (*ty, callback.clone()))
            .collect();
        for (ty, callback) in callbacks {
            let callback = self.freeze_sources(
                point,
                callback.sourced(&Storage::ExecutionCallback(local, ty)),
            );
            if let Some(peer) = peer {
                let transfer = ExecutionTransfer::prepare(local, peer, ty, callback, state, false);
                transfer.detach(self, point, state);
                transfer.install(self, peer, point, state);
            } else {
                self.demote_execution(local, ty, &callback, point, state);
            }
            self.record_path_store(
                point,
                Coordinate {
                    storage: Storage::ExecutionCallback(local, ty),
                    path: vec![],
                },
                &Value::default(),
                true,
            );
        }
    }

    fn demote_execution(
        &mut self,
        local: Local,
        ty: TypeId,
        callback: &Value,
        point: Point,
        state: &mut State,
    ) {
        state.future_owners.invalidate(local, &state.future_copies);
        state.input_owners.escape_execution(local, ty);
        state.future_owners.forget_execution(local, ty);
        state.future_copies.forget_execution(local, ty);
        self.preserve_pending_loss(callback, &vec![], point, state, true);
        self.overwrite_completion_sources(
            &Coordinate {
                storage: Storage::ExecutionCallback(local, ty),
                path: Vec::new(),
            },
            point,
        );
        state.remove_execution_callback(local, ty);
    }

    pub(in crate::provenance) fn demote_completed_execution(
        &mut self,
        local: Local,
        point: Point,
        state: &mut State,
    ) {
        let callbacks: Vec<_> = state
            .executions
            .iter()
            .filter(|((anchor, _), _)| *anchor == local)
            .map(|((_, ty), callback)| (*ty, callback.clone()))
            .collect();
        for (ty, callback) in callbacks {
            let callback = self.freeze_sources(
                point,
                callback.sourced(&Storage::ExecutionCallback(local, ty)),
            );
            self.demote_execution(local, ty, &callback, point, state);
            self.record_path_store(
                point,
                Coordinate {
                    storage: Storage::ExecutionCallback(local, ty),
                    path: vec![],
                },
                &Value::default(),
                true,
            );
        }
    }
}
