// TAG [SPARK-114]

//! Function-typed backing for the current callback of a task execution.

use spark_mir::{Local, Operand, Place, Rvalue};
use spark_types::TypeId;

use super::lifecycle::{CapturedOwner, Transfer};
use crate::provenance::{
    demand::{Coordinate, Storage},
    value, Flow, Point, State, Value,
};

impl State {
    pub(in crate::provenance) fn clobber_execution_callback(
        &mut self,
        local: Local,
        ty: TypeId,
        callback: Value,
    ) {
        self.input_owners.escape_execution(local, ty);
        self.future_owners.invalidate(local, &self.future_copies);
        self.future_owners.forget_execution(local, ty);
        self.future_copies.forget_execution(local, ty);
        self.invalidated_copies.insert(local);
        self.executions.insert((local, ty), callback);
    }
}

pub(in crate::provenance) struct ExecutionTransfer {
    pub source: Local,
    callback_ty: TypeId,
    callback: Value,
    captures: Vec<CapturedOwner>,
    tokens: Transfer,
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
                    closed: state.future_owners.known_role(role),
                })
            })
            .collect();
        Self {
            source,
            callback_ty: ty,
            callback,
            captures,
            tokens: Transfer::execution(source, destination, ty, &state.input_owners, adopting),
        }
    }

    pub fn restrict_coverage(&mut self, state: &State) {
        for capture in &mut self.captures {
            capture.closed &= capture
                .witness
                .peers
                .iter()
                .all(|peer| state.future_owners.known_role(peer));
        }
    }

    pub fn visit_callback(&mut self, visit: &mut impl FnMut(&mut Value)) {
        visit(&mut self.callback);
    }

    pub fn detach(&self, flow: &mut Flow<'_>, point: Point, state: &mut State) {
        self.tokens.detach(&mut state.input_owners);
        if state
            .executions
            .remove(&(self.source, self.callback_ty))
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
                capture.witness.peers.iter().all(|peer| {
                    !clobbered.contains(&peer.local())
                        && !state.invalidated_copies.contains(&peer.local())
                })
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
        self.callback.sources.clear();
        state
            .executions
            .insert((destination, self.callback_ty), self.callback);
    }
}

impl Flow<'_> {
    pub(in crate::provenance) fn current_role_value(
        &self,
        role: &super::Role,
        state: &State,
    ) -> Option<Value> {
        if !state.live.contains(&role.local()) {
            return None;
        }
        let storage = role.callback_type().map_or_else(
            || Storage::Logical(role.local()),
            |ty| Storage::ExecutionCallback(role.local(), ty),
        );
        let ty = self.selected_type(self.storage_type(&storage), role.selection());
        if !matches!(
            self.table.resolve(value::strip_mutable(ty, self.table)),
            spark_types::Ty::Future { .. }
        ) {
            return None;
        }
        let mut current = state.stored_value(&storage)?.clone().sourced(&storage);
        for slot in role.selection() {
            current = current.project(slot);
        }
        Some(current)
    }

    pub(in crate::provenance) fn complete_owned_capture(
        &mut self,
        role: &super::Role,
        result: Option<&Value>,
        point: Point,
        state: &mut State,
    ) {
        let Some(crate::provenance::Slot::Capture(_, _, child_ty)) = role.selection().last() else {
            return;
        };
        let storage = role.callback_type().map_or_else(
            || Storage::Logical(role.local()),
            |ty| Storage::ExecutionCallback(role.local(), ty),
        );
        let Some(mut callback) = state.stored_value(&storage).cloned() else {
            return;
        };
        let Some(mut child) = self.current_role_value(role, state) else {
            return;
        };
        child.mark_future_completed();
        if let Some(result) = result {
            child.join(&result.prefixed(crate::provenance::Slot::Element, *child_ty, self.table));
        }
        child = self.freeze_sources(point, child);
        self.import_task_sources(&mut child, point, state);
        self.record_path_store(
            point,
            Coordinate {
                storage: storage.clone(),
                path: role.selection().to_vec(),
            },
            &child,
            true,
        );
        callback.clear_path(role.selection());
        callback.join(&child.prefixed_path(
            role.selection(),
            self.storage_type(&storage),
            self.table,
        ));
        callback.sources.clear();
        if let Some(ty) = role.callback_type() {
            state.executions.insert((role.local(), ty), callback);
        } else {
            state.set(role.local(), callback);
        }
    }

    pub(in crate::provenance) fn spawn_owner_transfer(
        &self,
        args: &[Operand],
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
        (!transfer.captures.is_empty()).then_some(transfer)
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
        state.executions.remove(&(local, ty));
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
