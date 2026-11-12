// TAG [SPARK-114]

use rustc_hash::FxHashSet;
use spark_mir::Local;
use spark_types::{Ty, TypeId};

use crate::provenance::{
    demand::{Coordinate, Storage},
    tasks::{Record, Role, Witness},
    value, Flow, Point, Slot, State, Value,
};

struct CaptureRelease {
    role: Role,
    witness: Witness,
    clean: bool,
}

struct ExecutionCleanup {
    local: Local,
    captures: Vec<CaptureRelease>,
}

impl Flow<'_> {
    fn clean_execution_captures(
        &mut self,
        local: Local,
        ty: TypeId,
        callback: &Value,
        point: Point,
        state: &State,
    ) -> FxHashSet<Slot> {
        let current = state.value(local);
        let mut clean: Option<FxHashSet<Slot>> = None;
        for handle in current.futures.get(&Vec::new()).into_iter().flatten() {
            let Ty::Future { inner } = self
                .table
                .resolve(value::strip_mutable(handle.target.ty(), self.table))
            else {
                return FxHashSet::default();
            };
            let facts = self.task_facts(
                &Record {
                    callback: callback.clone(),
                    callback_ty: ty,
                },
                &handle.target,
                point,
                state,
                *inner,
            );
            if let Some(clean) = &mut clean {
                clean.retain(|slot| facts.clean_captures.contains(slot));
            } else {
                clean = Some(facts.clean_captures);
            }
        }
        clean.unwrap_or_default()
    }

    fn prepare_completed_execution_cleanup(
        &mut self,
        parent: &Witness,
        point: Point,
        state: &State,
    ) -> Vec<ExecutionCleanup> {
        let roots: FxHashSet<_> = parent.locals().collect();
        let mut prepared = Vec::new();
        for (&(local, ty), callback) in &state.executions {
            if !roots.contains(&local) {
                continue;
            }
            let clean = if state.live.contains(&local) && !state.invalidated_copies.contains(&local)
            {
                self.clean_execution_captures(local, ty, callback, point, state)
            } else {
                FxHashSet::default()
            };
            let captures = state
                .future_copies
                .anchored(local)
                .into_iter()
                .filter(|role| role.callback_type() == Some(ty))
                .map(|role| {
                    let witness = state.future_copies.role_witness(&role);
                    let eligible = role.selection().len() == 1
                        && clean.contains(&role.selection()[0])
                        && self.current_role_value(&role, state).is_some()
                        && witness.peers.iter().all(|peer| {
                            state.live.contains(&peer.local())
                                && !state.invalidated_copies.contains(&peer.local())
                                && state.future_owners.known_role(peer)
                                && peer.as_local().is_none_or(|peer| !roots.contains(&peer))
                        });
                    CaptureRelease {
                        role,
                        witness,
                        clean: eligible,
                    }
                })
                .collect();
            prepared.push(ExecutionCleanup { local, captures });
        }
        prepared
    }

    pub(in crate::provenance) fn cleanup_completed_executions(
        &mut self,
        parent: &Witness,
        point: Point,
        state: &mut State,
    ) {
        let prepared = self.prepare_completed_execution_cleanup(parent, point, state);
        for execution in &prepared {
            for capture in &execution.captures {
                if !capture.clean {
                    state
                        .future_owners
                        .revoke(&capture.witness, &state.future_copies);
                }
            }
        }
        for execution in &prepared {
            for capture in &execution.captures {
                if !capture.clean {
                    continue;
                }
                let witness = state.future_copies.role_witness(&capture.role);
                if !witness
                    .peers
                    .iter()
                    .all(|peer| state.future_owners.known_role(peer))
                {
                    continue;
                }
                if witness.peers.len() == 1 {
                    self.complete_future_witness(witness, None, point, state);
                }
                self.release_execution_capture(&capture.role, point, state);
            }
        }
        for execution in prepared {
            // Demote only the remaining slots; released owners must not reappear as activity.
            self.demote_completed_execution(execution.local, point, state);
        }
    }

    fn release_execution_capture(&mut self, role: &Role, point: Point, state: &mut State) {
        let Some(ty) = role.callback_type() else {
            return;
        };
        let storage = Storage::ExecutionCallback(role.local(), ty);
        let Some(callback) = state.executions.get_mut(&(role.local(), ty)) else {
            return;
        };
        callback.clear_path(role.selection());
        self.record_path_store(
            point,
            Coordinate {
                storage,
                path: role.selection().to_vec(),
            },
            &Value::default(),
            true,
        );
        state.input_owners.drop_role(role);
        state.future_owners.forget_role(role);
        state.future_copies.forget_role(role);
    }
}
