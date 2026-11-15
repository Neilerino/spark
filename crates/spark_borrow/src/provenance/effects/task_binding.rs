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
    NoReturn,
}

#[derive(Default)]
pub(in crate::provenance) struct TaskFacts {
    pub returned: Value,
    pub prefix: super::prefix::Prefix,
    pub normal_return: bool,
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
            let facts = self.task_facts(&record, point, state, *inner);
            self.observe_prefix(&facts.prefix, point, state);
            let (target, mut returned) =
                super::super::tasks::birth(self.function.def_id, point, ty);
            returned.join(&callback.task_lineage());
            returned.join(&facts.returned.prefixed(Slot::Element, ty, self.table));
            return Some(TaskDispatch::Returns(vec![CallOutcome {
                returned,
                tasks: vec![(target, record)],
                completed_task: None,
                clobbered_copies: Vec::new(),
                future_proof: CallProof::default(),
                place_writes: Vec::new(),
                memory: Vec::new(),
                queues: facts.prefix.queues,
                queue_contents: Vec::new(),
            }]));
        }
        if info.name != kw::SPARK_AWAIT {
            return None;
        }
        let argument = args.first()?;
        let (Operand::Copy(place) | Operand::Move(place)) = argument else {
            return None;
        };
        let future = self.operand(argument, state);
        let handles: Vec<_> = future
            .futures
            .iter()
            .filter(|(path, _)| path.is_empty() || **path == [Slot::Tail])
            .flat_map(|(_, handles)| handles.iter().cloned())
            .collect();
        if handles.is_empty() {
            return None;
        }
        let mut returned = future.future_result();
        let mut normal = false;
        let mut queues = Vec::new();
        for handle in handles {
            if let Some(record) = state.tasks.get(&handle.target) {
                let mut record = record.clone();
                record.callback = record
                    .callback
                    .sourced(&super::super::demand::Storage::Task(
                        handle.target.clone(),
                        record.callback_ty,
                    ));
                let facts = self.task_facts(&record, point, state, ty);
                if handle.pending {
                    self.observe_prefix(&facts.prefix, point, state);
                    queues.extend(facts.prefix.queues);
                }
                returned.join(&facts.returned);
                normal |= facts.normal_return || !handle.pending;
            } else {
                normal = true;
                if handle.pending {
                    let mut accesses = super::Accesses::default();
                    accesses.observe_reference(&future, None, &[], super::AccessKind::Value);
                    self.record_accesses(point, accesses, state);
                }
            }
        }
        if !normal {
            return Some(TaskDispatch::NoReturn);
        }
        let completed_task = place
            .projections
            .is_empty()
            .then_some((place.local, returned.clone()));
        Some(TaskDispatch::Returns(vec![CallOutcome {
            returned,
            tasks: Vec::new(),
            completed_task,
            clobbered_copies: Vec::new(),
            future_proof: CallProof::default(),
            place_writes: Vec::new(),
            memory: Vec::new(),
            queues,
            queue_contents: Vec::new(),
        }]))
    }

    pub(in crate::provenance) fn task_facts(
        &mut self,
        record: &Record,
        point: Point,
        state: &super::super::State,
        ty: TypeId,
    ) -> TaskFacts {
        let mut facts = TaskFacts::default();
        let callee = &record.callback;
        let actuals = CallActuals {
            callee,
            arguments: &[],
            ty,
            point,
            state,
            aliases: None,
        };
        let targets = self.callee_targets(callee, record.callback_ty);
        facts.normal_return = targets.open || targets.unresolved;
        for def in &targets.known {
            let Some(context) = self.program else {
                continue;
            };
            let Some(function) = context.mir.functions.get(def) else {
                continue;
            };
            let roots = &context.ports[def];
            let locations = self.call_locations(roots, function, actuals);
            for aliases in Aliases::call_variants(roots, self.table, &locations, &self.aliases) {
                let key = Key {
                    function: *def,
                    aliases,
                    callables: self.callback_inputs(function, actuals),
                };
                let Some(summary) = context.summaries.get(&key) else {
                    self.requests.insert(key);
                    continue;
                };
                let actuals = CallActuals {
                    aliases: Some(&key.aliases),
                    ..actuals
                };
                facts
                    .prefix
                    .merge(self.bind_prefix(summary, function, actuals));
                facts.normal_return |= summary.normal_return;
                if summary.normal_return {
                    facts
                        .returned
                        .join(&self.bind(&summary.returned, function, actuals));
                }
            }
        }
        facts
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
                            site: super::super::address::OpaqueSite {
                                function: self.function.def_id,
                                point: actuals.point,
                            },
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
                site: super::super::address::OpaqueSite {
                    function: self.function.def_id,
                    point: actuals.point,
                },
                constructor: *constructor,
                ty: *ty,
            },
            Target::Unknown { ty, .. } => Target::Unknown {
                site: super::super::address::OpaqueSite {
                    function: self.function.def_id,
                    point: actuals.point,
                },
                ty: *ty,
            },
            Target::Validation { path, ty, .. } => Target::Validation {
                site: super::super::address::OpaqueSite {
                    function: self.function.def_id,
                    point: actuals.point,
                },
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
