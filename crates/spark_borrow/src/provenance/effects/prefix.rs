// TAG [SPARK-114]

//! Bind possible executed-prefix effects independently of normal return.

use rustc_hash::FxHashSet;

use super::{
    child_type, export_value, value, Accesses, CallActuals, Dependency, Endpoint, Flow, Intent,
    MirFunction, Origin, Point, Slot, State, Summary, TypeId, Value,
};
use crate::provenance::{
    address::{FrameLifetime, OpaqueSite},
    aliases::View,
    channels::QueueEvent,
    obligations::Boundary,
    read::ReadSite,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) enum Completion {
    Call(OpaqueSite),
    Owner(OpaqueSite),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) struct PrefixOccurrence {
    pub event: OpaqueSite,
    pub returned_at: Option<Completion>,
}

pub(in crate::provenance) type ReachedPrefix = rustc_hash::FxHashSet<PrefixOccurrence>;
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) struct Receive {
    pub site: ReadSite,
    pub before: std::sync::Arc<[PrefixOccurrence]>,
    pub source: Option<crate::provenance::channels::Target>,
}

impl Receive {
    pub fn at(site: ReadSite, reached: &ReachedPrefix) -> Self {
        let mut before: Vec<_> = reached.iter().copied().collect();
        before.sort_unstable_by_key(|occurrence| {
            let key = |site: OpaqueSite| (site.function.0, site.point.block.0, site.point.index);
            let completion = occurrence.returned_at.map(|completion| match completion {
                Completion::Call(site) => (0, key(site)),
                Completion::Owner(site) => (1, key(site)),
            });
            (key(occurrence.event), completion)
        });
        Self {
            site,
            before: before.into(),
            source: None,
        }
    }

    pub fn including(&self, reached: &ReachedPrefix) -> Self {
        let mut combined = reached.clone();
        combined.extend(self.before.iter().copied());
        Self {
            source: self.source.clone(),
            ..Self::at(self.site, &combined)
        }
    }
}

pub(in crate::provenance) fn join_prefix(
    current: &mut ReachedPrefix,
    incoming: &ReachedPrefix,
) -> bool {
    let before = current.len();
    current.extend(incoming.iter().copied());
    before != current.len()
}

pub(in crate::provenance) type MemoryKey = (OpaqueSite, View);
pub(in crate::provenance) type Memory = rustc_hash::FxHashMap<MemoryKey, Value>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) enum EventKey {
    Memory(MemoryKey),
    Queue(OpaqueSite),
    Residual(std::sync::Arc<crate::provenance::tasks::residual::Key>),
}

#[derive(Clone, Default)]
pub(in crate::provenance) struct PrefixWrites {
    pub memory: Vec<MemoryEvent>,
    pub queues: Vec<QueueEvent>,
    pub residuals: Vec<crate::provenance::tasks::residual::Import>,
}

#[derive(Clone)]
pub(in crate::provenance) struct MemoryEvent {
    pub site: OpaqueSite,
    pub write: super::MemoryWrite,
    pub fence: crate::provenance::tasks::proofs::EventFence,
}

#[derive(Default)]
pub(in crate::provenance) struct Prefix {
    pub residuals: Vec<crate::provenance::tasks::residual::Import>,
    accesses: Accesses,
    pub queues: Vec<QueueEvent>,
    pub memory: Vec<MemoryEvent>,
    pub returned: ReachedPrefix,
    pub publications: PrefixWrites,
    pub snapshots: rustc_hash::FxHashMap<std::sync::Arc<Receive>, PrefixWrites>,
    obligations: Vec<(Boundary, Value)>,
}

impl Prefix {
    pub fn merge(&mut self, other: Self) {
        self.accesses.merge(other.accesses);
        self.queues.extend(other.queues);
        self.memory.extend(other.memory);
        self.residuals.extend(other.residuals);
        self.obligations.extend(other.obligations);
        self.returned.extend(other.returned);
        self.publications.memory.extend(other.publications.memory);
        self.publications.queues.extend(other.publications.queues);
        for (receive, writes) in other.snapshots {
            let current = self.snapshots.entry(receive).or_default();
            current.memory.extend(writes.memory);
            current.queues.extend(writes.queues);
        }
    }
}

