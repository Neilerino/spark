// TAG [SPARK-114]

//! Address resolution and the pointee accesses performed by MIR reads.

use rustc_hash::FxHashSet;
use spark_mir::{Constant, Operand, Place, Projection};
use spark_types::{Ty, TypeId};

use super::observations::{Memory, ReadRole, ReadView, Sample};
use super::{child_type, projection_slot, value, Flow, Origin, Path, Point, Slot, State, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum AccessKind {
    Address,
    Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct ReadSite {
    pub function: spark_resolve::DefId,
    pub point: Point,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Access {
    pub site: Option<ReadSite>,
    pub origin: Origin,
    pub target: super::address::Dependency,
    pub selection: Path,
    pub kind: AccessKind,
}
#[derive(Clone, Debug, Default)]
pub(super) struct Accesses {
    events: FxHashSet<Access>,
    pub demands: super::demand::Frontier,
}

impl Accesses {
    pub fn observe_reference(
        &mut self,
        reference: &Value,
        reference_ty: Option<TypeId>,
        selection: &[Slot],
        kind: AccessKind,
    ) {
        self.observe_reference_at(reference, reference_ty, selection, kind, None);
    }

    pub fn observe_reference_at(
        &mut self,
        reference: &Value,
        reference_ty: Option<TypeId>,
        selection: &[Slot],
        kind: AccessKind,
        site: Option<ReadSite>,
    ) {
        use super::address::Dependency;
        self.demands.extend(reference.address_sources());
        for ((path, origin, _), leaf) in &reference.leaves {
            if !path.is_empty() && *path != [Slot::Tail] {
                continue;
            }
            for endpoint in &leaf.endpoints {
                self.insert(Access {
                    site,
                    origin: origin.clone(),
                    target: Dependency::Address(endpoint.clone()),
                    selection: selection.to_vec(),
                    kind,
                });
            }
            for dependency in &leaf.dependencies {
                let mut target = dependency.clone();
                if leaf.opaque.is_empty() {
                    if let Dependency::InputValue(selector, _) = &mut target {
                        if reference_ty.is_some() && selector.queue.is_none() {
                            selector.expected = reference_ty;
                        }
                    }
                }
                self.insert(Access {
                    site,
                    origin: origin.clone(),
                    target,
                    selection: selection.to_vec(),
                    kind,
                });
            }
        }
    }

    pub fn at_site(mut self, site: ReadSite) -> Self {
        self.events = self
            .events
            .into_iter()
            .map(|mut access| {
                access.site.get_or_insert(site);
                access
            })
            .collect();
        self
    }

    pub fn after_owner_returns(mut self, returns: &FxHashSet<super::address::OpaqueSite>) -> Self {
        use super::address::{Dependency, Endpoint, FrameLifetime};

        self.events = self
            .events
            .into_iter()
            .map(|mut access| {
                if let Dependency::Address(Endpoint::Frame { lifetime, .. }) = &mut access.target {
                    if let FrameLifetime::DuringCall {
                        boundary,
                        owner_return,
                    } = *lifetime
                    {
                        if boundary != owner_return && returns.contains(&owner_return) {
                            *lifetime = FrameLifetime::Returned(owner_return);
                        }
                    }
                }
                access
            })
            .collect();
        self
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
            demands: super::demand::Frontier::default(),
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

struct TailValues {
    writes: Vec<super::value::Leaf>,
    references: Value,
    channels: Value,
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
        self.dereference_in(value, ty, ReadView::current(state))
    }

    pub(super) fn dereference_in(&self, value: &Value, ty: TypeId, read: ReadView<'_>) -> Value {
        let inner = child_type(ty, &Slot::Deref, self.table).unwrap_or(TypeId::ERROR);
        self.read_pointee(value, inner, read)
    }

    pub(super) fn read_pointee(&self, value: &Value, inner: TypeId, read: ReadView<'_>) -> Value {
        let mut result = Value::default();
        for origin in value.addresses() {
            result.join(&self.pointee_in(&origin, inner, value, read));
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
        self.pointee_in(origin, ty, reference, ReadView::current(state))
    }

    fn pointee_in(
        &self,
        origin: &Origin,
        ty: TypeId,
        reference: &Value,
        read: ReadView<'_>,
    ) -> Value {
        use super::address::{Dependency, Endpoint};
        let mut result = Value::default();
        for ((path, candidate, _), leaf) in &reference.leaves {
            if candidate != origin || (!path.is_empty() && *path != [Slot::Tail]) {
                continue;
            }
            if leaf.has_validation_hole(origin) {
                result.join(&value::validation_hole(ty, self.table));
            }
            for endpoint in &leaf.endpoints {
                let payload = match endpoint {
                    Endpoint::Frame {
                        lifetime: crate::provenance::address::FrameLifetime::Local,
                        function,
                        local,
                        root_ty,
                        location,
                        ..
                    } if *function == self.function.def_id => self.read_memory(
                        Sample {
                            memory: Memory::Frame {
                                function: *function,
                                local: *local,
                                ty: *root_ty,
                            },
                            path: location.clone(),
                            ty,
                        },
                        read,
                    ),
                    Endpoint::Frame { .. } => {
                        value::tainted_from(ty, reference, self.table).without_reference_captures()
                    }
                    Endpoint::Input(selector, site) => self.symbolic_pointee_in(
                        selector,
                        ty,
                        read.with_bound_receive(site.as_deref()),
                    ),
                };
                result.join(&payload);
            }
            if leaf.opaque.is_empty() {
                for dependency in &leaf.dependencies {
                    if let Dependency::InputValue(selector, site) = dependency {
                        result.join(&self.symbolic_pointee_in(
                            selector,
                            ty,
                            read.with_bound_receive(site.as_deref()),
                        ));
                    }
                }
            }
            if !leaf.opaque.is_empty() {
                result.join(
                    &value::tainted_from(ty, reference, self.table).without_reference_captures(),
                );
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
        self.symbolic_pointee_in(selector, ty, ReadView::current(state))
    }

    fn symbolic_pointee_in(
        &self,
        selector: &super::address::Selector,
        ty: TypeId,
        read: ReadView<'_>,
    ) -> Value {
        use super::address::{DerefKind, Intent, Selector, TypedPath};
        let source_ty = self.function.locals[selector.input.index()].ty;
        let mut basis = selector.clone();
        if let Intent::Payload(payload) = &selector.intent {
            basis.select_payload(payload, source_ty, self.table);
        }
        let (selection, payload) = match &selector.intent {
            Intent::Address(payload) => (selector.selection.clone(), payload.clone()),
            Intent::Value => (
                selector.selection.clone(),
                TypedPath {
                    kind: DerefKind::All,
                    pointee: ty,
                    projection: Vec::new(),
                },
            ),
            Intent::Payload(_) => (
                basis.selection.clone(),
                TypedPath {
                    kind: DerefKind::All,
                    pointee: ty,
                    projection: Vec::new(),
                },
            ),
        };
        let fact_path = if let Intent::Payload(payload) = &selector.intent {
            [payload.projection.clone(), vec![Slot::Deref]].concat()
        } else {
            Vec::new()
        };
        let qualified = if matches!(&selector.intent, Intent::Payload(_)) {
            selector
                .capture
                .map(|capture| super::callbacks::FactSource {
                    input: selector.input,
                    capture: Some(capture),
                    prefix: &fact_path,
                })
        } else {
            None
        };
        let address = Selector {
            channel_capture: None,
            queue: basis.queue,
            capture: selector.address_capture(),
            input: selector.input,
            selection,
            intent: Intent::Address(payload),
            expected: Some(ty),
        };
        let mut result = Value::default();
        for (view, _) in self.external_targets(&address) {
            let mut address = view.root.selector();
            if let Intent::Address(payload) = &mut address.intent {
                payload.projection = view.path;
                if !self.aliases.is_cell(&view.root) {
                    payload.kind = DerefKind::Residual;
                }
            }
            result.join(&self.symbolic_cell_pointee(&address, ty, read, qualified));
        }
        result
    }

    fn symbolic_cell_pointee(
        &self,
        address: &super::address::Selector,
        ty: TypeId,
        read: ReadView<'_>,
        qualified: Option<super::callbacks::FactSource<'_>>,
    ) -> Value {
        use super::address::{Dependency, Intent, Selector, TypedPath};
        let Intent::Address(payload) = &address.intent else {
            return Value::default();
        };
        let source_ty = self.function.locals[address.input.index()].ty;
        let selection = &address.selection;
        let (stored, covered) = self.external_contents(address, ty, read);
        if covered.iter().any(Vec::is_empty) {
            return stored;
        }
        let prefix = value::normalize(
            source_ty,
            [
                selection.clone(),
                vec![payload.kind.slot()],
                payload.projection.clone(),
            ]
            .concat(),
            self.table,
        );
        let mut result = self.symbolic_pointee_input(ty, source_ty, &prefix, address, qualified);
        self.qualify_queued_pointee_handles(&mut result, address, payload);
        result
            .leaves
            .retain(|(path, _, _), _| !covered.iter().any(|prefix| path.starts_with(prefix)));
        result
            .callees
            .retain(|path, _| !covered.iter().any(|prefix| path.starts_with(prefix)));
        result
            .queues
            .retain(|path, _| !covered.iter().any(|prefix| path.starts_with(prefix)));
        result
            .futures
            .retain(|path, _| !covered.iter().any(|prefix| path.starts_with(prefix)));
        for ((path, _, _), leaf) in &mut result.leaves {
            leaf.dependencies.clear();
            leaf.dependencies.insert(Dependency::InputValue(
                Selector {
                    channel_capture: None,
                    queue: address.queue.clone(),
                    capture: address.capture,
                    input: address.input,
                    selection: selection.clone(),
                    expected: Some(self.selected_type(ty, path)),
                    intent: Intent::Payload(TypedPath {
                        kind: payload.kind,
                        pointee: payload.pointee,
                        projection: value::normalize(
                            payload.pointee,
                            [payload.projection.clone(), path.clone()].concat(),
                            self.table,
                        ),
                    }),
                },
                read.receive(),
            ));
        }
        result.join(&stored);
        result
    }

    pub(super) fn project_path(
        &self,
        mut value: Value,
        mut ty: TypeId,
        path: &Path,
        read: ReadView<'_>,
    ) -> Value {
        for (index, slot) in path.iter().enumerate() {
            let read = read.role(ReadRole::Projection(index));
            if *slot == Slot::Tail {
                return value;
            }
            value = if *slot == Slot::Element
                && matches!(
                    self.table.resolve(value::strip_mutable(ty, self.table)),
                    Ty::Channel { .. }
                ) {
                self.queue_payload_in(
                    &value,
                    child_type(ty, slot, self.table).unwrap_or(TypeId::ERROR),
                    read,
                )
            } else if *slot == Slot::Deref {
                self.dereference_in(&value, ty, read)
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
        self.tail_values(value, ty, expected, state).references
    }

    pub(super) fn tail_write_targets(
        &self,
        value: Value,
        ty: TypeId,
        state: &State,
    ) -> Vec<super::value::Leaf> {
        self.tail_values(value, ty, None, state).writes
    }

    pub(super) fn tail_channel_value(
        &self,
        value: Value,
        ty: TypeId,
        expected: TypeId,
        state: &State,
    ) -> Value {
        self.tail_values(value, ty, Some(expected), state).channels
    }

    fn may_write_reference(&self, ty: TypeId) -> bool {
        match self.table.resolve(value::strip_mutable(ty, self.table)) {
            Ty::Ref { mutable, .. } => *mutable || matches!(self.table.resolve(ty), Ty::Mutable(_)),
            _ => true,
        }
    }

    fn tail_values(
        &self,
        value: Value,
        ty: TypeId,
        expected: Option<TypeId>,
        state: &State,
    ) -> TailValues {
        let mut result = Value::default();
        let mut channels = Value::default();
        let mut writes = Vec::new();
        let mut pending = vec![(value, ty)];
        let mut seen: rustc_hash::FxHashMap<_, Value> = rustc_hash::FxHashMap::default();
        while let Some((value, ty)) = pending.pop() {
            for target in value.queues.values().flatten() {
                if expected.is_none_or(|expected| {
                    matches!(
                        spark_types::check_expr::cast_kind(target.ty(), expected, self.table),
                        Ok(spark_types::check_expr::CastKind::Identity
                            | spark_types::check_expr::CastKind::Deferred)
                    )
                }) {
                    channels
                        .queues
                        .entry(Vec::new())
                        .or_default()
                        .insert(target.clone());
                }
            }
            for ((path, origin, capture), leaf) in &value.leaves {
                let selected = self.selected_type(ty, path);
                let writable = self.may_write_reference(selected);

                let sources = value
                    .sources
                    .get(&(path.clone(), origin.clone(), *capture))
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
                    if writable {
                        writes.push(leaf.clone());
                    }
                    result.insert_contribution(Vec::new(), origin.clone(), *capture, leaf);
                    result.insert_source_contribution(
                        Vec::new(),
                        origin.clone(),
                        *capture,
                        sources,
                    );
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
                        result.insert_contribution(Vec::new(), origin.clone(), *capture, leaf);
                        result.insert_source_contribution(
                            Vec::new(),
                            origin.clone(),
                            *capture,
                            sources.clone(),
                        );
                    }
                    let mut reference = Value::default();
                    reference.insert_contribution(Vec::new(), origin.clone(), *capture, leaf);
                    reference.insert_source_contribution(
                        Vec::new(),
                        origin.clone(),
                        *capture,
                        sources.clone(),
                    );
                    if writable {
                        writes.push(self.project_reference(&reference, pointee, &Vec::new()));
                    }
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
        TailValues {
            writes,
            references: result,
            channels,
        }
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
                Endpoint::Input(selector, _) => {
                    if let Some(expected) = selector.expected {
                        types.insert(expected);
                    }
                }
            }
        }
        if leaf.opaque.is_empty() {
            for dependency in &leaf.dependencies {
                if let Dependency::InputValue(selector, _) = dependency {
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
