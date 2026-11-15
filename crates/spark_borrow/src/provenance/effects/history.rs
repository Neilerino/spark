// TAG [SPARK-114]

//! Rebind saved observation support at call entry without re-reading final helper state.

use crate::provenance::{
    address::{Endpoint, FrameLifetime},
    observations::{FrameScope, History, Memory, Sample},
    tasks::views::Subject,
};
use rustc_hash::{FxHashMap, FxHashSet};

use super::{CallActuals, Flow, MirFunction, Value};

impl Flow<'_> {
    pub(super) fn bind_history(
        &mut self,
        history: &History,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        actor: &Subject,
    ) -> (History, bool) {
        let mut bound = History::default();
        let mut unbound = false;
        for (lane, facts) in history.samples() {
            let samples = self.bind_observation_sample(&lane.sample, function, actuals);
            let facts = self.bind(
                &facts,
                function,
                CallActuals {
                    ty: lane.sample.ty,
                    ..actuals
                },
            );
            for sample in samples {
                unbound |= matches!(sample.memory, Memory::Unavailable(_));
                bound.import_sample(lane.rebound(actor.clone(), sample), facts.clone());
            }
        }
        (bound, unbound)
    }

    pub(super) fn bind_task_memory(
        &mut self,
        memory: &crate::provenance::tasks::memory::Effects,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        actor: &Subject,
    ) -> crate::provenance::tasks::memory::Effects {
        let mut bound = crate::provenance::tasks::memory::Effects::default();
        bound.may_write = memory.may_write;
        bound.unknown_write = memory.unknown_write;
        bound.unknown_interference = memory.unknown_interference;
        for sample in &memory.interference {
            for location in self.bind_observation_sample(sample, function, actuals) {
                bound.unknown_interference |= matches!(
                    location.memory,
                    Memory::Unavailable(_)
                        | Memory::Frame {
                            scope: FrameScope::Foreign,
                            ..
                        }
                );
                bound.interference.insert(location);
            }
        }
        for (lane, value) in memory.samples() {
            let value = self.bind(
                &value,
                function,
                CallActuals {
                    ty: lane.location.ty,
                    ..actuals
                },
            );
            for location in self.bind_observation_sample(&lane.location, function, actuals) {
                bound.import(
                    std::sync::Arc::new(crate::provenance::tasks::memory::Lane {
                        event: lane.event,
                        actor: actor.clone(),
                        execution: lane.execution,
                        location,
                    }),
                    value.clone(),
                );
            }
        }
        bound
    }

    pub(super) fn bind_residuals(
        &mut self,
        summary: &super::Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        context: &super::Key,
        args: Option<&[spark_mir::Operand]>,
    ) -> Vec<crate::provenance::tasks::residual::Import> {
        let mut result = Vec::new();
        for (key, value) in &summary.residuals {
            let activity = crate::provenance::tasks::residual::Activity {
                value: value.clone(),
                callbacks: summary
                    .residual_callbacks
                    .iter()
                    .filter(|((owner, _), _)| owner == key)
                    .map(|((_, ty), value)| (*ty, value.clone()))
                    .collect(),
            };
            let fence = self.event_fence(
                context,
                &super::EventKey::Residual(key.clone()),
                args,
                actuals.read.state,
            );
            result.extend(self.bind_residual_activity(
                key,
                &activity,
                function,
                actuals,
                &crate::provenance::tasks::residual::Visibility::Continuation(fence),
            ));
        }
        result
    }

    fn bind_residual_activity(
        &mut self,
        key: &std::sync::Arc<crate::provenance::tasks::residual::Key>,
        activity: &crate::provenance::tasks::residual::Activity,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        visibility: &crate::provenance::tasks::residual::Visibility,
    ) -> Vec<crate::provenance::tasks::residual::Import> {
        let mut result = Vec::new();
        let value = self.bind(
            &activity.value,
            function,
            CallActuals {
                ty: key.ty,
                ..actuals
            },
        );
        for handle in value
            .futures
            .get(&Vec::new())
            .into_iter()
            .flatten()
            .filter(|handle| handle.pending)
        {
            let actor = Subject::from(handle);
            let view = crate::provenance::tasks::views::contribution(&value, &[], handle);
            let mut callbacks: FxHashMap<_, _> = view
                .callbacks
                .iter()
                .map(|(ty, callback)| (*ty, callback.value()))
                .collect();
            for (ty, recipe) in &activity.callbacks {
                let bound = self.bind(recipe, function, CallActuals { ty: *ty, ..actuals });
                callbacks.entry(*ty).or_default().join(&bound);
            }
            if !view.retains_activity() && !callbacks.values().any(Value::retains_task_activity) {
                continue;
            }
            let key = std::sync::Arc::new(crate::provenance::tasks::residual::Key {
                actor: actor.clone(),
                ty: handle.target.ty(),
                ..(**key).clone()
            });
            result.push(crate::provenance::tasks::residual::Import {
                key,
                value: crate::provenance::tasks::residual::select(value.clone(), &actor),
                callbacks: callbacks.into_iter().collect(),
                visibility: visibility.clone(),
            });
        }

        result
    }

    pub(super) fn bind_prefix_residuals(
        &mut self,
        summary: &super::Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        context: &super::Key,
        args: Option<&[spark_mir::Operand]>,
        resident_captures: Option<&FxHashSet<spark_mir::Local>>,
    ) -> Vec<crate::provenance::tasks::residual::Import> {
        let mut result = Vec::new();
        for (key, activity) in &summary.prefix_residuals {
            let resident = resident_captures
                .is_some_and(|captures| resident_capture_activity(key, function, captures));
            let fence = self.event_fence(
                context,
                &super::EventKey::Residual(key.clone()),
                args,
                actuals.read.state,
            );
            let bound = self.bind_residual_activity(
                key,
                activity,
                function,
                actuals,
                &crate::provenance::tasks::residual::Visibility::Occurrence(fence),
            );
            for activity in bound {
                if !resident || !self.capture_contains_activity(key, &activity, function, actuals) {
                    result.push(activity);
                }
            }
        }
        result
    }

    fn capture_contains_activity(
        &self,
        key: &crate::provenance::tasks::residual::Key,
        activity: &crate::provenance::tasks::residual::Import,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> bool {
        let crate::provenance::tasks::Target::Input { input, ty, .. } = key.actor.target else {
            return false;
        };
        let capture = crate::provenance::tasks::Capture {
            owner: function.def_id,
            index: input.index() - function.param_count as usize - 1,
            ty,
        };
        let selected = self
            .select_input_value(
                &super::Selector {
                    input,
                    selection: Vec::new(),
                    intent: super::Intent::Value,
                    expected: Some(ty),
                    capture: None,
                    channel_capture: None,
                    queue: None,
                },
                function,
                actuals,
            )
            .with_future_capture(capture);
        let mut covered = selected.clone();
        if covered.join(&activity.value.with_future_capture(capture)) {
            return false;
        }
        let Some(view) = selected.task_views.get(&Vec::new()).and_then(|views| {
            views.get(&crate::provenance::tasks::views::Subject {
                target: activity.key.actor.target.clone(),
                capture: Some(capture),
            })
        }) else {
            return activity.callbacks.is_empty();
        };
        activity.callbacks.iter().all(|(ty, callback)| {
            view.callbacks
                .get(ty)
                .is_some_and(|current| !current.value().join(callback))
        })
    }

    fn bind_external_sample(
        &self,
        port: &crate::provenance::aliases::Port,
        sample: &Sample,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        unavailable: &Sample,
    ) -> FxHashSet<Sample> {
        let mut bound = FxHashSet::default();
        let members = actuals.aliases.map_or_else(
            || vec![port.clone()],
            |aliases| aliases.members(port).collect(),
        );
        for member in members {
            let mut pointer = self.port_input(&member, function, actuals);
            if !actuals.is_cell(&member) {
                pointer = self.residual_reference(&pointer, &member, function, actuals);
            }
            let metadata = self.project_reference(&pointer, member.pointee, &sample.path);
            if !metadata.opaque.is_empty() {
                bound.insert(unavailable.clone());
            }
            for endpoint in metadata.endpoints {
                match endpoint {
                    Endpoint::Frame {
                        function,
                        local,
                        root_ty,
                        location,
                        lifetime,
                        ..
                    } => {
                        let scope = if function == self.function.def_id
                            && lifetime == FrameLifetime::Local
                        {
                            FrameScope::Current
                        } else {
                            FrameScope::Foreign
                        };
                        bound.insert(Sample {
                            memory: Memory::Frame {
                                scope,
                                function,
                                local,
                                ty: root_ty,
                            },
                            path: location,
                            ty: sample.ty,
                        });
                    }
                    Endpoint::Input(selector, _) => {
                        let targets = self.external_targets(&selector);
                        if targets.is_empty() {
                            bound.insert(unavailable.clone());
                        }
                        for (view, _) in targets {
                            bound.insert(Sample {
                                memory: Memory::External(view.root),
                                path: view.path,
                                ty: sample.ty,
                            });
                        }
                    }
                }
            }
        }

        bound
    }

    fn bind_observation_sample(
        &self,
        sample: &Sample,
        function: &MirFunction,
        actuals: CallActuals<'_>,
    ) -> FxHashSet<Sample> {
        let mut bound = FxHashSet::default();
        let unavailable = Sample {
            memory: Memory::Unavailable(actuals.instantiation_site(self.function.def_id)),
            ..sample.clone()
        };
        match &sample.memory {
            Memory::External(port) => {
                bound = self.bind_external_sample(port, sample, function, actuals, &unavailable);
            }
            Memory::Queue(target, ty) => {
                for target in self.bind_queue_target(target, function, actuals) {
                    if matches!(
                        target,
                        crate::provenance::channels::Target::Unknown { .. }
                            | crate::provenance::channels::Target::Validation { .. }
                    ) {
                        bound.insert(unavailable.clone());
                    }
                    bound.insert(Sample {
                        memory: Memory::Queue(target, *ty),
                        ..sample.clone()
                    });
                }
            }
            Memory::Frame {
                function,
                local,
                ty,
                ..
            } => {
                bound.insert(Sample {
                    memory: Memory::Frame {
                        scope: FrameScope::Foreign,
                        function: *function,
                        local: *local,
                        ty: *ty,
                    },
                    ..sample.clone()
                });
            }
            Memory::Unavailable(_) => {
                bound.insert(sample.clone());
            }
        }
        if bound.is_empty() {
            bound.insert(unavailable);
        }
        bound
    }
}

fn resident_capture_activity(
    key: &crate::provenance::tasks::residual::Key,
    function: &MirFunction,
    resident: &FxHashSet<spark_mir::Local>,
) -> bool {
    let crate::provenance::tasks::Target::Input {
        input,
        path,
        ty,
        capture,
        queue,
    } = &key.actor.target
    else {
        return false;
    };
    if !resident.contains(input) || !path.is_empty() || queue.is_some() {
        return false;
    }
    let Some(index) = input.index().checked_sub(function.param_count as usize + 1) else {
        return false;
    };
    function
        .captures
        .get(index)
        .is_some_and(|(_, captured_ty)| {
            *ty == *captured_ty
                && capture.is_none_or(|capture| {
                    capture.owner == function.def_id && capture.index == index && capture.ty == *ty
                })
        })
}

#[cfg(test)]
mod tests;
