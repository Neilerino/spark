// TAG [SPARK-114]

//! Finite callable input facts used to specialize the shared provenance flow.

use super::{value, Flow, Path, Slot, Value};
use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::Local;
use spark_resolve::DefId;
use spark_types::{Ty, TypeId};
use std::hash::{Hash, Hasher};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Targets {
    pub known: FxHashSet<DefId>,
    pub unresolved: bool,
    pub open: bool,
}

impl Targets {
    pub fn function(def: DefId) -> Self {
        Self {
            known: std::iter::once(def).collect(),
            unresolved: false,
            open: false,
        }
    }
    pub fn join(&mut self, other: &Self) -> bool {
        let before = self.known.len();
        self.known.extend(&other.known);
        let changed = before != self.known.len()
            || (!self.unresolved && other.unresolved)
            || (!self.open && other.open);
        self.unresolved |= other.unresolved;
        self.open |= other.open;
        changed
    }
}

impl Hash for Targets {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let mut known: Vec<_> = self.known.iter().map(|def| def.as_u32()).collect();
        known.sort_unstable();
        known.len().hash(state);
        for def in known {
            def.hash(state);
        }
        self.unresolved.hash(state);
        self.open.hash(state);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Inputs {
    pub slots: FxHashMap<(Local, Path, TypeId), Targets>,
    pub missing: InputRole,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(super) enum InputRole {
    #[default]
    Open,
    Unknown,
}

impl Inputs {
    pub fn invocation() -> Self {
        Self {
            missing: InputRole::Unknown,
            ..Self::default()
        }
    }
}

impl Hash for Inputs {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let mut entries: Vec<_> = self
            .slots
            .iter()
            .map(|entry| {
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                entry.hash(&mut hash);
                hash.finish()
            })
            .collect();
        entries.sort_unstable();
        self.missing.hash(state);
        entries.len().hash(state);
        for entry in entries {
            entry.hash(state);
        }
    }
}

impl Flow<'_> {
    pub(super) fn symbolic_input(
        &self,
        ty: TypeId,
        local: Local,
        source_ty: TypeId,
        prefix: &[Slot],
    ) -> Value {
        let mut result = value::inputs(ty, local, source_ty, prefix, self.table);
        apply_role(&mut result, self.callables.missing);
        let facts: Vec<_> = self
            .callables
            .slots
            .iter()
            .filter(|((input, _, _), _)| *input == local)
            .filter_map(|((_, path, _), targets)| {
                path.strip_prefix(prefix)
                    .map(|relative| (path, relative, targets))
            })
            .collect();
        for (path, relative, targets) in &facts {
            for def in &targets.known {
                let Some(function) = self
                    .program
                    .and_then(|context| context.mir.functions.get(def))
                else {
                    continue;
                };
                for (index, (_, capture_ty)) in function.captures.iter().enumerate() {
                    let slot = Slot::Capture(*def, index, *capture_ty);
                    let selection = [(*path).clone(), vec![slot.clone()]].concat();
                    let mut capture =
                        value::inputs(*capture_ty, local, source_ty, &selection, self.table);
                    apply_role(&mut capture, self.callables.missing);
                    result.join(&capture.prefixed_path(
                        &[relative.to_vec(), vec![slot]].concat(),
                        ty,
                        self.table,
                    ));
                }
            }
        }
        for (_, relative, targets) in facts {
            if !targets.unresolved
                && !targets.known.is_empty()
                && targets.known.iter().all(|def| {
                    self.program
                        .is_some_and(|context| context.mir.functions.contains_key(def))
                })
            {
                let placeholder = [relative.to_vec(), vec![Slot::Tail]].concat();
                result.leaves.retain(|(path, _), _| *path != placeholder);
            }
            result.callees.insert(relative.to_vec(), targets.clone());
        }
        result
    }
}

pub(super) fn callable_slots(
    value: &mut Value,
    ty: TypeId,
    table: &spark_types::TypeTable,
    targets: &Targets,
) {
    for (path, _) in value.leaves.keys() {
        let Some((Slot::Tail, selection)) = path.split_last() else {
            continue;
        };
        let selected = selection
            .iter()
            .try_fold(ty, |ty, slot| value::child_type(ty, slot, table));
        if selected.is_some_and(|ty| {
            matches!(
                table.resolve(value::strip_mutable(ty, table)),
                Ty::Function { .. }
            )
        }) {
            value
                .callees
                .entry(selection.to_vec())
                .or_default()
                .join(targets);
        }
    }
}

fn apply_role(value: &mut Value, role: InputRole) {
    for targets in value.callees.values_mut() {
        targets.open = role == InputRole::Open;
        targets.unresolved = role == InputRole::Unknown;
    }
}

pub(super) fn selectors(ty: TypeId, table: &spark_types::TypeTable) -> Vec<(Path, TypeId)> {
    fn walk(
        ty: TypeId,
        path: &mut Path,
        seen: &mut FxHashSet<TypeId>,
        table: &spark_types::TypeTable,
        out: &mut Vec<(Path, TypeId)>,
    ) {
        let ty = value::strip_mutable(ty, table);
        if !seen.insert(ty) {
            return;
        }
        match table.resolve(ty) {
            Ty::Function { .. } => out.push((path.clone(), ty)),
            Ty::Ref { inner, .. } => {
                path.push(Slot::Deref);
                walk(*inner, path, seen, table, out);
                path.pop();
            }
            Ty::List(inner) | Ty::Set(inner) => {
                path.push(Slot::Element);
                walk(*inner, path, seen, table, out);
                path.pop();
            }
            Ty::Map(key, item) => {
                for (slot, child) in [(Slot::Key, *key), (Slot::Value, *item)] {
                    path.push(slot);
                    walk(child, path, seen, table, out);
                    path.pop();
                }
            }
            _ => {
                for (slot, child) in value::inline_slots(ty, table) {
                    path.push(slot);
                    walk(child, path, seen, table, out);
                    path.pop();
                }
            }
        }
        seen.remove(&ty);
    }
    let mut result = Vec::new();
    walk(
        ty,
        &mut Vec::new(),
        &mut FxHashSet::default(),
        table,
        &mut result,
    );
    result
}

#[cfg(test)]
mod tests;
