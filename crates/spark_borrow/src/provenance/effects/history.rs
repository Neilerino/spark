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
