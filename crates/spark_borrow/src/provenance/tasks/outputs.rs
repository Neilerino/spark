// TAG [SPARK-114]

//! Callback output sources are MAY data; exact owner recipes are separate guarantees.

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::{Local, Mir, MirFunction, Operand, Place};
use spark_resolve::DefId;
use spark_types::{Ty, TypeId, TypeTable};

use super::{
    lifecycle::{CapturedOwner, Tags, Token},
    Role,
};
use crate::provenance::{
    demand::{Coordinate, Storage},
    effects::Summary,
    Flow, Path, Point, Slot, State, Value,
};

#[derive(Clone, Debug, Default)]
pub(in crate::provenance) struct CallbackOutput {
    pub callback: Value,
    pub lineage: FxHashMap<Path, Tags>,
}

pub(in crate::provenance) type CallbackOutputs = FxHashMap<TypeId, CallbackOutput>;

impl CallbackOutput {
    pub fn join(&mut self, other: &Self) -> bool {
        let mut changed = self.callback.join(&other.callback);
        for (path, tags) in &other.lineage {
            let current = self.lineage.entry(path.clone()).or_default();
            let before = current.len();
            current.extend(tags.iter().copied());
            changed |= before != current.len();
        }
        changed
    }
}

pub(in crate::provenance) fn output_role(local: Local, ty: TypeId, path: &[Slot]) -> Option<Role> {
    let [Slot::Capture(owner, index, child_ty)] = path else {
        return None;
    };
    Role::capture(local, *owner, *index, *child_ty).in_execution(local, ty)
}

