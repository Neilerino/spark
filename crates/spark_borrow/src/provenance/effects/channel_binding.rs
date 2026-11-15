// TAG [SPARK-114]

use super::super::channels::{Handle, Target};
use super::{CallActuals, Flow, Intent, MirFunction, Path, Selector, Value};
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
        if let Some(accesses) = &mut summary.normal_queue_accesses {
            accesses.remap(&bindings);
        }
        if let Some(counts) = &mut summary.normal_queue_sends {
            counts.remap(&bindings);
        }
        remap_handles(&mut summary.returned, &bindings);
        for record in summary.tasks.values_mut() {
            remap_handles(&mut record.callback, &bindings);
        }
        for value in summary
            .memory
            .values_mut()
            .chain(summary.fresh_outputs.values_mut())
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
        for (event, value) in &summary.prefix_memory {
            let view = &event.view;
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

    pub(super) fn export_queue_state(&self, state: &super::State, summary: &mut super::Summary) {
        crate::provenance::channels::counts::join_returns(
            &mut summary.normal_queue_sends,
            Some(&state.queue_send_counts),
        );
        crate::provenance::channels::access::join_returns(
            &mut summary.normal_queue_accesses,
            Some(&state.queue_accesses),
        );
        for (target, ty, _, value) in state.queue_entries() {
            summary
                .queue_contents
                .entry((target.clone(), ty))
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
            queue_accesses: if self.queue_free_builtin(args, actuals) {
                crate::provenance::channels::access::Accesses::default()
            } else {
                crate::provenance::channels::access::Accesses::unknown()
            },
            queue_send_counts: crate::provenance::channels::counts::Sends {
                unknown: !self.queue_free_builtin(args, actuals),
                ..Default::default()
            },
            returned,
            queue_contents,
            ..Default::default()
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

impl Flow<'_> {
    pub(super) fn bind_normal_queue_sends(
        &self,
        summary: &super::Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> crate::provenance::channels::counts::Sends {
        use crate::provenance::channels::counts::{Count, Sends};
        let Some(counts) = &summary.normal_queue_sends else {
            return Sends {
                unknown: true,
                ..Sends::default()
            };
        };
        let mut bound = Sends {
            unknown: counts.unknown,
            ..Sends::default()
        };
        for target in counts.targets.keys() {
            let count = counts.get(target);
            let targets: FxHashSet<_> = self
                .bind_queue_target(target, function, actuals)
                .iter()
                .flat_map(|target| self.queue_targets(target))
                .collect();
            if targets.is_empty() {
                bound.unknown = true;
            }
            let count = if targets.len() == 1 {
                count
            } else {
                count.union(Count::ZERO)
            };
            for target in targets {
                bound.unknown |=
                    matches!(target, Target::Unknown { .. } | Target::Validation { .. });
                bound.add(target, count);
            }
        }
        bound
    }
}

impl Flow<'_> {
    pub(super) fn bind_queue_accesses(
        &self,
        summary: &super::Summary,
        function: &MirFunction,
        actuals: super::CallActuals<'_>,
        context: &super::Key,
        args: &[spark_mir::Operand],
    ) -> crate::provenance::channels::access::Accesses {
        use crate::provenance::channels::access::Accesses;
        let Some(access) = &summary.normal_queue_accesses else {
            return Accesses::unknown();
        };
        let mut bound = Accesses {
            unknown: access.unknown,
            ..Accesses::default()
        };
        for target in &access.blocked {
            let targets: FxHashSet<_> = self
                .bind_queue_target(target, function, actuals)
                .iter()
                .flat_map(|target| self.queue_targets(target))
                .collect();
            bound.unknown |= targets.is_empty()
                || targets.iter().any(|target| {
                    matches!(target, Target::Unknown { .. } | Target::Validation { .. })
                });
            bound.blocked.extend(targets);
        }
        let certificate = self
            .program
            .and_then(|program| program.proofs)
            .and_then(|proofs| proofs.get(context))
            .and_then(crate::provenance::tasks::proofs::Proof::certificate);
        for input in &access.deferred_future_inputs {
            let operand = input
                .index()
                .checked_sub(1)
                .filter(|_| input.0 <= function.param_count)
                .and_then(|index| args.get(index));
            if let Some(operand) =
                operand.filter(|_| certificate.is_some_and(|proof| proof.completed.contains(input)))
            {
                bound.join(&self.awaited_queue_accesses(operand, actuals.read.state));
            } else {
                bound.unknown = true;
            }
        }
        bound
    }
}
