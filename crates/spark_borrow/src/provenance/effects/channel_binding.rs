// TAG [SPARK-114]

use super::super::channels::{Handle, Target};
use super::{CallActuals, Flow, Intent, MirFunction, Path, Selector, Value};
use crate::provenance::tasks::proofs::CallProof;
use rustc_hash::{FxHashMap, FxHashSet};

impl Flow<'_> {
    pub(super) fn normalize_fresh_queue_roles(&self, summary: &mut super::Summary) {
        use super::super::channels::OutputRole;
        let mut pending = self.queue_output_roots(summary);
        let mut seen = FxHashSet::default();
        let mut bindings = FxHashMap::default();
        while let Some((target, role)) = pending.pop() {
            if !seen.insert(target.clone()) {
                continue;
            }
            if !matches!(target, Target::Input { .. } | Target::Validation { .. }) {
                bindings.insert(target.clone(), target.with_output_role(role.clone()));
            }
            for ((stored, _), value) in &summary.queue_contents {
                if *stored != target {
                    continue;
                }
                for (path, handles) in &value.queues {
                    for handle in handles {
                        pending.push((
                            handle.target.clone(),
                            OutputRole {
                                storage: role.storage.clone(),
                                ty: role.ty,
                                selection: super::value::normalize(
                                    role.ty,
                                    [
                                        role.selection.clone(),
                                        vec![super::Slot::Element],
                                        path.clone(),
                                    ]
                                    .concat(),
                                    self.table,
                                ),
                                capture: handle.capture.or(role.capture),
                            },
                        ));
                    }
                }
            }
        }
        remap_handles(&mut summary.returned, &bindings);
        for record in summary.tasks.values_mut() {
            remap_handles(&mut record.callback, &bindings);
        }
        for value in summary
            .memory
            .values_mut()
            .chain(summary.prefix_memory.values_mut())
            .chain(summary.obligations.values_mut())
        {
            remap_handles(value, &bindings);
        }
        for ((target, ty), mut value) in std::mem::take(&mut summary.queue_contents) {
            remap_handles(&mut value, &bindings);
            let target = bindings.get(&target).cloned().unwrap_or(target);
            summary
                .queue_contents
                .entry((target, ty))
                .or_default()
                .join(&value);
        }
        for ((site, target, ty), mut value) in std::mem::take(&mut summary.queues) {
            remap_handles(&mut value, &bindings);
            let target = bindings.get(&target).cloned().unwrap_or(target);
            summary
                .queues
                .entry((site, target, ty))
                .or_default()
                .join(&value);
        }
    }

    fn queue_output_roots(
        &self,
        summary: &super::Summary,
    ) -> Vec<(Target, super::super::channels::OutputRole)> {
        use super::super::channels::{OutputRole, OutputStorage};
        let mut pending: Vec<_> = summary
            .returned
            .queues
            .iter()
            .flat_map(|(path, handles)| {
                handles.iter().map(move |handle| {
                    (
                        handle.target.clone(),
                        OutputRole {
                            storage: OutputStorage::Return,
                            ty: self.function.return_type,
                            selection: path.clone(),
                            capture: handle.capture,
                        },
                    )
                })
            })
            .collect();
        for (port, value) in &summary.memory {
            for (path, handles) in &value.queues {
                pending.extend(handles.iter().map(|handle| {
                    (
                        handle.target.clone(),
                        OutputRole {
                            storage: OutputStorage::Memory(port.clone()),
                            ty: port.pointee,
                            selection: path.clone(),
                            capture: handle.capture,
                        },
                    )
                }));
            }
        }
        for ((_, view), value) in &summary.prefix_memory {
            for (path, handles) in &value.queues {
                pending.extend(handles.iter().map(|handle| {
                    (
                        handle.target.clone(),
                        OutputRole {
                            storage: OutputStorage::Memory(view.root.clone()),
                            ty: view.root.pointee,
                            selection: super::value::normalize(
                                view.root.pointee,
                                [view.path.clone(), path.clone()].concat(),
                                self.table,
                            ),
                            capture: handle.capture,
                        },
                    )
                }));
            }
        }
        for ((_, target, ty), value) in &summary.queues {
            let storage = if let Some(port) = target.port(self.table) {
                OutputStorage::Queue(port)
            } else if let Target::Unknown { site, .. } = target {
                OutputStorage::Opaque(*site, target.ty())
            } else {
                continue;
            };
            for (path, handles) in &value.queues {
                pending.extend(handles.iter().map(|handle| {
                    (
                        handle.target.clone(),
                        OutputRole {
                            storage: storage.clone(),
                            ty: *ty,
                            selection: path.clone(),
                            capture: handle.capture,
                        },
                    )
                }));
            }
        }
        pending
    }

    pub(super) fn export_queue_contents(&self, state: &super::State, summary: &mut super::Summary) {
        for (target, value) in &state.queues {
            summary
                .queue_contents
                .entry(target.clone())
                .or_default()
                .join(&super::export_value(value, self.function.def_id));
        }
    }

    pub(super) fn opaque_queue_outcome(
        &mut self,
        mut returned: Value,
        args: &[spark_mir::Operand],
        actuals: CallActuals<'_>,
    ) -> super::CallOutcome {
        use super::super::channels::{opaque_handles, Write};
        let site = super::super::address::OpaqueSite {
            function: self.function.def_id,
            point: actuals.point,
        };
        returned.join(&opaque_handles(actuals.ty, site, self.table));
        returned.join(&super::super::tasks::opaque_handles(
            actuals.ty, site, self.table, false,
        ));
        for handles in returned.queues.values_mut() {
            let expected: Vec<_> = handles.iter().map(Handle::ty).collect();
            for ty in expected {
                for (argument, operand) in actuals.arguments.iter().zip(args) {
                    let reachable = self.reachable_queue_payloads(argument, actuals.read.state);
                    let selected = self.tail_channel_value(
                        reachable,
                        super::super::operand_type(operand, self.function),
                        ty,
                        actuals.read.state,
                    );
                    handles.extend(
                        selected
                            .queues
                            .into_values()
                            .flatten()
                            .map(|handle| Handle::new(handle.target)),
                    );
                }
            }
        }
        let mut pending: Vec<_> = returned
            .queues
            .values()
            .flatten()
            .map(|handle| handle.target.clone())
            .collect();
        let mut seen = FxHashSet::default();
        let mut queue_contents = Vec::new();
        while let Some(target) = pending.pop() {
            if !seen.insert(target.clone()) {
                continue;
            }
            let Target::Unknown { ty, .. } = &target else {
                continue;
            };
            let spark_types::Ty::Channel { inner } = self.table.resolve(*ty) else {
                continue;
            };
            let inner = *inner;
            let mut payload = self.opaque_result(
                args,
                CallActuals {
                    ty: inner,
                    ..actuals
                },
            );
            payload.join(&opaque_handles(inner, site, self.table));
            pending.extend(
                payload
                    .queues
                    .values()
                    .flatten()
                    .map(|handle| handle.target.clone()),
            );
            queue_contents.push(Write {
                target,
                ty: inner,
                value: payload,
            });
        }
        super::CallOutcome {
            fresh_leaf_task: false,
            spawn_owners: None,
            returned_owner_tags: crate::provenance::tasks::lifecycle::Tags::default(),
            retained_captures: Vec::new(),
            returned_callbacks: crate::provenance::tasks::outputs::CallbackOutputs::default(),
            residuals: Vec::new(),
            async_memory: Vec::new(),
            tasks: Vec::new(),
            completed_task: None,
            clobbered_copies: Vec::new(),
            future_proof: CallProof::default(),
            returned_prefix: rustc_hash::FxHashSet::default(),
            publications: crate::provenance::effects::PrefixWrites::default(),
            place_writes: Vec::new(),
            prefix_memory: Vec::new(),
            prefix_residuals: Vec::new(),
            returned,
            memory: Vec::new(),
            queues: Vec::new(),
            queue_contents,
        }
    }

    pub(super) fn bind_queue_contents(
        &mut self,
        summary: &super::Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> Vec<super::super::channels::Write> {
        let mut result = Vec::new();
        for ((target, ty), value) in &summary.queue_contents {
            let value = self.bind(value, function, CallActuals { ty: *ty, ..actuals });
            for target in self.bind_queue_target(target, function, actuals) {
                result.push(super::super::channels::Write {
                    target,
                    ty: *ty,
                    value: value.clone(),
                });
            }
        }
        result
    }

    pub(super) fn export_queue_writes(&self) -> FxHashMap<super::super::channels::QueueKey, Value> {
        self.queue_sends
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    super::export_value(value, self.function.def_id),
                )
            })
            .collect()
    }

    pub(super) fn bind_queue_writes(
        &mut self,
        writes: &rustc_hash::FxHashMap<crate::provenance::channels::QueueKey, Value>,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        key: &super::Key,
        args: Option<&[spark_mir::Operand]>,
    ) -> Vec<super::super::channels::QueueEvent> {
        use super::super::channels::{QueueEvent, Write};
        let mut queues = Vec::new();
        for ((site, target, ty), payload) in writes {
            let bound = self.bind(payload, function, CallActuals { ty: *ty, ..actuals });
            for target in self.bind_queue_target(target, function, actuals) {
                queues.push(QueueEvent {
                    site: *site,
                    fence: self.event_fence(
                        key,
                        &super::EventKey::Queue(*site),
                        args,
                        actuals.read.state,
                    ),
                    write: Write {
                        target,
                        ty: *ty,
                        value: bound.clone(),
                    },
                });
            }
        }
        queues
    }

    pub(super) fn bind_queue_target(
        &self,
        target: &Target,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> FxHashSet<Target> {
        let site = actuals.instantiation_site(self.function.def_id);
        match target {
            Target::Input {
                local,
                path,
                ty,
                queue,
                capture,
            } => {
                let selector = Selector {
                    channel_capture: *capture,
                    queue: queue.clone(),
                    input: *local,
                    selection: path.clone(),
                    intent: Intent::Value,
                    expected: Some(*ty),
                    capture: None,
                };
                let selected = self.select_input_value(&selector, function, actuals);
                selected.queues.get(&Vec::new()).map_or_else(
                    || {
                        FxHashSet::from_iter([Target::Unknown {
                            site,
                            ty: *ty,
                            constructor: None,
                        }])
                    },
                    |handles| handles.iter().map(|handle| handle.target.clone()).collect(),
                )
            }
            Target::Allocation {
                ty,
                function,
                point,
                constructor,
            } => FxHashSet::from_iter([Target::Allocation {
                ty: *ty,
                function: site.function,
                point: site.point,
                constructor: Some(constructor.clone().unwrap_or(
                    super::super::channels::Constructor {
                        site: super::super::address::OpaqueSite {
                            function: *function,
                            point: *point,
                        },
                        role: None,
                    },
                )),
            }]),
            Target::Validation { ty, selection, .. } => {
                FxHashSet::from_iter([Target::Validation {
                    site,
                    ty: *ty,
                    selection: selection.clone(),
                }])
            }
            Target::Unknown {
                site: source_site,
                ty,
                constructor,
            } => FxHashSet::from_iter([Target::Unknown {
                site,
                ty: *ty,
                constructor: Some(constructor.clone().unwrap_or(
                    super::super::channels::Constructor {
                        site: *source_site,
                        role: None,
                    },
                )),
            }]),
        }
    }

    pub(super) fn bind_queue_handles(
        &self,
        value: &Value,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> FxHashMap<Path, FxHashSet<Handle>> {
        value
            .queues
            .iter()
            .map(|(path, targets)| {
                (
                    path.clone(),
                    targets
                        .iter()
                        .flat_map(|handle| {
                            self.bind_queue_target(&handle.target, function, actuals)
                                .into_iter()
                                .map(move |target| Handle {
                                    target,
                                    capture: handle.capture,
                                })
                        })
                        .collect(),
                )
            })
            .collect()
    }
}

fn remap_handles(value: &mut Value, bindings: &FxHashMap<Target, Target>) {
    super::super::channels::remap_queues(&mut value.queues, bindings);
    for view in value
        .task_views
        .values_mut()
        .flat_map(|subjects| subjects.values_mut())
    {
        for callback in view.callbacks.values_mut() {
            callback.remap_queues(bindings);
        }
    }
}
