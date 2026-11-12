// TAG [SPARK-114]

//! Actual destructive, escaping, or deferred queue access; sampled effects do not execute it.

use super::Target;
use crate::provenance::{demand::Storage, value, Flow, State, Value};
use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::{Local, Operand, Place};
use spark_types::{Ty, TypeId, TypeTable};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::provenance) struct Accesses {
    pub blocked: FxHashSet<Target>,
    pub unknown: bool,
}

impl Accesses {
    pub fn unknown() -> Self {
        Self {
            unknown: true,
            ..Self::default()
        }
    }
    pub fn join(&mut self, other: &Self) -> bool {
        let before = self.blocked.len();
        let changed = !self.unknown && other.unknown;
        self.unknown |= other.unknown;
        self.blocked.extend(other.blocked.iter().cloned());
        changed || self.blocked.len() != before
    }
    pub fn allows(&self, target: &Target) -> bool {
        !self.unknown && !self.blocked.contains(target)
    }
    pub fn remap(&mut self, bindings: &FxHashMap<Target, Target>) {
        self.blocked = std::mem::take(&mut self.blocked)
            .into_iter()
            .map(|target| bindings.get(&target).cloned().unwrap_or(target))
            .collect();
    }
}

pub(in crate::provenance) fn join_returns(
    into: &mut Option<Accesses>,
    other: Option<&Accesses>,
) -> bool {
    let Some(other) = other else { return false };
    if let Some(current) = into {
        current.join(other)
    } else {
        *into = Some(other.clone());
        true
    }
}

fn may_contain_queue(ty: TypeId, table: &TypeTable, seen: &mut FxHashSet<TypeId>) -> bool {
    let ty = value::strip_mutable(ty, table);
    if !seen.insert(ty) {
        return false;
    }
    match table.resolve(ty) {
        Ty::Channel { .. } | Ty::Future { .. } | Ty::Function { .. } | Ty::Param(_) | Ty::Error => {
            true
        }
        Ty::Ref { inner, .. } | Ty::List(inner) | Ty::Set(inner) | Ty::Optional(inner) => {
            may_contain_queue(*inner, table, seen)
        }
        Ty::Map(key, payload) => {
            may_contain_queue(*key, table, seen) || may_contain_queue(*payload, table, seen)
        }
        _ => value::inline_slots(ty, table)
            .into_iter()
            .any(|(_, child)| may_contain_queue(child, table, seen)),
    }
}

