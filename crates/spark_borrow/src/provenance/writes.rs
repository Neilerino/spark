//! Current carrier contents written into incoming borrowed storage.

// TAG [SPARK-114]

use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_mir::BlockId;

use super::address::{Dependency, Endpoint, Intent, Selector};
use super::aliases::Port;
use super::demand::{Coordinate, Storage};
use super::{Flow, Path, Point, State, Value};

impl Flow<'_> {
    pub(super) fn store_references(
        &mut self,
        metadata: &super::value::Leaf,
        value: &Value,
        point: Point,
        state: &mut State,
    ) {
        let mut destinations = rustc_hash::FxHashSet::default();
        let mut exact = rustc_hash::FxHashSet::default();
        for endpoint in &metadata.endpoints {
            match endpoint {
                Endpoint::Frame {
                    function,
                    local,
                    location,
                    ..
                } if *function == self.function.def_id => {
                    destinations.insert(Coordinate {
                        storage: Storage::Cell(*local),
                        path: location.clone(),
                    });
                }
                Endpoint::Input(selector) => {
                    if let Some((port, path, whole)) = self.external_target(selector) {
                        let coordinate = Coordinate {
                            storage: Storage::External(port),
                            path,
                        };
                        if whole {
                            exact.insert(coordinate.clone());
                        }
                        destinations.insert(coordinate);
                    }
                }
                Endpoint::Frame { .. } => {}
            }
        }
        let unique = destinations.len() == 1 && metadata.opaque.is_empty();
        for destination in destinations {
            let Coordinate { storage, path } = &destination;
            let (root, mut current) = match storage {
                Storage::External(port) => (
                    port.pointee,
                    state.external.get(port).cloned().unwrap_or_default(),
                ),
                Storage::Cell(local) | Storage::Logical(local) => {
                    (self.function.locals[local.index()].ty, state.cell(*local))
                }
            };
            let strong = unique
                && match storage {
                    Storage::Cell(_) => path.is_empty(),
                    Storage::External(_) => exact.contains(&destination),
                    Storage::Logical(_) => false,
                };
            self.record_path_store(point, destination.clone(), value, strong);
            if strong {
                current
                    .leaves
                    .retain(|(stored, _), _| !super::storage::prefix(path, stored));
                current
                    .callees
                    .retain(|stored, _| !super::storage::prefix(path, stored));
            }
            current.join(&value.prefixed_path(path, root, self.table));
            current.sources.clear();
            match storage {
                Storage::External(port) => {
                    self.written_inputs.insert(port.clone());
                    state.modified_ports.insert(port.clone());
                    if strong {
                        state.external_defined.insert(port.clone());
                    }
                    state.external.insert(port.clone(), current);
                }
                Storage::Cell(local) | Storage::Logical(local) => {
                    State::set_value(&mut state.cells, *local, current.clone());
                    if state.live.contains(local) {
                        self.record_path_store(
                            point,
                            Coordinate {
                                storage: Storage::Logical(*local),
                                path: path.clone(),
                            },
                            value,
                            strong,
                        );
                        State::set_value(&mut state.values, *local, current);
                    }
                    if strong {
                        state.invalidated.retain(|(owner, stored), _| {
                            owner != local || !super::storage::prefix(path, stored)
                        });
                    }
                }
            }
        }
    }

    fn external_target(&self, selector: &Selector) -> Option<(Port, Path, bool)> {
        let port = Port::from_selector(selector, self.function, self.table)?;
        let representative = self.aliases.representative(&port);
        let projection = match &selector.intent {
            Intent::Address(payload) => payload.projection.clone(),
            Intent::Value | Intent::Payload(_) => Vec::new(),
        };
        let whole = port.exact() && representative.exact() && projection.is_empty();
        Some((representative, projection, whole))
    }

    pub(super) fn external_contents(&self, selector: &Selector, state: &State) -> (Value, bool) {
        let Some((port, path, _)) = self.external_target(selector) else {
            return (Value::default(), false);
        };
        let mut value = state
            .external
            .get(&port)
            .cloned()
            .unwrap_or_default()
            .sourced(&Storage::External(port.clone()));
        for slot in &path {
            value = value.project(slot);
        }
        (value, state.external_defined.contains(&port))
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

#[cfg(test)]
mod tests {
    use super::*;
    use spark_mir::{BasicBlock, Local, LocalDecl, MirFunction, Terminator};
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::{IntWidth, LifetimeId, Ty, TypeTable};

    #[test]
    fn physical_writes_preserve_consumed_logical_bindings() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let reference = table.intern(Ty::Ref {
            inner: int,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let span = Span::DUMMY;
        let function = MirFunction {
            def_id: DefId::new(9802),
            param_count: 0,
            return_type: int,
            locals: [int, reference]
                .into_iter()
                .enumerate()
                .map(|(index, ty)| LocalDecl {
                    local: Local(index as u32),
                    ty,
                    name: None,
                    is_user_variable: false,
                    mutable: true,
                    span,
                })
                .collect(),
            blocks: vec![BasicBlock {
                statements: vec![],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            is_async: false,
            span,
        };
        let point = Point::new(BlockId::ENTRY, 0);
        let targets = super::super::value::Leaf {
            endpoints: std::iter::once(Endpoint::Frame {
                function: function.def_id,
                root: point,
                local: Local(1),
                root_ty: reference,
                pointee: reference,
                location: Vec::new(),
            })
            .collect(),
            ..Default::default()
        };
        let value = Value::reference(super::super::Origin::Input {
            local: Local(1),
            path: Vec::new(),
        });
        for consumed in [false, true] {
            let mut state = State::default();
            state.set(Local(1), Value::default());
            if consumed {
                state.consume(Local(1));
            }
            let mut flow = Flow::new(&function, &table, None);
            flow.store_references(&targets, &value, point, &mut state);
            assert_eq!(state.value(Local(1)).is_empty(), consumed);
            assert_eq!(state.live.contains(&Local(1)), !consumed);
            assert_eq!(state.cell(Local(1)), value);
        }
    }
}
