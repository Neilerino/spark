// TAG [SPARK-114]

//! A lost memory writer stays active until an actual transfer or cleanup proof covers it.

use std::sync::Arc;

use super::views::{self, Subject};
use crate::provenance::{
    address::OpaqueSite,
    demand::{Coordinate, Storage},
    Flow, Path, Point, State, Value,
};
use spark_mir::Place;
use spark_types::TypeId;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::provenance) struct Key {
    pub site: OpaqueSite,
    pub role: Path,
    pub ty: TypeId,
    pub actor: Subject,
}

#[derive(Clone, Default)]
pub(in crate::provenance) struct Activity {
    pub value: Value,
    pub callbacks: rustc_hash::FxHashMap<TypeId, Value>,
}

impl Activity {
    pub fn join(&mut self, other: &Self) -> bool {
        let mut changed = self.value.join(&other.value);
        for (ty, value) in &other.callbacks {
            let absent = !self.callbacks.contains_key(ty);
            changed |= self.callbacks.entry(*ty).or_default().join(value) || absent;
        }
        changed
    }
    pub fn exported(&self, function: spark_resolve::DefId) -> Self {
        Self {
            value: crate::provenance::effects::export_value(&self.value, function),
            callbacks: self
                .callbacks
                .iter()
                .map(|(ty, value)| {
                    (
                        *ty,
                        crate::provenance::effects::export_value(value, function),
                    )
                })
                .collect(),
        }
    }
}

pub(in crate::provenance) type Activities = rustc_hash::FxHashMap<Arc<Key>, Activity>;

#[derive(Clone)]
pub(in crate::provenance) enum Visibility {
    Occurrence(super::proofs::EventFence),
    Continuation(super::proofs::EventFence),
}

impl Visibility {
    pub const fn fence(&self) -> &super::proofs::EventFence {
        match self {
            Self::Occurrence(fence) | Self::Continuation(fence) => fence,
        }
    }
}

#[derive(Clone)]
pub(in crate::provenance) struct Import {
    pub key: Arc<Key>,
    pub value: Value,
    pub callbacks: Vec<(TypeId, Value)>,
    pub visibility: Visibility,
}

impl Import {
    pub fn join(&mut self, other: &Self) {
        self.value.join(&other.value);
        for (ty, value) in &other.callbacks {
            if let Some((_, current)) = self
                .callbacks
                .iter_mut()
                .find(|(current_ty, _)| current_ty == ty)
            {
                current.join(value);
            } else {
                self.callbacks.push((*ty, value.clone()));
            }
        }
        let other = other.visibility.fence();
        let current = match &mut self.visibility {
            Visibility::Occurrence(fence) | Visibility::Continuation(fence) => fence,
        };
        current.inputs.retain(|input| other.inputs.contains(input));
        current.peers.retain(|peer| other.peers.contains(peer));
    }

    pub fn freeze_sources(&mut self, flow: &Flow<'_>, point: Point) {
        self.visit_values(|value| *value = flow.freeze_sources(point, std::mem::take(value)));
    }

    pub fn visit_values(&mut self, mut visit: impl FnMut(&mut Value)) {
        visit(&mut self.value);
        for (_, value) in &mut self.callbacks {
            visit(value);
        }
    }
}

pub(in crate::provenance) fn select(mut value: Value, actor: &Subject) -> Value {
    value.futures.retain(|path, _| path.is_empty());
    for handles in value.futures.values_mut() {
        handles.retain(|handle| Subject::from(handle) == *actor);
    }
    value.task_views.retain(|path, _| path.is_empty());
    for subjects in value.task_views.values_mut() {
        subjects.retain(|subject, _| subject == actor);
    }
    value
}

