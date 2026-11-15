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
    pub slots: FxHashMap<(Local, Path, TypeId, Option<value::ReferenceCapture>), Targets>,
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

#[derive(Clone, Copy)]
pub(super) struct FactSource<'a> {
    pub input: Local,
    pub capture: Option<value::ReferenceCapture>,
    pub prefix: &'a [Slot],
}

impl Flow<'_> {
    fn input_seed(
        &self,
        ty: TypeId,
        local: Local,
        source_ty: TypeId,
        prefix: &[Slot],
        facts: Option<FactSource<'_>>,
    ) -> Value {
        let mut result = value::inputs(ty, local, source_ty, prefix, self.table);
        apply_role(&mut result, self.callables.missing);
        let Some(facts) = facts else {
            return result;
        };
        let slots: Vec<_> = result.callees.keys().cloned().collect();
        for path in slots {
            let Some(selected) = path
                .iter()
                .try_fold(ty, |ty, slot| value::child_type(ty, slot, self.table))
            else {
                continue;
            };
            let selected = value::strip_mutable(selected, self.table);
            let fact_ty = facts
                .capture
                .and_then(|capture| value::child_type(capture.ty, &Slot::Deref, self.table))
                .unwrap_or(source_ty);
            let source = value::normalize(
                fact_ty,
                [facts.prefix.to_vec(), path.clone()].concat(),
                self.table,
            );
            let Some(targets) = self.input_targets(facts.input, &source, selected, facts.capture)
            else {
                continue;
            };
            if !targets.unresolved
                && !targets.known.is_empty()
                && targets.known.iter().all(|def| {
                    self.program
                        .is_some_and(|context| context.mir.functions.contains_key(def))
                })
            {
                let placeholder = [path.clone(), vec![Slot::Tail]].concat();
                result.leaves.retain(|(path, _, _), _| *path != placeholder);
            }
            result.callees.insert(path, targets);
        }
        result
    }

    fn owned_callable_location(&self, mut ty: TypeId, path: &[Slot]) -> bool {
        for slot in path {
            if *slot == Slot::Tail {
                return selectors(ty, self.table)
                    .iter()
                    .any(|(path, _)| !path.contains(&Slot::Deref));
            }
            if matches!(slot, Slot::Deref | Slot::ResidualDeref) {
                return false;
            }
            let Some(next) = value::child_type(ty, slot, self.table) else {
                return false;
            };
            ty = next;
        }
        matches!(
            self.table.resolve(value::strip_mutable(ty, self.table)),
            Ty::Function { .. }
        )
    }

    pub(super) fn callee_targets(&self, value: &Value, ty: TypeId) -> Targets {
        if let Some(targets) = value.callees.get(&Vec::new()) {
            return targets.clone();
        }
        let Some(tail) = value.callees.get(&vec![Slot::Tail]) else {
            return Targets {
                unresolved: true,
                ..Targets::default()
            };
        };
        self.tail_targets(tail, ty)
    }

    fn input_targets(
        &self,
        local: Local,
        path: &[Slot],
        ty: TypeId,
        capture: Option<value::ReferenceCapture>,
    ) -> Option<Targets> {
        if let Some(targets) = self
            .callables
            .slots
            .get(&(local, path.to_vec(), ty, capture))
        {
            return Some(targets.clone());
        }
        let mut result = None;
        let mut longest = 0;
        for ((input, selected, _, qualifier), targets) in &self.callables.slots {
            if *input != local || *qualifier != capture {
                continue;
            }
            let Some(index) = selected.iter().position(|slot| *slot == Slot::Tail) else {
                continue;
            };
            if !path.starts_with(&selected[..index]) || index < longest {
                continue;
            }
            let targets = self.tail_targets(targets, ty);
            if index > longest {
                result = None;
                longest = index;
            }
            result.get_or_insert_with(Targets::default).join(&targets);
        }
        result
    }

    fn tail_targets(&self, tail: &Targets, ty: TypeId) -> Targets {
        let mut targets = Targets {
            open: tail.open,
            unresolved: tail.unresolved,
            ..Targets::default()
        };
        for def in &tail.known {
            match self.target_matches(*def, ty) {
                Some(true) => {
                    targets.known.insert(*def);
                }
                Some(false) => {}
                None => targets.unresolved = true,
            }
        }
        if targets.known.is_empty() && !targets.open {
            targets.unresolved = true;
        }
        targets
    }

    fn target_matches(&self, def: DefId, ty: TypeId) -> Option<bool> {
        let Ty::Function {
            params,
            ret,
            is_async,
        } = self.table.resolve(value::strip_mutable(ty, self.table))
        else {
            return None;
        };
        if let Some(function) = self
            .program
            .and_then(|context| context.mir.functions.get(&def))
        {
            return Some(
                function.return_type == *ret
                    && function.is_async == *is_async
                    && function.param_count as usize == params.len()
                    && function
                        .locals
                        .iter()
                        .skip(1)
                        .take(params.len())
                        .map(|decl| decl.ty)
                        .eq(params.iter().copied()),
            );
        }
        self.table.get_def_type(def).map(|actual| {
            value::strip_mutable(actual, self.table) == value::strip_mutable(ty, self.table)
        })
    }

    pub(super) fn symbolic_input(
        &self,
        ty: TypeId,
        local: Local,
        source_ty: TypeId,
        prefix: &[Slot],
    ) -> Value {
        self.symbolic_input_from(
            ty,
            local,
            source_ty,
            prefix,
            Some(FactSource {
                input: local,
                capture: None,
                prefix,
            }),
        )
    }

    pub(super) fn symbolic_pointee_input(
        &self,
        ty: TypeId,
        source_ty: TypeId,
        prefix: &[Slot],
        address: &super::address::Selector,
        qualified: Option<FactSource<'_>>,
    ) -> Value {
        let super::address::Intent::Address(payload) = &address.intent else {
            return self.symbolic_input(ty, address.input, source_ty, prefix);
        };
        let facts = if qualified.is_some() {
            qualified
        } else if address.capture.is_some() {
            Some(FactSource {
                input: address.input,
                capture: address.capture,
                prefix: &payload.projection,
            })
        } else if !prefix.contains(&Slot::Tail) {
            Some(FactSource {
                input: address.input,
                capture: None,
                prefix,
            })
        } else {
            None
        };
        self.symbolic_input_from(ty, address.input, source_ty, prefix, facts)
    }

    fn symbolic_input_from(
        &self,
        ty: TypeId,
        local: Local,
        source_ty: TypeId,
        prefix: &[Slot],
        facts: Option<FactSource<'_>>,
    ) -> Value {
        let mut result = self.input_seed(ty, local, source_ty, prefix, facts);
        let Some(facts) = facts else {
            return result;
        };
        let entries: Vec<_> = self
            .callables
            .slots
            .iter()
            .filter(|((input, _, _, capture), _)| {
                *input == facts.input && *capture == facts.capture
            })
            .filter_map(|((_, path, _, _), targets)| {
                let relative = path.strip_prefix(facts.prefix)?;
                self.owned_callable_location(ty, relative)
                    .then_some((path, relative, targets))
            })
            .collect();
        for (path, relative, targets) in &entries {
            for def in &targets.known {
                let Some(function) = self
                    .program
                    .and_then(|context| context.mir.functions.get(def))
                else {
                    continue;
                };
                for (index, (_, capture_ty)) in function.captures.iter().enumerate() {
                    let slot = Slot::Capture(*def, index, *capture_ty);
                    let selection =
                        [prefix.to_vec(), relative.to_vec(), vec![slot.clone()]].concat();
                    let fact_prefix = [(*path).clone(), vec![slot.clone()]].concat();
                    let mut capture = self.input_seed(
                        *capture_ty,
                        local,
                        source_ty,
                        &selection,
                        Some(FactSource {
                            input: facts.input,
                            capture: facts.capture,
                            prefix: &fact_prefix,
                        }),
                    );
                    if matches!(
                        self.table
                            .resolve(value::strip_mutable(*capture_ty, self.table)),
                        Ty::Ref { .. }
                    ) {
                        capture = capture.with_reference_capture(value::ReferenceCapture {
                            owner: *def,
                            index,
                            ty: *capture_ty,
                        });
                    } else if matches!(
                        self.table
                            .resolve(value::strip_mutable(*capture_ty, self.table)),
                        Ty::Channel { .. }
                    ) {
                        capture = capture.with_channel_capture(super::channels::Capture {
                            owner: *def,
                            index,
                            ty: *capture_ty,
                        });
                    } else if matches!(
                        self.table
                            .resolve(value::strip_mutable(*capture_ty, self.table)),
                        Ty::Future { .. }
                    ) {
                        capture = capture.with_future_capture(super::tasks::Capture {
                            owner: *def,
                            index,
                            ty: *capture_ty,
                        });
                    }
                    result.join(&capture.prefixed_path(
                        &[relative.to_vec(), vec![slot]].concat(),
                        ty,
                        self.table,
                    ));
                }
            }
        }
        for (_, relative, targets) in entries {
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
    for (path, _, _) in value.leaves.keys() {
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
