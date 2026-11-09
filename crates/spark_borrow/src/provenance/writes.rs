//! Current carrier contents written into incoming borrowed storage.

// TAG [SPARK-114]

use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_mir::{BlockId, Place, Projection};

use super::address::{Dependency, Endpoint, Intent, Selector};
use super::demand::{Bank, Coordinate};
use super::{value, Flow, Path, Point, Slot, State, Value};

impl Flow<'_> {
    pub(super) fn store_external(
        &mut self,
        place: &Place,
        value: &Value,
        point: Point,
        state: &mut State,
    ) -> bool {
        if !place
            .projections
            .iter()
            .any(|projection| matches!(projection, Projection::Deref))
        {
            return false;
        }
        let metadata = self.reference_metadata(place, point, state);
        let unique = metadata.endpoints.len() == 1 && metadata.opaque.is_empty();
        let mut stored = false;
        for endpoint in &metadata.endpoints {
            let Endpoint::Input(selector) = endpoint else {
                continue;
            };
            let prefix = self.external_path(selector);
            let strong = unique
                && selector.selection.is_empty()
                && matches!(&selector.intent, Intent::Address(payload) if payload.projection.is_empty());
            let root = self.function.locals[selector.input.index()].ty;
            self.record_path_store(
                point,
                Coordinate {
                    bank: Bank::External,
                    local: selector.input,
                    path: prefix.clone(),
                },
                value,
                strong,
            );
            let mut current = state
                .external
                .get(&selector.input)
                .cloned()
                .unwrap_or_default();
            if strong {
                current
                    .leaves
                    .retain(|(path, _), _| !super::storage::prefix(&prefix, path));
                current
                    .callees
                    .retain(|path, _| !super::storage::prefix(&prefix, path));
                state.external_defined.retain(|(local, path)| {
                    *local != selector.input || !super::storage::prefix(&prefix, path)
                });
                state
                    .external_defined
                    .insert((selector.input, prefix.clone()));
            }
            current.join(&value.prefixed_path(&prefix, root, self.table));
            current.sources.clear();
            state.external.insert(selector.input, current);
            stored = true;
        }
        stored
    }

    pub(super) fn external_path(&self, selector: &Selector) -> Path {
        let mut path = selector.selection.clone();
        path.push(Slot::Deref);
        if let Intent::Address(payload) | Intent::Payload(payload) = &selector.intent {
            path.extend(payload.projection.iter().cloned());
        }
        value::normalize(
            self.function.locals[selector.input.index()].ty,
            path,
            self.table,
        )
    }

    pub(super) fn external_contents(&self, selector: &Selector, state: &State) -> (Value, bool) {
        let path = self.external_path(selector);
        let mut value = state
            .external
            .get(&selector.input)
            .cloned()
            .unwrap_or_default()
            .sourced(Bank::External, selector.input);
        for slot in &path {
            value = value.project(slot);
        }
        let complete = state.external_defined.iter().any(|(local, prefix)| {
            *local == selector.input && super::storage::prefix(prefix, &path)
        });
        (value, complete)
    }

    pub(super) fn report_external_stores(
        &self,
        state: &State,
        block: BlockId,
        diagnostics: &mut Diagnostics,
    ) {
        let mut reported = rustc_hash::FxHashSet::default();
        for value in state.external.values() {
            for leaf in value.leaves.values() {
                for endpoint in leaf
                    .endpoints
                    .iter()
                    .chain(leaf.dependencies.iter().filter_map(|dependency| {
                        if let Dependency::Address(endpoint) = dependency {
                            Some(endpoint)
                        } else {
                            None
                        }
                    }))
                {
                    if let Endpoint::Frame { function, root, .. } = endpoint {
                        if *function == self.function.def_id && reported.insert(*root) {
                            let exit = Point::new(
                                block,
                                self.function.blocks[block.index()].statements.len() as u32,
                            );
                            diagnostics.push(Diagnostic::error("stored value contains a reference to local storage")
                                .with_label(Label::primary(crate::errors::span_for_point(exit, self.function), "borrowed storage retains this reference when the function returns"))
                                .with_label(Label::secondary(crate::errors::span_for_point(*root, self.function), "borrowed storage belongs to this function")));
                        }
                    }
                }
            }
        }
    }
}