impl Summary {
    fn returned_prefix(&self, callee: spark_resolve::DefId, boundary: OpaqueSite) -> ReachedPrefix {
        let mut returned: ReachedPrefix = self
            .normal_prefix
            .iter()
            .map(|occurrence| PrefixOccurrence {
                event: occurrence.event,
                returned_at: Some(Completion::Call(boundary)),
            })
            .collect();
        for (event, value) in self.normal_publications.events() {
            for leaf in value.leaves.values() {
                let endpoints =
                    leaf.endpoints
                        .iter()
                        .chain(leaf.dependencies.iter().filter_map(
                            |dependency| match dependency {
                                Dependency::Address(endpoint) => Some(endpoint),
                                Dependency::InputValue(..) => None,
                            },
                        ));
                for endpoint in endpoints {
                    if let Endpoint::Frame {
                        lifetime, function, ..
                    } = endpoint
                    {
                        let owner = match lifetime {
                            FrameLifetime::Local if *function == callee => boundary,
                            FrameLifetime::DuringCall { owner_return, .. }
                            | FrameLifetime::Returned(owner_return) => *owner_return,
                            FrameLifetime::Local => continue,
                        };
                        returned.insert(PrefixOccurrence {
                            event,
                            returned_at: Some(Completion::Owner(owner)),
                        });
                    }
                }
            }
        }
        returned
    }
}

