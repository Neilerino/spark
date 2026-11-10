//! Current carrier contents written into incoming borrowed storage.

// TAG [SPARK-114]

use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_mir::BlockId;

use super::address::{DerefKind, Endpoint, Intent, Selector};
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

#[derive(Default)]
struct WriteTargets {
    all: rustc_hash::FxHashSet<Coordinate>,
    physical: rustc_hash::FxHashSet<Coordinate>,
    exact: rustc_hash::FxHashSet<Coordinate>,
    unknown: bool,
}

impl Flow<'_> {
    pub(super) fn possibly_written_locals(
        &mut self,
        metadata: &super::value::Leaf,
        state: &State,
    ) -> rustc_hash::FxHashSet<spark_mir::Local> {
        let targets = self.write_targets(metadata);
        for coordinate in &targets.all {
            if let Storage::External(port) = &coordinate.storage {
                self.written_inputs.insert(View {
                    root: port.clone(),
                    path: coordinate.path.clone(),
                });
            }
        }
        if targets.unknown {
            self.unknown_copy_clobber = true;
            return state.live.clone();
        }
        targets
            .all
            .into_iter()
            .filter_map(|coordinate| match coordinate.storage {
                Storage::Cell(local) | Storage::Logical(local) => Some(local),
                _ => None,
            })
            .collect()
    }

    pub(super) fn store_references(
        &mut self,
        metadata: &super::value::Leaf,
        value: &Value,
        ty: spark_types::TypeId,
        point: Point,
        state: &mut State,
    ) {
        self.ensure_transfer(point, state);
        let value = &self.freeze_sources(point, value.clone());
        let targets = self.write_targets(metadata);
        if targets.unknown {
            let reachable = self.reachable_queue_payloads(value, state);
            self.record_obligation(point, super::obligations::Boundary::Store(ty), &reachable);
        }
        if targets.unknown
            || targets
                .all
                .iter()
                .any(|target| matches!(target.storage, Storage::External(_)))
        {
            self.expose_queue_handles(value, state);
        }
        let unique = targets.physical.len() == 1 && !targets.unknown;
        for destination in targets.all {
            let Coordinate { storage, path } = &destination;
            let (root, mut current) = self.stored_carrier(storage, state);
            let strong = unique
                && match storage {
                    Storage::Cell(_) => owned_path(path),
                    Storage::External(_) => targets.exact.contains(&destination),
                    Storage::Logical(_) | Storage::Queue(_, _) | Storage::Task(_, _) => false,
                };
            self.record_path_store(point, destination.clone(), value, strong);
            if strong {
                current
                    .leaves
                    .retain(|(stored, _, _), _| !super::storage::prefix(path, stored));
                current
                    .callees
                    .retain(|stored, _| !super::storage::prefix(path, stored));
                current
                    .queues
                    .retain(|stored, _| !super::storage::prefix(path, stored));
                current
                    .futures
                    .retain(|stored, _| !super::storage::prefix(path, stored));
                current
                    .task_views
                    .retain(|stored, _| !super::storage::prefix(path, stored));
            }
            current.join(&value.prefixed_path(path, root, self.table));
            current.sources.clear();
            match storage {
                Storage::Task(target, ty) => {
                    let record = super::tasks::Record {
                        callback: current,
                        callback_ty: *ty,
                    };
                    state.tasks.insert(target.clone(), record);
                }
                Storage::Queue(target, ty) => {
                    state.queues.insert((target.clone(), *ty), current);
                }
                Storage::External(port) => {
                    let view = View {
                        root: port.clone(),
                        path: path.clone(),
                    };
                    if targets.physical.contains(&destination) {
                        self.written_inputs.insert(view.clone());
                        state.modified_views.insert(view.clone());
                    }
                    if strong {
                        state.external_defined.insert(view);
                    }
                    state.external.insert(port.clone(), current);
                }
                Storage::Cell(local) | Storage::Logical(local) => {
                    state.future_copies.forget(*local);
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

    fn stored_carrier(&self, storage: &Storage, state: &State) -> (spark_types::TypeId, Value) {
        match storage {
            Storage::Task(target, ty) => (
                *ty,
                state
                    .tasks
                    .get(target)
                    .map(|record| record.callback.clone())
                    .unwrap_or_default(),
            ),
            Storage::Queue(target, ty) => (
                *ty,
                state
                    .queues
                    .get(&(target.clone(), *ty))
                    .cloned()
                    .unwrap_or_default(),
            ),
            Storage::External(port) => (
                port.pointee,
                state.external.get(port).cloned().unwrap_or_default(),
            ),
            Storage::Cell(local) | Storage::Logical(local) => {
                (self.function.locals[local.index()].ty, state.cell(*local))
            }
        }
    }

    fn write_targets(&self, metadata: &super::value::Leaf) -> WriteTargets {
        let mut targets = WriteTargets {
            unknown: !metadata.opaque.is_empty(),
            ..Default::default()
        };
        for endpoint in &metadata.endpoints {
            match endpoint {
                Endpoint::Frame {
                    function,
                    local,
                    location,
                    ..
                } if *function == self.function.def_id => {
                    let coordinate = Coordinate {
                        storage: Storage::Cell(*local),
                        path: location.clone(),
                    };
                    targets.physical.insert(coordinate.clone());
                    targets.all.insert(coordinate);
                }
                Endpoint::Input(selector) => {
                    for (view, exact_view) in self.external_targets(selector) {
                        let coordinate = Coordinate {
                            storage: Storage::External(view.root.clone()),
                            path: view.path.clone(),
                        };
                        if exact_view {
                            targets.exact.insert(coordinate.clone());
                        }
                        targets.physical.insert(coordinate.clone());
                        targets.all.insert(coordinate);
                        if !self.aliases.is_cell(&view.root) {
                            for peer in self.residual_peers(&view.root) {
                                targets.all.insert(Coordinate {
                                    storage: Storage::External(peer),
                                    path: view.path.clone(),
                                });
                            }
                        }
                    }
                }
                Endpoint::Frame { .. } => targets.unknown = true,
            }
        }
        targets
    }

    pub(super) fn external_targets(&self, selector: &Selector) -> Vec<(View, bool)> {
        let Some((view, exact)) = self.external_target(selector) else {
            return Vec::new();
        };
        if self.aliases.is_cell(&view.root) {
            return vec![(view, exact)];
        }
        let family = self.family(&view.root);
        let residual_only = matches!(&selector.intent, Intent::Address(payload) if payload.kind == DerefKind::Residual);
        let single =
            family.roots.len() == 1 && !family.residual && !residual_only && owned_path(&view.path);
        let mut targets = Vec::new();
        if !residual_only {
            targets.extend(family.roots.into_iter().map(|root| {
                (
                    View {
                        root,
                        path: view.path.clone(),
                    },
                    single,
                )
            }));
        }
        if family.residual {
            targets.push((view, false));
        }
        targets
    }

    pub(super) fn external_target(&self, selector: &Selector) -> Option<(View, bool)> {
        let port = Port::from_selector(selector, self.function, self.table)?;
        let mut view = View::whole(self.aliases.representative(&port));
        if let Intent::Address(payload) = &selector.intent {
            view.path.extend(payload.projection.iter().cloned());
        }
        view.path = super::value::normalize(view.root.pointee, view.path, self.table);
        let exact = self.aliases.is_cell(&port)
            && self.aliases.is_cell(&view.root)
            && owned_path(&view.path);
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
            let value = self.reachable_queue_payloads(value, state);
            for root in value.frame_sources(self.function.def_id) {
                if reported.insert(root) {
                    let exit = Point::new(
                        block,
                        self.function.blocks[block.index()].statements.len() as u32,
                    );
                    diagnostics.push(
                        Diagnostic::error("stored value contains a reference to local storage")
                            .with_label(Label::primary(
                                crate::errors::span_for_point(exit, self.function),
                                "borrowed storage retains this reference when the function returns",
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
            kind: crate::provenance::aliases::StorageKind::Reference,
            channel_capture: None,
            queue: None,
            capture: None,
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
            flow.store_references(&targets, &value, reference, point, &mut state);
            assert_eq!(state.value(Local(1)).is_empty(), consumed);
            assert_eq!(state.live.contains(&Local(1)), !consumed);
            assert_eq!(state.cell(Local(1)), value);
        }
    }
}
