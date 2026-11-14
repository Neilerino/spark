//! Address resolution and the pointee accesses performed by MIR reads.

use rustc_hash::FxHashSet;
use spark_mir::{Constant, Operand, Place, Projection};
use spark_types::{Ty, TypeId};

use super::{child_type, projection_slot, value, Flow, Origin, Path, Slot, State, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum AccessKind {
    Address,
    Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Access {
    pub origin: Origin,
    pub target: super::address::Dependency,
    pub selection: Path,
    pub kind: AccessKind,
}
#[derive(Clone, Debug, Default)]
pub(super) struct Accesses {
    events: FxHashSet<Access>,
    pub demands: super::demand::Demand,
}

impl Accesses {
    pub fn observe_reference(
        &mut self,
        reference: &Value,
        reference_ty: Option<TypeId>,
        selection: &[Slot],
        kind: AccessKind,
    ) {
        use super::address::Dependency;
        self.demands.extend(reference.address_sources());
        for ((path, origin), leaf) in &reference.leaves {
            if !path.is_empty() && *path != [Slot::Tail] {
                continue;
            }
            for endpoint in &leaf.endpoints {
                self.insert(Access {
                    origin: origin.clone(),
                    target: Dependency::Address(endpoint.clone()),
                    selection: selection.to_vec(),
                    kind,
                });
            }
            for dependency in &leaf.dependencies {
                let mut target = dependency.clone();
                if leaf.opaque.is_empty() {
                    if let Dependency::InputValue(selector) = &mut target {
                        if reference_ty.is_some() {
                            selector.expected = reference_ty;
                        }
                    }
                }
                self.insert(Access {
                    origin: origin.clone(),
                    target,
                    selection: selection.to_vec(),
                    kind,
                });
            }
        }
    }

    pub fn insert(&mut self, access: Access) {
        self.events.insert(access);
    }
    pub fn iter(&self) -> std::collections::hash_set::Iter<'_, Access> {
        self.events.iter()
    }
    pub fn len(&self) -> usize {
        self.events.len()
    }
    pub fn merge(&mut self, other: Self) {
        self.events.extend(other.events);
        self.demands.extend(other.demands);
    }
}

impl Extend<Access> for Accesses {
    fn extend<T: IntoIterator<Item = Access>>(&mut self, values: T) {
        self.events.extend(values);
    }
}
impl FromIterator<Access> for Accesses {
    fn from_iter<T: IntoIterator<Item = Access>>(values: T) -> Self {
        Self {
            events: values.into_iter().collect(),
            demands: super::demand::Demand::default(),
        }
    }
}
impl<'a> IntoIterator for &'a Accesses {
    type Item = &'a Access;
    type IntoIter = std::collections::hash_set::Iter<'a, Access>;
    fn into_iter(self) -> Self::IntoIter {
        self.events.iter()
    }
}

impl Flow<'_> {
    pub(super) fn operand(&self, operand: &Operand, state: &State) -> Value {
        match operand {
            Operand::Copy(place) | Operand::Move(place) => self.read(place, state),
            Operand::Constant(Constant::FunctionRef(def)) => Value::function(*def),
            Operand::Constant(_) => Value::default(),
        }
    }

    pub(super) fn read(&self, place: &Place, state: &State) -> Value {
        self.read_access(place, state, AccessKind::Value, &mut Accesses::default())
    }

    pub(super) fn read_operand(
        &self,
        operand: &Operand,
        state: &State,
        accesses: &mut Accesses,
    ) -> Value {
        match operand {
            Operand::Copy(place) | Operand::Move(place) => {
                self.read_access(place, state, AccessKind::Value, accesses)
            }
            Operand::Constant(Constant::FunctionRef(def)) => Value::function(*def),
            Operand::Constant(_) => Value::default(),
        }
    }

    pub(super) fn read_access(
        &self,
        place: &Place,
        state: &State,
        kind: AccessKind,
        accesses: &mut Accesses,
    ) -> Value {
        let mut value = state
            .value(place.local)
            .sourced(&super::demand::Storage::Logical(place.local));
        let mut ty = self.function.locals[place.local.index()].ty;
        for (index, projection) in place.projections.iter().enumerate() {
            let slot = projection_slot(projection, ty, self.table);
            value = if slot == Slot::Deref {
                let inner = child_type(ty, &slot, self.table).unwrap_or(TypeId::ERROR);
                let remaining = &place.projections[index + 1..];
                let count = remaining
                    .iter()
                    .position(|p| matches!(p, Projection::Deref))
                    .unwrap_or(remaining.len());
                let (selection, _) = self.projection_path(inner, &remaining[..count]);
                accesses.observe_reference(&value, Some(ty), &selection, kind);
                self.dereference(&value, ty, state)
            } else {
                value.project(&slot)
            };
            ty = child_type(ty, &slot, self.table).unwrap_or(TypeId::ERROR);
        }
        value
    }

    pub(super) fn dereference(&self, value: &Value, ty: TypeId, state: &State) -> Value {
        let inner = child_type(ty, &Slot::Deref, self.table).unwrap_or(TypeId::ERROR);
        self.read_pointee(value, inner, state)
    }

    pub(super) fn read_pointee(&self, value: &Value, inner: TypeId, state: &State) -> Value {
        let mut result = Value::default();
        for origin in value.addresses() {
            result.join(&self.pointee(&origin, inner, value, state));
        }
        result
    }

    pub(super) fn pointee(
        &self,
        origin: &Origin,
        ty: TypeId,
        reference: &Value,
        state: &State,
    ) -> Value {
        use super::address::{Dependency, Endpoint};
        let mut result = Value::default();
        for path in [Vec::new(), vec![Slot::Tail]] {
            let Some(leaf) = reference.leaves.get(&(path, origin.clone())) else {
                continue;
            };
            for endpoint in &leaf.endpoints {
                let payload = match endpoint {
                    Endpoint::Frame {
                        function,
                        local,
                        root_ty,
                        location,
                        ..
                    } if *function == self.function.def_id => {
                        let value = self.project_path(
                            state
                                .cell(*local)
                                .sourced(&super::demand::Storage::Cell(*local)),
                            *root_ty,
                            location,
                            state,
                        );
                        if location.contains(&Slot::Tail) {
                            value::tainted_from(ty, &value, self.table)
                        } else {
                            value
                        }
                    }
                    Endpoint::Frame { .. } => value::tainted_from(ty, reference, self.table),
                    Endpoint::Input(selector) => self.symbolic_pointee(selector, ty, state),
                };
                result.join(&payload);
            }
            if leaf.opaque.is_empty() {
                for dependency in &leaf.dependencies {
                    if let Dependency::InputValue(selector) = dependency {
                        result.join(&self.symbolic_pointee(selector, ty, state));
                    }
                }
            }
            if !leaf.opaque.is_empty() {
                result.join(&value::tainted_from(ty, reference, self.table));
            }
        }
        result
    }

    pub(super) fn symbolic_pointee(
        &self,
        selector: &super::address::Selector,
        ty: TypeId,
        state: &State,
    ) -> Value {
        use super::address::{Dependency, Intent, Selector, TypedPath};
        let source_ty = self.function.locals[selector.input.index()].ty;
        let (selection, payload) = match &selector.intent {
            Intent::Address(payload) => (selector.selection.clone(), payload.clone()),
            Intent::Value => (
                selector.selection.clone(),
                TypedPath {
                    pointee: ty,
                    projection: Vec::new(),
                },
            ),
            Intent::Payload(payload) => (
                value::normalize(
                    source_ty,
                    [
                        selector.selection.clone(),
                        vec![Slot::Deref],
                        payload.projection.clone(),
                    ]
                    .concat(),
                    self.table,
                ),
                TypedPath {
                    pointee: ty,
                    projection: Vec::new(),
                },
            ),
        };
        let address = Selector {
            input: selector.input,
            selection: selection.clone(),
            intent: Intent::Address(payload.clone()),
            expected: Some(ty),
        };
        let (stored, complete) = self.external_contents(&address, state);
        if complete {
            return stored;
        }
        let prefix = value::normalize(
            source_ty,
            [
                selection.clone(),
                vec![Slot::Deref],
                payload.projection.clone(),
            ]
            .concat(),
            self.table,
        );
        let mut result = value::inputs(ty, selector.input, source_ty, &prefix, self.table);
        for ((path, _), leaf) in &mut result.leaves {
            leaf.dependencies.clear();
            leaf.dependencies.insert(Dependency::InputValue(Selector {
                input: selector.input,
                selection: selection.clone(),
                expected: Some(self.selected_type(ty, path)),
                intent: Intent::Payload(TypedPath {
                    pointee: payload.pointee,
                    projection: value::normalize(
                        payload.pointee,
                        [payload.projection.clone(), path.clone()].concat(),
                        self.table,
                    ),
                }),
            }));
        }
        result.join(&stored);
        result
    }

    pub(super) fn project_path(
        &self,
        mut value: Value,
        mut ty: TypeId,
        path: &Path,
        state: &State,
    ) -> Value {
        for slot in path {
            if *slot == Slot::Tail {
                return value;
            }
            value = if *slot == Slot::Deref {
                self.dereference(&value, ty, state)
            } else {
                value.project(slot)
            };
            ty = child_type(ty, slot, self.table).unwrap_or(TypeId::ERROR);
        }
        value
    }

    pub(super) fn projection_path(
        &self,
        mut ty: TypeId,
        projections: &[Projection],
    ) -> (Path, TypeId) {
        let mut path = Vec::new();
        for projection in projections {
            let slot = projection_slot(projection, ty, self.table);
            ty = child_type(ty, &slot, self.table).unwrap_or(TypeId::ERROR);
            path.push(slot);
        }
        (path, ty)
    }

    pub(super) fn place_type(&self, place: &Place) -> TypeId {
        self.projection_path(
            self.function.locals[place.local.index()].ty,
            &place.projections,
        )
        .1
    }
    pub(super) fn tail_reference_value(
        &self,
        value: Value,
        ty: TypeId,
        expected: Option<TypeId>,
        state: &State,
    ) -> Value {
        let mut result = Value::default();
        let mut pending = vec![(value, ty)];
        let mut seen: rustc_hash::FxHashMap<_, Value> = rustc_hash::FxHashMap::default();
        while let Some((value, ty)) = pending.pop() {
            for ((path, origin), leaf) in &value.leaves {
                let selected = self.selected_type(ty, path);

                let sources = value
                    .sources
                    .get(&(path.clone(), origin.clone()))
                    .cloned()
                    .unwrap_or_default();
                let pointees = if let Ty::Ref { inner, .. } = self
                    .table
                    .resolve(value::strip_mutable(selected, self.table))
                {
                    vec![*inner]
                } else {
                    self.pointee_types(leaf)
                };
                if pointees.is_empty() {
                    result.insert_leaf(Vec::new(), origin.clone(), leaf);
                    result.insert_sourced(Vec::new(), origin.clone(), sources);
                    continue;
                }
                for pointee in pointees {
                    if expected.is_none_or(|expected| {
                        matches!(
                            spark_types::check_expr::cast_kind(pointee, expected, self.table),
                            Ok(spark_types::check_expr::CastKind::Identity
                                | spark_types::check_expr::CastKind::Deferred)
                        )
                    }) {
                        result.insert_leaf(Vec::new(), origin.clone(), leaf);
                        result.insert_sourced(Vec::new(), origin.clone(), sources.clone());
                    }
                    let mut reference = Value::default();
                    reference.insert_leaf(Vec::new(), origin.clone(), leaf);
                    reference.insert_sourced(Vec::new(), origin.clone(), sources.clone());
                    let context = seen.entry((origin.clone(), pointee)).or_default();
                    if !context.join(&reference) {
                        continue;
                    }
                    let payload = self.pointee(origin, pointee, context, state);
                    if !payload.is_empty() {
                        pending.push((payload, pointee));
                    }
                }
            }
        }
        result
    }

    pub(super) fn selected_type(&self, mut ty: TypeId, path: &[Slot]) -> TypeId {
        for slot in path {
            if *slot == Slot::Tail {
                return TypeId::ERROR;
            }
            ty = child_type(ty, slot, self.table).unwrap_or(TypeId::ERROR);
        }
        ty
    }

    fn pointee_types(&self, leaf: &super::value::Leaf) -> Vec<TypeId> {
        use super::address::{Dependency, Endpoint};
        let mut types = FxHashSet::default();
        for endpoint in &leaf.endpoints {
            match endpoint {
                Endpoint::Frame { pointee, .. } => {
                    types.insert(*pointee);
                }
                Endpoint::Input(selector) => {
                    if let Some(expected) = selector.expected {
                        types.insert(expected);
                    }
                }
            }
        }
        if leaf.opaque.is_empty() {
            for dependency in &leaf.dependencies {
                if let Dependency::InputValue(selector) = dependency {
                    let ty = selector.expected.unwrap_or_else(|| {
                        if let super::address::Intent::Payload(payload) = &selector.intent {
                            self.selected_type(payload.pointee, &payload.projection)
                        } else {
                            self.selected_type(
                                self.function.locals[selector.input.index()].ty,
                                &selector.selection,
                            )
                        }
                    });
                    if let Ty::Ref { inner, .. } =
                        self.table.resolve(value::strip_mutable(ty, self.table))
                    {
                        types.insert(*inner);
                    }
                }
            }
        }
        types.remove(&TypeId::ERROR);
        types.into_iter().collect()
    }
}

#[cfg(test)]
mod tests;
