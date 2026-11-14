// TAG [SPARK-114]

//! Executed-prefix boundaries that cannot be discharged by a later clear.

use super::{Flow, Point, Slot, Value};
use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_resolve::DefId;
use spark_types::TypeId;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Boundary {
    Store(TypeId),
    Invoke { ty: TypeId, checked: Vec<DefId> },
    InvokeArgument(TypeId),
}

impl Boundary {
    pub fn invoke(ty: TypeId, targets: impl IntoIterator<Item = DefId>) -> Self {
        let mut checked: Vec<_> = targets.into_iter().collect();
        checked.sort_unstable_by_key(|def| def.as_u32());
        checked.dedup();
        Self::Invoke { ty, checked }
    }
    pub const fn ty(&self) -> TypeId {
        match self {
            Self::Store(ty) | Self::Invoke { ty, .. } | Self::InvokeArgument(ty) => *ty,
        }
    }
}

impl Flow<'_> {
    pub(super) fn record_unknown_arguments(
        &mut self,
        point: Point,
        operands: &[spark_mir::Operand],
        values: &[Value],
        state: &super::State,
    ) {
        for (operand, value) in operands.iter().zip(values) {
            let value = self.reachable_queue_payloads(value, state);
            self.record_obligation(
                point,
                Boundary::InvokeArgument(super::operand_type(operand, self.function)),
                &value,
            );
        }
    }

    pub(super) fn record_obligation(&mut self, point: Point, boundary: Boundary, value: &Value) {
        let mut value = value.clone();
        if let Boundary::Invoke { checked, .. } = &boundary {
            let already_checked = |path: &[Slot]| matches!(path.first(), Some(Slot::Capture(owner, _, _)) if checked.contains(owner));
            value
                .leaves
                .retain(|(path, _, _), _| !already_checked(path));
            value
                .sources
                .retain(|(path, _, _), _| !already_checked(path));
        }
        if value.leaves.is_empty() {
            return;
        }
        self.obligations
            .entry((point, boundary))
            .or_default()
            .join(&value);
        self.transfers
            .entry(point)
            .or_default()
            .seeds
            .extend(value.all_sources());
    }

    pub(super) fn report_obligations(&self, diagnostics: &mut Diagnostics) {
        let mut reported = rustc_hash::FxHashSet::default();
        for ((point, boundary), value) in &self.obligations {
            for root in value.frame_sources(self.function.def_id) {
                if !reported.insert((*point, root, matches!(boundary, Boundary::Store(_)))) {
                    continue;
                }
                let (message, primary) = match boundary {
                    Boundary::Store(_) => (
                        "cannot store a reference to local storage through an unproven destination",
                        "this write may retain the borrowed value",
                    ),
                    Boundary::Invoke { .. } | Boundary::InvokeArgument(_) => (
                        "cannot verify effects of a callback on borrowed storage",
                        "this callback's reads and writes are unproven",
                    ),
                };
                diagnostics.push(
                    Diagnostic::error(message)
                        .with_label(Label::primary(
                            crate::errors::span_for_point(*point, self.function),
                            primary,
                        ))
                        .with_label(Label::secondary(
                            crate::errors::span_for_point(root, self.function),
                            "borrowed storage belongs to this function",
                        )),
                );
            }
        }
    }
}
