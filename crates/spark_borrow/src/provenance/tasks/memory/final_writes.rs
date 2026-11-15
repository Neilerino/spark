// TAG [SPARK-114]

//! Prepare final writes and preserve weak contributions until completion is verified.

use super::{
    completion, Coordinate, Effects, Emitter, Flow, FrameScope, FxHashMap, FxHashSet, Memory,
    NormalMemory, OpaqueSite, Point, PreparedMemory, Sample, SettledCompletion, State, Storage,
    Value,
};

#[derive(Default)]
pub(super) struct NormalWrites {
    pub(super) exact: FxHashMap<Sample, Value>,
    pub(super) weak: Vec<(Sample, Value)>,
}

impl NormalWrites {
    fn include_alternative(&mut self, mut alternative: Self, first: bool) {
        self.weak.append(&mut alternative.weak);
        if first {
            self.exact = alternative.exact;
            return;
        }
        self.exact.retain(|location, value| {
            if let Some(other) = alternative.exact.remove(location) {
                value.join(&other);
                true
            } else {
                self.weak.push((location.clone(), std::mem::take(value)));
                false
            }
        });
        self.weak.extend(alternative.exact);
    }
}

impl Flow<'_> {
    pub(super) fn normalize_final_writes(
        &self,
        memory: NormalMemory,
        point: Point,
    ) -> NormalWrites {
        let mut result = NormalWrites::default();
        if memory.incomplete {
            return result;
        }
        let site = OpaqueSite {
            function: self.function.def_id,
            point,
        };
        for (index, alternative) in memory.alternatives.into_iter().enumerate() {
            let mut writes = NormalWrites::default();
            for write in alternative {
                let targets = self.write_targets(&write.target);
                let locations = self.memory_locations(&write.target, write.ty, site);
                if locations.len() == 1
                    && targets
                        .all
                        .iter()
                        .any(|target| targets.strongly_defines(target))
                {
                    writes
                        .exact
                        .entry(locations[0].clone())
                        .or_default()
                        .join(&write.value);
                } else {
                    writes.weak.extend(
                        locations
                            .into_iter()
                            .map(|location| (location, write.value.clone())),
                    );
                }
            }
            result.include_alternative(writes, index == 0);
        }
        result
    }

    pub(super) fn physical_sample(&self, location: &Sample) -> Option<Coordinate> {
        let storage = match &location.memory {
            Memory::Frame {
                scope: FrameScope::Current,
                function,
                local,
                ..
            } if *function == self.function.def_id => Storage::Cell(*local),
            Memory::External(port) => Storage::External(port.clone()),
            _ => return None,
        };
        Some(Coordinate {
            storage,
            path: location.path.clone(),
        })
    }

    fn stage_facet_finals<'a>(
        &mut self,
        emitter: &Emitter,
        memory: &'a Effects,
        support: &FxHashSet<Sample>,
        exact: &FxHashMap<Sample, Value>,
        competing: impl Iterator<Item = &'a Sample>,
        point: Point,
    ) {
        if memory.unknown_write || memory.unknown_interference {
            return;
        }
        let Some(support): Option<Vec<_>> = support
            .iter()
            .filter(|sample| !matches!(sample.memory, Memory::Queue(..)))
            .map(|sample| self.physical_sample(sample))
            .collect()
        else {
            return;
        };
        let Some(interference): Option<Vec<_>> = memory
            .interference
            .iter()
            .chain(competing)
            .map(|sample| self.physical_sample(sample))
            .collect()
        else {
            return;
        };
        for (sample, value) in exact {
            let Some(destination) = self.physical_sample(sample) else {
                continue;
            };
            if interference.iter().any(|write| {
                completion::overlaps(&destination, write)
                    || support.iter().any(|read| completion::overlaps(read, write))
            }) {
                continue;
            }
            self.stage_final_write(
                emitter.role.clone(),
                sample.clone(),
                destination,
                support.clone(),
                value.clone(),
                point,
            );
        }
    }

    pub(super) fn store_settled_sample(
        &mut self,
        location: Sample,
        mut value: Value,
        point: Point,
        state: &mut State,
    ) {
        self.remember_new_settled(&location, &value, point);
        self.record_path_store(
            point,
            Coordinate {
                storage: Storage::SettledMemory(location.clone()),
                path: Vec::new(),
            },
            &value,
            false,
        );
        value.clear_sources();
        state
            .settled_memory
            .entry(location)
            .or_default()
            .join(&value);
    }

    pub(in crate::provenance) fn commit_task_memory(
        &mut self,
        mut prepared: PreparedMemory,
        point: Point,
        state: &mut State,
    ) -> Vec<SettledCompletion> {
        prepared.visit_values(|value| self.import_task_sources(value, point, state));
        let mut values = Vec::new();
        let batches: Vec<_> = prepared
            .facets
            .into_iter()
            .map(|mut facet| {
                let support = std::mem::take(&mut facet.normal.support);
                let normal = self.normalize_final_writes(std::mem::take(&mut facet.normal), point);
                (facet, support, normal)
            })
            .collect();
        let mut destinations = Vec::new();
        for sample in batches.iter().flat_map(|(facet, _, normal)| {
            facet
                .prefix
                .iter()
                .map(|(lane, _)| &lane.location)
                .chain(normal.weak.iter().map(|(sample, _)| sample))
                .chain(normal.exact.keys())
        }) {
            if let Some(destination) = self.physical_sample(sample) {
                destinations.push(destination);
            } else if !matches!(sample.memory, Memory::Queue(..)) {
                self.trace_unknown_completion_write(point);
            }
        }
        self.protect_previous_final_writes(&destinations, point);
        for (index, (facet, support, normal)) in batches.iter().enumerate() {
            let emitter = facet
                .emitter
                .as_ref()
                .filter(|emitter| emitter.still_current(self, state));
            for (sample, source) in facet
                .prefix
                .iter()
                .map(|(lane, _)| {
                    let source = emitter.filter(|_| {
                        lane.execution == crate::provenance::effects::Execution::Current
                    });
                    (&lane.location, source)
                })
                .chain(normal.weak.iter().map(|(sample, _)| (sample, None)))
                .chain(normal.exact.keys().map(|sample| (sample, emitter)))
            {
                if let Some(destination) = self.physical_sample(sample) {
                    let peers =
                        source.map(|source| state.future_copies.role_witness(&source.role).peers);
                    self.record_batch_source(destination, peers.as_ref(), point);
                }
            }
            let competing = batches
                .iter()
                .enumerate()
                .flat_map(|(other, (facet, _, normal))| {
                    facet
                        .prefix
                        .iter()
                        .filter(move |(lane, _)| {
                            other != index
                                || lane.execution != crate::provenance::effects::Execution::Current
                        })
                        .map(|(lane, _)| &lane.location)
                        .chain(normal.weak.iter().map(|(sample, _)| sample))
                        .chain(normal.exact.keys().filter(move |_| other != index))
                });
            if let Some(emitter) = emitter {
                self.stage_facet_finals(
                    emitter,
                    &facet.interference,
                    support,
                    &normal.exact,
                    competing,
                    point,
                );
            }
        }
        self.remember_completion_batch(destinations, point);
        for (facet, _, normal) in batches {
            values.extend(
                facet
                    .prefix
                    .into_iter()
                    .map(|(lane, value)| (lane.location.clone(), value)),
            );
            values.extend(normal.weak);
            values.extend(normal.exact);
        }
        for (location, value) in values {
            self.store_settled_sample(location, value, point, state);
        }
        prepared
            .requests
            .into_iter()
            .filter_map(|request| request.settle(self, point))
            .collect()
    }
}
