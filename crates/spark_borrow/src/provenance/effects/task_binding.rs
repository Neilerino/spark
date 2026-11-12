// TAG [SPARK-114]

use crate::provenance::tasks::proofs::CallProof;
use spark_intern::kw;
use spark_resolve::DefinitionKind;
use spark_types::Ty;

use super::super::tasks::{Handle, Record, Target};
use super::{
    Aliases, CallActuals, CallOutcome, Flow, FxHashMap, FxHashSet, Intent, Key, MirFunction,
    Operand, Point, Selector, Slot, TypeId, Value,
};

pub(super) enum TaskDispatch {
    Returns(Vec<CallOutcome>),
    NoReturn(super::PrefixWrites),
}

#[derive(Default)]
pub(in crate::provenance) struct TaskFacts {
    pub returned: Value,
    pub prefix: super::prefix::Prefix,
    pub normal_return: bool,
    pub memory_support: bool,
    pub clean_captures: FxHashSet<Slot>,
}

impl Flow<'_> {
    pub(super) fn task_call(
        &mut self,
        func: &Operand,
        args: &[Operand],
        ty: TypeId,
        point: Point,
        state: &super::super::State,
    ) -> Option<TaskDispatch> {
        let def = self.function.resolve_callee_def_id(func)?;
        let info = self.program?.definitions?.get(def)?;
        if !matches!(info.kind, DefinitionKind::BuiltinFunction) {
            return None;
        }
        if info.name == kw::SPARK_SPAWN {
            self.spawn_task(args, ty, point, state)
        } else if info.name == kw::SPARK_AWAIT {
            self.await_task(args, ty, point, state)
        } else {
            None
        }
    }

    fn spawn_task(
        &mut self,
        args: &[Operand],
        ty: TypeId,
        point: Point,
        state: &super::super::State,
    ) -> Option<TaskDispatch> {
        let Ty::Future { inner } = self
            .table
            .resolve(super::value::strip_mutable(ty, self.table))
        else {
            return None;
        };
        let argument = args.first()?;
        let callback = self.operand(argument, state);
        let record = Record {
            callback: callback.clone(),
            callback_ty: super::super::operand_type(argument, self.function),
        };
        let fresh_leaf_task = self.leaf_task(&record);
        let (target, mut returned) = super::super::tasks::birth(self.function.def_id, point, ty);
        let facts = self.task_facts(&record, &target, point, state, *inner);
        self.observe_prefix(&facts.prefix, point, state);
        returned.task_views.entry(Vec::new()).or_default().insert(
            super::super::tasks::views::Subject {
                target: target.clone(),
                capture: None,
            },
            super::super::tasks::views::View::pending(callback.clone(), record.callback_ty),
        );
        returned.join(&callback.task_lineage());
        returned.join(&facts.returned.prefixed(Slot::Element, ty, self.table));
        Some(TaskDispatch::Returns(vec![CallOutcome {
            fresh_leaf_task,
            spawn_owners: self.spawn_owner_transfer(args, fresh_leaf_task, state),
            returned_owner_tags: crate::provenance::tasks::lifecycle::Tags::default(),
            retained_captures: Vec::new(),
            returned_callbacks: crate::provenance::tasks::outputs::CallbackOutputs::default(),
            residuals: facts.prefix.residuals,
            async_memory: Vec::new(),
            returned,
            tasks: vec![(target, record)],
            completed_task: None,
            clobbered_copies: Vec::new(),
            future_proof: CallProof::default(),
            returned_prefix: rustc_hash::FxHashSet::default(),
            publications: crate::provenance::effects::PrefixWrites::default(),
            place_writes: Vec::new(),
            prefix_memory: facts.prefix.memory,
            prefix_residuals: Vec::new(),
            memory: Vec::new(),
            queues: facts.prefix.queues,
            queue_contents: Vec::new(),
        }]))
    }

    fn await_task(
        &mut self,
        args: &[Operand],
        ty: TypeId,
        point: Point,
        state: &super::super::State,
    ) -> Option<TaskDispatch> {
        let argument = args.first()?;
        let (Operand::Copy(place) | Operand::Move(place)) = argument else {
            return None;
        };
        let future = self.operand(argument, state);
        let handles: Vec<_> = future
            .futures
            .iter()
            .filter(|(path, _)| path.is_empty() || **path == [Slot::Tail])
            .flat_map(|(path, handles)| {
                handles
                    .iter()
                    .map(move |handle| (path.clone(), handle.clone()))
            })
            .collect();
        if handles.is_empty() {
            return None;
        }
        let mut returned = future.future_result();
        let mut normal = false;
        let mut queues = Vec::new();
        let mut prefix_memory = Vec::new();
        let mut residuals = Vec::new();
        for (path, handle) in handles {
            if !handle.pending {
                normal = true;
                continue;
            }
            let (records, unknown) = if place.projections.is_empty() {
                super::super::tasks::views::pending_records_at(
                    &future,
                    &path,
                    &handle,
                    state,
                    &super::super::demand::Storage::Logical(place.local),
                )
            } else {
                super::super::tasks::views::pending_records(&future, &path, &handle, state)
            };
            normal |= records.is_empty() || unknown;
            for record in records {
                let facts = self.task_facts(&record, &handle.target, point, state, ty);
                self.observe_prefix(&facts.prefix, point, state);
                queues.extend(facts.prefix.queues);
                prefix_memory.extend(facts.prefix.memory);
                residuals.extend(facts.prefix.residuals);
                returned.join(&facts.returned);
                normal |= facts.normal_return;
            }
            if unknown {
                let mut accesses = super::Accesses::default();
                accesses.observe_reference(&future, None, &[], super::AccessKind::Value);
                self.record_accesses(point, accesses, state);
            }
        }
        if !normal {
            return Some(TaskDispatch::NoReturn(super::PrefixWrites {
                memory: prefix_memory,
                queues,
                residuals,
            }));
        }
        let completed_task = place
            .projections
            .is_empty()
            .then_some((place.local, returned.clone()));
        Some(TaskDispatch::Returns(vec![CallOutcome {
            fresh_leaf_task: false,
            spawn_owners: None,
            returned_owner_tags: crate::provenance::tasks::lifecycle::Tags::default(),
            retained_captures: Vec::new(),
            returned_callbacks: crate::provenance::tasks::outputs::CallbackOutputs::default(),
            residuals,
            async_memory: Vec::new(),
            returned,
            tasks: Vec::new(),
            completed_task,
            clobbered_copies: Vec::new(),
            future_proof: CallProof::default(),
            returned_prefix: rustc_hash::FxHashSet::default(),
            publications: crate::provenance::effects::PrefixWrites::default(),
            place_writes: Vec::new(),
            prefix_memory,
            prefix_residuals: Vec::new(),
            memory: Vec::new(),
            queues,
            queue_contents: Vec::new(),
        }]))
    }

    pub(in crate::provenance) fn task_facts(
        &mut self,
        record: &Record,
        target: &Target,
        point: Point,
        state: &super::super::State,
        ty: TypeId,
    ) -> TaskFacts {
        self.task_facts_in(
            record,
            target,
            point,
            super::super::observations::ReadView::current(state),
            ty,
        )
    }

    pub(in crate::provenance) fn task_reads_caller_storage(&self, record: &Record) -> bool {
        let Some(context) = self.program else {
            return true;
        };
        let targets = self.callee_targets(&record.callback, record.callback_ty);
        targets.open
            || targets.unresolved
            || targets.known.is_empty()
            || targets.known.iter().any(|def| {
                context.mir.functions.get(def).is_none_or(|function| {
                    function.param_count != 0 || !function.captures.is_empty()
                }) || context.ports.get(def).is_none_or(|ports| !ports.is_empty())
            })
    }

    pub(in crate::provenance) fn task_facts_in(
        &mut self,
        record: &Record,
        target: &Target,
        point: Point,
        read: super::super::observations::ReadView<'_>,
        ty: TypeId,
    ) -> TaskFacts {
        let mut facts = TaskFacts::default();
        let callee = &record.callback;
        let actuals = CallActuals {
            callee_place: None,
            callee,
            arguments: &[],
            ty,
            point,
            read,
            instantiation_site: target.instantiation_site(),
            frame_caller: None,
            aliases: None,
        };
        let targets = self.callee_targets(callee, record.callback_ty);
        facts.normal_return = targets.open || targets.unresolved;
        facts.memory_support = targets.open || targets.unresolved;
        for def in &targets.known {
            let Some(context) = self.program else {
                continue;
            };
            let Some(function) = context.mir.functions.get(def) else {
                continue;
            };
            let roots = &context.ports[def];
            let locations = self.call_locations(roots, function, actuals);
            let mut clean: FxHashSet<_> = function
                .captures
                .iter()
                .enumerate()
                .filter(|(_, (_, ty))| {
                    matches!(
                        self.table
                            .resolve(super::value::strip_mutable(*ty, self.table)),
                        Ty::Future { .. }
                    )
                })
                .map(|(index, (_, ty))| Slot::Capture(*def, index, *ty))
                .collect();
            let mut observed = false;
            for aliases in Aliases::call_variants(roots, self.table, &locations, &self.aliases) {
                let key = Key {
                    function: *def,
                    aliases,
                    callables: self.callback_inputs(function, actuals),
                };
                observed = true;
                let proven = self.clean_task_capture_slots(function, &key);
                clean.retain(|slot| proven.contains(slot));
                let Some(summary) = context.summaries.get(&key) else {
                    clean.clear();
                    self.requests.insert(key);
                    continue;
                };
                let actuals = CallActuals {
                    aliases: Some(&key.aliases),
                    ..actuals
                };
                let resident = clean
                    .iter()
                    .filter_map(|slot| {
                        let Slot::Capture(_, index, _) = slot else {
                            return None;
                        };
                        callee
                            .futures
                            .get(&vec![slot.clone()])
                            .filter(|handles| !handles.is_empty())
                            .map(|_| spark_mir::Local(function.param_count + 1 + *index as u32))
                    })
                    .collect();
                facts.prefix.merge(self.bind_prefix(
                    summary,
                    function,
                    actuals,
                    &key,
                    None,
                    Some(&resident),
                ));
                facts.normal_return |= summary.normal_return;
                facts.memory_support |=
                    !summary.prefix_memory.is_empty() || summary.unknown_copy_clobber;
                if summary.normal_return {
                    facts
                        .returned
                        .join(&self.bind(&summary.returned, function, actuals));
                }
            }
            if observed && !targets.open && !targets.unresolved {
                facts.clean_captures.extend(clean);
            }
        }
        facts
    }

    fn clean_task_capture_slots(&self, function: &MirFunction, key: &Key) -> FxHashSet<Slot> {
        let Some(certificate) = self
            .program
            .and_then(|context| context.proofs)
            .and_then(|proofs| proofs.get(key))
            .and_then(crate::provenance::tasks::proofs::Proof::certificate)
        else {
            return FxHashSet::default();
        };
        function
            .captures
            .iter()
            .enumerate()
            .filter_map(|(index, (_, ty))| {
                (certificate
                    .clean_ambient_captures
                    .contains(&spark_mir::Local(function.param_count + 1 + index as u32))
                    && matches!(
                        self.table
                            .resolve(super::value::strip_mutable(*ty, self.table)),
                        Ty::Future { .. }
                    ))
                .then_some(Slot::Capture(function.def_id, index, *ty))
            })
            .collect()
    }

    pub(super) fn bind_task_target(
        &self,
        target: &Target,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> FxHashSet<Handle> {
        let targets = match target {
            Target::Input {
                input,
                path,
                ty,
                capture,
                queue,
            } => {
                let selected = self.select_input_value(
                    &Selector {
                        input: *input,
                        selection: path.clone(),
                        intent: Intent::Value,
                        expected: Some(*ty),
                        capture: None,
                        channel_capture: None,
                        queue: queue.clone(),
                    },
                    function,
                    actuals,
                );
                let handles: FxHashSet<_> = selected
                    .futures
                    .into_values()
                    .flatten()
                    .filter(|handle| {
                        (capture.is_none()
                            || handle.capture.is_none()
                            || handle.capture == *capture)
                            && matches!(
                                spark_types::check_expr::cast_kind(
                                    handle.target.ty(),
                                    *ty,
                                    self.table
                                ),
                                Ok(spark_types::check_expr::CastKind::Identity
                                    | spark_types::check_expr::CastKind::Deferred)
                            )
                    })
                    .collect();
                if handles.is_empty() {
                    return FxHashSet::from_iter([Handle {
                        target: Target::Unknown {
                            site: actuals.instantiation_site(self.function.def_id),
                            ty: *ty,
                        },
                        pending: true,
                        capture: None,
                    }]);
                }
                return handles;
            }
            Target::Birth {
                constructor, ty, ..
            } => Target::Birth {
                site: actuals.instantiation_site(self.function.def_id),
                constructor: *constructor,
                ty: *ty,
            },
            Target::Unknown { ty, .. } => Target::Unknown {
                site: actuals.instantiation_site(self.function.def_id),
                ty: *ty,
            },
            Target::Validation { path, ty, .. } => Target::Validation {
                site: actuals.instantiation_site(self.function.def_id),
                path: path.clone(),
                ty: *ty,
            },
        };
        FxHashSet::from_iter([Handle {
            target: targets,
            pending: true,
            capture: None,
        }])
    }

    pub(super) fn bind_task_handles(
        &self,
        value: &Value,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> FxHashMap<super::Path, FxHashSet<Handle>> {
        value
            .futures
            .iter()
            .map(|(path, handles)| {
                (
                    path.clone(),
                    handles
                        .iter()
                        .flat_map(|handle| {
                            self.bind_task_target(&handle.target, function, actuals)
                                .into_iter()
                                .map(move |actual| Handle {
                                    target: actual.target,
                                    pending: handle.pending && actual.pending,
                                    capture: handle.capture,
                                })
                        })
                        .collect(),
                )
            })
            .collect()
    }

    pub(super) fn export_tasks(&self, state: &super::super::State, summary: &mut super::Summary) {
        for (target, record) in &state.tasks {
            let record = Record {
                callback: super::export_value(&record.callback, self.function.def_id),
                callback_ty: record.callback_ty,
            };
            if let Some(current) = summary.tasks.get_mut(target) {
                current.join(&record);
            } else {
                summary.tasks.insert(target.clone(), record);
            }
        }
    }

    pub(super) fn bind_tasks(
        &mut self,
        summary: &super::Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> Vec<(Target, Record)> {
        let mut result = Vec::new();
        for (target, record) in &summary.tasks {
            let callback = self.bind(
                &record.callback,
                function,
                CallActuals {
                    ty: record.callback_ty,
                    ..actuals
                },
            );
            for handle in self.bind_task_target(target, function, actuals) {
                result.push((
                    handle.target,
                    Record {
                        callback: callback.clone(),
                        callback_ty: record.callback_ty,
                    },
                ));
            }
        }
        result
    }
}

#[cfg(test)]
mod tests;
