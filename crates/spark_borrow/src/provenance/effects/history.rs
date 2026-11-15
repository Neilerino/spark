// TAG [SPARK-114]

//! Rebind saved observation support at call entry without re-reading final helper state.

use crate::provenance::{
    address::{Endpoint, FrameLifetime},
    observations::{FrameScope, History, Memory, Sample},
    tasks::views::Subject,
};
use rustc_hash::FxHashSet;

use super::{CallActuals, Flow, MirFunction};

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
            if !view.memory.may_write && view.memory.samples().next().is_none() && !view.fallback {
                continue;
            }
            let callbacks = view
                .callbacks
                .iter()
                .map(|(ty, callback)| {
                    let bound = if let Some(recipe) = activity.callbacks.get(ty) {
                        self.bind(recipe, function, CallActuals { ty: *ty, ..actuals })
                    } else {
                        callback
                            .value()
                            .sourced(&crate::provenance::demand::Storage::Task(
                                actor.target.clone(),
                                *ty,
                            ))
                    };
                    (*ty, bound)
                })
                .collect();
            let key = std::sync::Arc::new(crate::provenance::tasks::residual::Key {
                actor: actor.clone(),
                ty: handle.target.ty(),
                ..(**key).clone()
            });
            result.push(crate::provenance::tasks::residual::Import {
                key,
                value: crate::provenance::tasks::residual::select(value.clone(), &actor),
                callbacks,
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
    ) -> Vec<crate::provenance::tasks::residual::Import> {
        let mut result = Vec::new();
        for (key, activity) in &summary.prefix_residuals {
            let fence = self.event_fence(
                context,
                &super::EventKey::Residual(key.clone()),
                args,
                actuals.read.state,
            );
            result.extend(self.bind_residual_activity(
                key,
                activity,
                function,
                actuals,
                &crate::provenance::tasks::residual::Visibility::Occurrence(fence),
            ));
        }
        result
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

#[cfg(test)]
mod tests;