impl Flow<'_> {
    pub(in crate::provenance) fn queue_accesses(&self, value: &Value, state: &State) -> Accesses {
        let mut access = Accesses::default();
        let mut pending = Vec::new();
        self.queue_access_roots(value, &mut pending, &mut access);
        let mut seen = FxHashSet::default();
        while let Some(target) = pending.pop() {
            if !seen.insert(target.clone()) {
                continue;
            }
            access.blocked.insert(target.clone());
            for (stored, _, _, payload) in state.queue_entries() {
                if *stored == target {
                    self.queue_access_roots(payload, &mut pending, &mut access);
                }
            }
            if matches!(
                target,
                Target::Input { .. } | Target::Unknown { .. } | Target::Validation { .. }
            ) {
                if let Ty::Channel { inner } = self
                    .table
                    .resolve(value::strip_mutable(target.ty(), self.table))
                {
                    access.unknown |=
                        may_contain_queue(*inner, self.table, &mut FxHashSet::default());
                }
            }
        }
        access
    }

    fn queue_access_roots(&self, value: &Value, pending: &mut Vec<Target>, access: &mut Accesses) {
        let mut visit = |value: &Value| {
            for handle in value.queues.values().flatten() {
                let targets = self.queue_targets(&handle.target);
                access.unknown |= targets.is_empty()
                    || targets.iter().any(|target| {
                        matches!(target, Target::Unknown { .. } | Target::Validation { .. })
                    });
                pending.extend(targets);
            }
            access.unknown |= value
                .callees
                .values()
                .any(|targets| targets.open || targets.unresolved || targets.known.is_empty());
            for (path, handles) in &value.futures {
                for handle in handles.iter().filter(|handle| handle.pending) {
                    let view = value.task_views.get(path).and_then(|views| {
                        views.get(&crate::provenance::tasks::views::Subject::from(handle))
                    });
                    access.unknown |=
                        view.is_none_or(|view| view.fallback || view.callbacks.is_empty());
                }
            }
        };
        visit(value);
        for (_, callback) in value.callback_storage_values(&Storage::Logical(Local::RETURN_PLACE)) {
            visit(&callback);
        }
    }

    pub(in crate::provenance) fn expand_queue_accesses(
        &self,
        access: &Accesses,
        state: &State,
    ) -> Accesses {
        let mut roots = Value::default();
        roots.queues.insert(
            Vec::new(),
            access
                .blocked
                .iter()
                .cloned()
                .map(super::Handle::new)
                .collect(),
        );
        let mut expanded = self.queue_accesses(&roots, state);
        expanded.unknown |= access.unknown;
        expanded
    }

    pub(in crate::provenance) fn operand_queue_accesses(
        &self,
        operand: &Operand,
        state: &State,
    ) -> Accesses {
        let mut access = self.queue_accesses(&self.operand(operand, state), state);
        if matches!(
            self.table.resolve(value::strip_mutable(
                crate::provenance::operand_type(operand, self.function),
                self.table
            )),
            Ty::Function { .. }
        ) {
            access.unknown |= self.environment_sources(operand, state).1;
        }
        access
    }

    pub(in crate::provenance) fn address_queue_access(&self, place: &Place, state: &mut State) {
        if may_contain_queue(
            self.place_type(place),
            self.table,
            &mut FxHashSet::default(),
        ) {
            // References can hide the handle from ordinary argument queue traversal.
            state.queue_accesses.unknown = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_mir::{LocalDecl, MirFunction, Operand};
    use spark_resolve::DefId;
    use spark_span::Span;

    #[test]
    fn pending_or_opaque_value_support_never_counts_as_empty_access() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(spark_types::IntWidth::I64));
        let future = table.intern(Ty::Future { inner: int });
        let callback = table.intern(Ty::Function {
            params: Vec::new(),
            ret: int,
            is_async: false,
        });
        let function = MirFunction {
            def_id: DefId::new(9970),
            param_count: 2,
            return_type: int,
            locals: [int, future, callback]
                .into_iter()
                .enumerate()
                .map(|(index, ty)| LocalDecl {
                    local: Local(index as u32),
                    ty,
                    name: None,
                    is_user_variable: true,
                    mutable: false,
                    span: Span::DUMMY,
                })
                .collect(),
            blocks: Vec::new(),
            captures: Vec::new(),
            is_async: false,
            span: Span::DUMMY,
        };
        let flow = Flow::new(&function, &table, None);
        let mut state = flow.initial_state();
        let pending = state.value(Local(1));
        assert!(pending
            .futures
            .values()
            .flatten()
            .any(|handle| handle.pending));
        assert!(pending.task_views.is_empty());
        assert!(
            flow.queue_accesses(&pending, &state).unknown,
            "a missing Future view is unknown support, not an empty callback"
        );
        let opaque = state.value(Local(2));
        assert!(
            flow.queue_accesses(&opaque, &state).unknown,
            "an opaque callable can hide channel captures"
        );
        state.set(Local(2), Value::function(DefId::new(9971)));
        assert!(!flow.queue_accesses(&state.value(Local(2)), &state).unknown);
        assert!(
            flow.operand_queue_accesses(&Operand::Copy(Place::from_local(Local(2))), &state)
                .unknown,
            "a known MAY target cannot erase unresolved environment support"
        );
    }
}