impl Flow<'_> {
    pub(in crate::provenance) fn export_callback_outputs(
        &self,
        state: &State,
        summary: &mut Summary,
    ) {
        let roots: FxHashSet<_> = state
            .future_copies
            .witness(Local::RETURN_PLACE)
            .locals()
            .collect();
        for ((local, ty), callback) in &state.executions {
            if !roots.contains(local) {
                continue;
            }
            let output = CallbackOutput {
                callback: crate::provenance::effects::export_value(callback, self.function.def_id),
                lineage: state.input_owners.execution_outputs(*local, *ty),
            };
            summary
                .returned_callbacks
                .entry(*ty)
                .or_default()
                .join(&output);
        }
    }

    pub(in crate::provenance) fn install_callback_outputs(
        &mut self,
        outputs: CallbackOutputs,
        destination: &Place,
        point: Point,
        state: &mut State,
    ) {
        if !destination.projections.is_empty() {
            state.input_owners.escape_tags(
                outputs
                    .into_values()
                    .flat_map(|output| output.lineage.into_values().flatten())
                    .collect(),
            );
            return;
        }
        for (ty, mut output) in outputs {
            if let Some(existing) = state.executions.get(&(destination.local, ty)) {
                output.callback.join(
                    &self.freeze_sources(
                        point,
                        existing
                            .clone()
                            .sourced(&Storage::ExecutionCallback(destination.local, ty)),
                    ),
                );
            }
            for (path, tags) in output.lineage {
                if let Some(role) = output_role(destination.local, ty, &path) {
                    state.input_owners.add_output(role, tags);
                }
            }
            self.import_task_sources(&mut output.callback, point, state);
            self.record_path_store(
                point,
                Coordinate {
                    storage: Storage::ExecutionCallback(destination.local, ty),
                    path: vec![],
                },
                &output.callback,
                true,
            );
            output.callback.clear_sources();
            state.install_execution_callback(destination.local, ty, output.callback, false);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::provenance) struct CallbackRecipe {
    pub parent_ty: TypeId,
    pub callback_ty: TypeId,
    pub callback: DefId,
    pub slots: FxHashMap<Path, Token>,
}

impl CallbackRecipe {
    pub fn candidate(
        outputs: &CallbackOutputs,
        function: &MirFunction,
        mir: &Mir,
        table: &TypeTable,
    ) -> Option<Self> {
        let parent_ty = crate::provenance::value::strip_mutable(function.return_type, table);
        if !matches!(table.resolve(parent_ty), Ty::Future { .. }) || outputs.len() != 1 {
            return None;
        }
        let (ty, output) = outputs.iter().next()?;
        let targets = output.callback.callees.get(&vec![])?;
        if targets.open || targets.unresolved || targets.known.len() != 1 {
            return None;
        }
        let callback = *targets.known.iter().next()?;
        let definition = mir.functions.get(&callback)?;
        let mut slots = FxHashMap::default();
        for (index, (_, child_ty)) in definition.captures.iter().enumerate() {
            let child_ty = crate::provenance::value::strip_mutable(*child_ty, table);
            if matches!(table.resolve(child_ty), Ty::Future { .. }) {
                let path = vec![Slot::Capture(callback, index, definition.captures[index].1)];
                let tags = output.lineage.get(&path)?;
                if tags.len() != 1 {
                    return None;
                }
                let tag = *tags.iter().next()?;
                let input = tag.input();
                if input.0 == 0
                    || input.0 > function.param_count
                    || crate::provenance::value::strip_mutable(
                        function.locals[input.index()].ty,
                        table,
                    ) != child_ty
                {
                    return None;
                }
                slots.insert(path, tag);
            } else if super::lifecycle::may_own_future(child_ty, table, &mut FxHashSet::default()) {
                return None;
            }
        }
        (!slots.is_empty()).then_some(Self {
            parent_ty,
            callback_ty: *ty,
            callback,
            slots,
        })
    }

    pub fn formals(&self) -> FxHashSet<Local> {
        self.slots.values().map(|token| token.input()).collect()
    }
}

pub(in crate::provenance) struct BoundOutput {
    parent_ty: TypeId,
    callback_ty: TypeId,
    pub covered: FxHashSet<usize>,
    captures: Vec<CapturedOwner>,
}

impl BoundOutput {
    pub fn valid(
        &self,
        flow: &Flow<'_>,
        destination: &Place,
        clobbered: &[Local],
        state: &State,
    ) -> bool {
        crate::provenance::value::strip_mutable(flow.place_type(destination), flow.table)
            == self.parent_ty
            && self.captures.iter().all(|capture| {
                capture
                    .witness
                    .peers
                    .iter()
                    .all(|peer| !peer.frame_clobbered(clobbered, &state.invalidated_copies))
            })
    }
    pub fn carry_progress(
        &self,
        flow: &Flow<'_>,
        outputs: &mut CallbackOutputs,
        clobbered: &[Local],
        point: Point,
        state: &State,
    ) {
        for capture in &self.captures {
            let Some(ty) = capture.role.callback_type() else {
                continue;
            };
            let Some(output) = outputs.get_mut(&ty) else {
                continue;
            };
            let [Slot::Capture(owner, index, child_ty)] = capture.role.selection() else {
                continue;
            };
            let mut child = output.callback.clone();
            for slot in capture.role.selection() {
                child = child.project(slot);
            }
            for peer in capture.witness.locals().filter(|peer| {
                state.live.contains(peer)
                    && !clobbered.contains(peer)
                    && !state.invalidated_copies.contains(peer)
            }) {
                let current = state
                    .value(peer)
                    .sourced(&Storage::Logical(peer))
                    .with_future_capture(super::Capture {
                        owner: *owner,
                        index: *index,
                        ty: *child_ty,
                    });
                child.merge_task_progress(&current);
            }
            output.callback.clear_path(capture.role.selection());
            output
                .callback
                .join(&child.prefixed_path(capture.role.selection(), ty, flow.table));
            output.callback = flow.freeze_sources(point, std::mem::take(&mut output.callback));
        }
    }

    pub fn reserves(&self, local: Local) -> bool {
        self.captures
            .iter()
            .any(|capture| capture.witness.contains_local(local))
    }

    pub fn restrict_coverage(&mut self, state: &State) {
        for capture in &mut self.captures {
            capture.coverage = capture.coverage.take().and_then(|coverage| {
                coverage.meet(state.future_owners.group_coverage(&capture.witness.peers)?)
            });
        }
    }
    pub fn install(mut self, destination: Local, state: &mut State) {
        if state.invalidated_copies.contains(&destination)
            || self.captures.iter().any(|capture| {
                capture
                    .witness
                    .peers
                    .iter()
                    .any(|peer| peer.frame_clobbered(&[], &state.invalidated_copies))
            })
        {
            for capture in self.captures {
                state
                    .future_owners
                    .revoke(&capture.witness, &state.future_copies);
            }
            state
                .execution_backing_must
                .remove(&(destination, self.callback_ty));
            return;
        }
        for capture in &mut self.captures {
            capture.role = capture.role.reanchor(destination);
        }
        CapturedOwner::install_all(self.captures, state);
        if state
            .executions
            .contains_key(&(destination, self.callback_ty))
        {
            state
                .execution_backing_must
                .insert((destination, self.callback_ty));
        }
    }
}

impl Flow<'_> {
    pub(in crate::provenance) fn observed_callback_recipe(
        &self,
        state: &State,
    ) -> Option<CallbackRecipe> {
        if !state.future_copies.inputs(Local::RETURN_PLACE).is_empty() {
            return None;
        }
        state.current_execution_record(Local::RETURN_PLACE)?;
        let mut summary = Summary::default();
        self.export_callback_outputs(state, &mut summary);
        let recipe = CallbackRecipe::candidate(
            &summary.returned_callbacks,
            self.function,
            self.program?.mir,
            self.table,
        )?;
        let roots: FxHashSet<_> = state
            .future_copies
            .witness(Local::RETURN_PLACE)
            .locals()
            .collect();
        let mut roles = FxHashSet::default();
        for (local, ty) in state.executions.keys() {
            if *ty != recipe.callback_ty || !roots.contains(local) {
                continue;
            }
            for (path, tag) in &recipe.slots {
                let role = output_role(*local, *ty, path)?;
                if !state
                    .future_copies
                    .role_witness(&role)
                    .inputs
                    .contains(&tag.input())
                    || state.input_owners.tags(&role) != Tags::from_iter([*tag])
                {
                    return None;
                }
                roles.insert(role);
            }
        }
        if roles.len() != recipe.slots.len()
            || !recipe
                .formals()
                .iter()
                .all(|input| state.input_owners.retains_only(*input, &roles))
        {
            return None;
        }
        Some(recipe)
    }

    pub(in crate::provenance) fn bind_output_recipe(
        &self,
        recipe: &CallbackRecipe,
        args: &[Operand],
        state: &State,
    ) -> Option<BoundOutput> {
        let mut captures = Vec::new();
        for (path, tag) in &recipe.slots {
            let input = tag.input().index().checked_sub(1)?;
            let (Operand::Copy(place) | Operand::Move(place)) = args.get(input)? else {
                return None;
            };
            let [Slot::Capture(_, _, child_ty)] = path.as_slice() else {
                return None;
            };
            if !place.projections.is_empty()
                || crate::provenance::value::strip_mutable(self.place_type(place), self.table)
                    != crate::provenance::value::strip_mutable(*child_ty, self.table)
            {
                return None;
            }
            captures.push(CapturedOwner {
                role: output_role(Local::RETURN_PLACE, recipe.callback_ty, path)?,
                witness: state.future_copies.witness(place.local),
                coverage: state
                    .future_owners
                    .coverage(&super::Role::root(place.local)),
            });
        }
        Some(BoundOutput {
            parent_ty: recipe.parent_ty,
            callback_ty: recipe.callback_ty,
            covered: recipe
                .formals()
                .iter()
                .filter_map(|input| input.index().checked_sub(1))
                .collect(),
            captures,
        })
    }
}