impl Flow<'_> {
    pub(super) fn export_prefix(&self, summary: &mut Summary) {
        summary.prefix_residuals = self
            .prefix_residuals
            .iter()
            .map(|(key, activity)| (key.clone(), activity.exported(self.function.def_id)))
            .collect();
        summary.prefix_memory = self
            .prefix_memory
            .iter()
            .map(|(event, value)| (event.clone(), export_value(value, self.function.def_id)))
            .collect();
        summary.queues = self.export_queue_writes();
        summary.receive_snapshots = self
            .receive_snapshots
            .iter()
            .map(|(receive, values)| (receive.clone(), values.exported(self.function.def_id)))
            .collect();
        for ((_, boundary), value) in &self.obligations {
            summary
                .obligations
                .entry(boundary.clone())
                .or_default()
                .join(&export_value(value, self.function.def_id));
        }
        for access in self.accesses.values().flatten() {
            let selector = match &access.target {
                Dependency::InputValue(selector, _)
                | Dependency::Address(Endpoint::Input(selector, _)) => selector,
                Dependency::Address(Endpoint::Frame { .. }) => continue,
            };
            let mut observed = access.clone();
            observed.origin = Origin::Input {
                local: selector.input,
                path: selector.selection.clone(),
            };
            let pointee = if matches!(selector.intent, Intent::Address(_)) {
                selector.expected.unwrap_or(TypeId::ERROR)
            } else {
                let ty = selector.expected.unwrap_or_else(|| {
                    self.selected_type(
                        self.function.locals[selector.input.index()].ty,
                        &selector.selection,
                    )
                });
                child_type(ty, &Slot::Deref, self.table).unwrap_or(TypeId::ERROR)
            };
            observed.selection = value::normalize(pointee, access.selection.clone(), self.table);
            summary.observed.insert(observed);
        }
    }

    pub(super) fn bind_prefix(
        &mut self,
        summary: &Summary,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        key: &super::Key,
        args: Option<&[spark_mir::Operand]>,
        resident_captures: Option<&FxHashSet<spark_mir::Local>>,
    ) -> Prefix {
        let obligations = summary
            .obligations
            .iter()
            .map(|(boundary, value)| {
                let bound = self.bind(
                    value,
                    function,
                    CallActuals {
                        ty: boundary.ty(),
                        ..actuals
                    },
                );
                (boundary.clone(), bound)
            })
            .collect();
        let memory = self.bind_memory_events(&summary.prefix_memory, function, actuals, key, args);
        let publications = if actuals.frame_caller.is_some() {
            self.bind_publications(&summary.normal_publications, function, actuals, key, args)
        } else {
            PrefixWrites::default()
        };
        let snapshots = self.bind_receive_snapshots(summary, function, actuals, key, args);
        let residuals =
            self.bind_prefix_residuals(summary, function, actuals, key, args, resident_captures);
        let prefix = Prefix {
            residuals,
            snapshots,
            publications,
            returned: actuals
                .frame_caller
                .map_or_else(ReachedPrefix::default, |caller| {
                    summary.returned_prefix(
                        function.def_id,
                        OpaqueSite {
                            function: caller,
                            point: actuals.point,
                        },
                    )
                }),
            memory,
            accesses: self.bound_accesses(summary, function, actuals),
            queues: self.bind_queue_writes(&summary.queues, function, actuals, key, args),
            obligations,
        };
        if actuals.frame_caller.is_some() {
            self.check_prefix_receives(summary, function, actuals, &prefix);
        }
        prefix
    }

    pub(super) fn bind_memory_events(
        &mut self,
        memory_values: &Memory,
        function: &MirFunction,
        actuals: CallActuals<'_>,
        key: &super::Key,
        args: Option<&[spark_mir::Operand]>,
    ) -> Vec<MemoryEvent> {
        let mut memory = Vec::new();
        for (event, value) in memory_values {
            let (site, view) = event;
            let mut target = crate::provenance::value::Leaf::default();
            let members = actuals.aliases.map_or_else(
                || vec![view.root.clone()],
                |aliases| aliases.members(&view.root).collect(),
            );
            for member in members {
                let mut input = self.port_input(&member, function, actuals);
                if !actuals.is_cell(&member) {
                    input = self.residual_reference(&input, &member, function, actuals);
                }
                target.join(&self.project_reference(&input, member.pointee, &view.path));
            }
            let ty = self.selected_type(view.root.pointee, &view.path);
            let value = self.bind(value, function, CallActuals { ty, ..actuals });
            memory.push(MemoryEvent {
                site: *site,
                fence: self.event_fence(
                    key,
                    &EventKey::Memory(event.clone()),
                    args,
                    actuals.read.state,
                ),
                write: super::MemoryWrite { target, ty, value },
            });
        }
        memory
    }

    pub(in crate::provenance) fn observe_prefix(
        &mut self,
        prefix: &Prefix,
        point: Point,
        state: &State,
    ) {
        for event in &prefix.memory {
            self.import_prefix_memory(event, point, state);
        }
        self.record_accesses(point, prefix.accesses.clone(), state);
        self.record_queue_prefix(&prefix.queues, point, state);
        for (boundary, value) in &prefix.obligations {
            self.record_obligation(point, boundary.clone(), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_mir::BlockId;
    use spark_resolve::DefId;

    #[test]
    fn prefix_returns_keep_callee_owners_separate_from_inputs_and_pending_task_frames() {
        let callee = DefId::new(40);
        let nested = DefId::new(41);
        let point = Point::new(BlockId::ENTRY, 0);
        let owner_return = OpaqueSite {
            function: callee,
            point,
        };
        let boundary = OpaqueSite {
            function: DefId::new(42),
            point,
        };
        let event = OpaqueSite {
            function: nested,
            point,
        };
        for evidence in [
            None,
            Some(Completion::Call(owner_return)),
            Some(Completion::Owner(owner_return)),
        ] {
            let mut summary = Summary::default();
            assert!(summary.returned_prefix(callee, boundary).is_empty());
            summary.normal_prefix.insert(PrefixOccurrence {
                event,
                returned_at: evidence,
            });
            let returned = summary.returned_prefix(callee, boundary);
            let expected = ReachedPrefix::from_iter([PrefixOccurrence {
                event,
                returned_at: Some(Completion::Call(boundary)),
            }]);
            assert_eq!(returned, expected);
        }
    }
}
