//! Current carrier contents written into incoming borrowed storage.

// TAG [SPARK-114]

use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_mir::BlockId;

use super::address::{Dependency, Endpoint, Intent, Selector};
use super::aliases::{Port, View};
use super::demand::{Coordinate, Storage};
use super::{Flow, Path, Point, State, Value};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Coverage(rustc_hash::FxHashSet<View>);

impl Coverage {
    pub fn insert(&mut self, view: View) {
        if self
            .0
            .iter()
            .any(|defined| defined.root == view.root && view.path.starts_with(&defined.path))
        {
            return;
        }
        self.0
            .retain(|defined| defined.root != view.root || !defined.path.starts_with(&view.path));
        self.0.insert(view);
    }

    pub fn intersect(&mut self, other: &Self) -> bool {
        let mut common = Self::default();
        for left in &self.0 {
            for right in &other.0 {
                if left.root != right.root {
                    continue;
                }
                if left.path.starts_with(&right.path) {
                    common.insert(left.clone());
                } else if right.path.starts_with(&left.path) {
                    common.insert(right.clone());
                }
            }
        }
        let changed = *self != common;
        *self = common;
        changed
    }

    fn relative(&self, view: &View) -> Vec<Path> {
        self.0
            .iter()
            .filter_map(|defined| {
                if defined.root != view.root {
                    None
                } else if view.path.starts_with(&defined.path) {
                    Some(Vec::new())
                } else {
                    defined
                        .path
                        .strip_prefix(view.path.as_slice())
                        .map(<[super::Slot]>::to_vec)
                }
            })
            .collect()
    }
}

pub(super) fn owned_path(path: &[super::Slot]) -> bool {
    path.iter()
        .all(|slot| matches!(slot, super::Slot::Field(_) | super::Slot::Tuple(_)))
}

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
                    if let Some((view, exact_view)) = self.external_target(selector) {
                        let coordinate = Coordinate {
                            storage: Storage::External(view.root),
                            path: view.path,
                        };
                        if exact_view {
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
                    Storage::Cell(_) => owned_path(path),
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
                    let view = View {
                        root: port.clone(),
                        path: path.clone(),
                    };
                    self.written_inputs.insert(view.clone());
                    state.modified_views.insert(view.clone());
                    if strong {
                        state.external_defined.insert(view);
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

    pub(super) fn external_target(&self, selector: &Selector) -> Option<(View, bool)> {
        let port = Port::from_selector(selector, self.function, self.table)?;
        let mut view = View::whole(self.aliases.representative(&port));
        if let Intent::Address(payload) = &selector.intent {
            view.path.extend(payload.projection.iter().cloned());
        }
        view.path = super::value::normalize(view.root.pointee, view.path, self.table);
        let exact = port.exact() && view.root.exact() && owned_path(&view.path);
        Some((view, exact))
    }

    pub(super) fn external_contents(
        &self,
        selector: &Selector,
        state: &State,
    ) -> (Value, Vec<Path>) {
        let Some((view, _)) = self.external_target(selector) else {
            return (Value::default(), Vec::new());
        };
        let mut value = state
            .external
            .get(&view.root)
            .cloned()
            .unwrap_or_default()
            .sourced(&Storage::External(view.root.clone()));
        for slot in &view.path {
            value = value.project(slot);
        }
        (value, state.external_defined.relative(&view))
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
    fn coverage_join_preserves_the_common_subfield_not_the_whole_parent() {
        let root = Port::root(Local(1), spark_types::TypeId::ERROR);
        let field = View {
            root: root.clone(),
            path: vec![super::super::Slot::Tuple(0)],
        };
        let sibling = View {
            root: root.clone(),
            path: vec![super::super::Slot::Tuple(1)],
        };
        let mut whole = Coverage::default();
        whole.insert(View::whole(root.clone()));
        let mut partial = Coverage::default();
        partial.insert(field.clone());
        assert!(whole.intersect(&partial));
        assert_eq!(whole.relative(&View::whole(root)), vec![field.path.clone()]);
        assert_eq!(
            whole.relative(&field),
            vec![Vec::<super::super::Slot>::new()]
        );
        assert!(whole.relative(&sibling).is_empty());
        assert!(!whole.intersect(&partial));
        assert!(whole.intersect(&Coverage::default()));
        assert!(
            whole.0.is_empty(),
            "a path without the clear prevents a definite definition"
        );
    }

    #[test]
    fn whole_owner_coverage_does_not_define_an_independent_reference_pointee() {
        let root = Port::root(Local(1), spark_types::TypeId::ERROR);
        let pointee = Port {
            selection: vec![super::super::Slot::Deref, super::super::Slot::Tuple(0)],
            ..root.clone()
        };
        let mut covered = Coverage::default();
        covered.insert(View::whole(root));
        assert!(covered.relative(&View::whole(pointee)).is_empty());
    }

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