impl Flow<'_> {
    pub(in crate::provenance) fn import_residuals(
        &mut self,
        residuals: &[Import],
        point: Point,
        state: &mut State,
    ) {
        let mut residuals = residuals.to_vec();
        for residual in &mut residuals {
            residual.freeze_sources(self, point);
        }
        for residual in residuals {
            self.import_residual(residual, point, state);
        }
    }

    pub(in crate::provenance) fn import_residual(
        &mut self,
        mut residual: Import,
        point: Point,
        state: &mut State,
    ) -> bool {
        residual.freeze_sources(self, point);
        let Import {
            key,
            mut value,
            callbacks,
            visibility,
        } = residual;
        self.import_task_sources(&mut value, point, state);
        self.record_path_store(
            point,
            Coordinate {
                storage: Storage::Residual(key.clone()),
                path: Vec::new(),
            },
            &value,
            false,
        );
        let mut activity = Activity {
            value: value.clone(),
            callbacks: rustc_hash::FxHashMap::default(),
        };
        activity.value.sources.clear();
        activity.value.clear_task_imports();
        value.sources.clear();
        let mut changed = state.residuals.entry(key.clone()).or_default().join(&value);
        for (ty, mut callback) in callbacks {
            self.import_task_sources(&mut callback, point, state);
            self.record_path_store(
                point,
                Coordinate {
                    storage: Storage::ResidualCallback(key.clone(), ty),
                    path: Vec::new(),
                },
                &callback,
                false,
            );
            callback.sources.clear();
            let mut recipe = callback.clone();
            recipe.clear_task_imports();
            activity.callbacks.entry(ty).or_default().join(&recipe);
            changed |= state
                .residual_callbacks
                .entry((key.clone(), ty))
                .or_default()
                .join(&callback);
        }
        if let Visibility::Occurrence(fence) = visibility {
            self.record_event_guard(
                crate::provenance::effects::EventKey::Residual(key.clone()),
                &fence.inputs,
            );
            self.prefix_residuals
                .entry(key.clone())
                .or_default()
                .join(&activity);
            state
                .reached_prefix
                .insert(crate::provenance::effects::PrefixOccurrence {
                    event: key.site,
                    returned_at: None,
                });
        }
        changed
    }

    pub(in crate::provenance) fn preserve_writer_loss(
        &mut self,
        place: &Place,
        point: Point,
        state: &mut State,
    ) {
        let source = self.read(place, state);
        if source.futures.is_empty() {
            return;
        }
        if place.projections.is_empty() {
            let witness = state.future_copies.witness(place.local);
            let peers: Vec<_> = witness
                .peers
                .iter()
                .copied()
                .filter(|peer| *peer != place.local && state.live.contains(peer))
                .collect();
            if !peers.is_empty() {
                let proof = super::proofs::CallProof {
                    completed: Vec::new(),
                    returned: Some(witness),
                };
                for peer in peers {
                    let mut current = state.value(peer);
                    Self::preserve_returned_task_progress(&mut current, &proof, &[], state);
                    state.set(peer, current);
                }
                return;
            }
            if state.future_owners.known(place.local)
                && !state.invalidated_copies.contains(&place.local)
            {
                self.complete_future_witness(witness, None, point, state);
                return;
            }
        }
        let root_ty = self.function.locals[place.local.index()].ty;
        let (role, _) = self.projection_path(root_ty, &place.projections);
        for (path, handles) in &source.futures {
            let mut selected = source.clone();
            for slot in path {
                selected = selected.project(slot);
            }
            for handle in handles.iter().filter(|handle| handle.pending) {
                let view = views::contribution(&source, path, handle);
                if !view.memory.may_write
                    && view.memory.samples().next().is_none()
                    && !view.fallback
                {
                    continue;
                }
                let actor = Subject::from(handle);
                let key = Arc::new(Key {
                    site: OpaqueSite {
                        function: self.function.def_id,
                        point,
                    },
                    role: [role.clone(), path.clone()].concat(),
                    ty: handle.target.ty(),
                    actor: actor.clone(),
                });
                let selected = select(selected.clone(), &actor);
                let callbacks = selected
                    .task_views
                    .values()
                    .flat_map(|subjects| subjects.values())
                    .flat_map(|view| view.callbacks.iter())
                    .map(|(ty, callback)| {
                        (
                            *ty,
                            self.freeze_sources(
                                point,
                                callback
                                    .value()
                                    .sourced(&Storage::Task(actor.target.clone(), *ty)),
                            ),
                        )
                    })
                    .collect();
                self.import_residual(
                    Import {
                        key,
                        value: selected,
                        callbacks,
                        visibility: Visibility::Occurrence(super::proofs::EventFence {
                            inputs: state.joined_inputs.clone(),
                            ..super::proofs::EventFence::default()
                        }),
                    },
                    point,
                    state,
                );
            }
        }
        self.settle_lost_publications(&source, point, state);
    }

    fn settle_lost_publications(&mut self, value: &Value, point: Point, state: &mut State) {
        let mut stored = Vec::new();
        for subjects in value.task_views.values() {
            for view in subjects.values() {
                for (lane, value) in view.memory.samples() {
                    if state.pending_memory.contains_key(lane) {
                        stored.push((
                            lane.location.clone(),
                            self.freeze_sources(
                                point,
                                value.sourced(&Storage::PendingWrite(lane.clone())),
                            ),
                        ));
                    }
                }
            }
        }
        for (location, mut value) in stored {
            self.record_path_store(
                point,
                Coordinate {
                    storage: Storage::SettledMemory(location.clone()),
                    path: Vec::new(),
                },
                &value,
                false,
            );
            value.sources.clear();
            state
                .settled_memory
                .entry(location)
                .or_default()
                .join(&value);
        }
    }
}
