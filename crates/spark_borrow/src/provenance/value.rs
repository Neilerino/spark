// TAG [SPARK-114]

use rustc_hash::{FxHashMap, FxHashSet};
use spark_intern::Symbol;
use spark_resolve::DefId;
use spark_types::{Ty, TypeId, TypeTable};

use super::address::{Dependency, Endpoint, Intent, OpaqueSite, Selector};
use super::demand::{Atom, Coordinate, Source, Sources, Storage};
use super::{Origin, Origins};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Slot {
    Element,
    Key,
    Value,
    Field(Symbol),
    Tuple(usize),
    Variant(DefId, usize),
    Member(TypeId),
    Optional,
    Deref,
    ResidualDeref,
    Capture(DefId, usize, TypeId),
    Tail,
}

pub(super) type Path = Vec<Slot>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct ReferenceCapture {
    pub owner: DefId,
    pub index: usize,
    pub ty: TypeId,
}

pub(super) type Contribution = (Path, Origin, Option<ReferenceCapture>);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Leaf {
    pub parents: Origins,
    pub endpoints: FxHashSet<Endpoint>,
    pub dependencies: FxHashSet<Dependency>,
    pub opaque: FxHashSet<OpaqueSite>,
}

impl Leaf {
    pub fn has_validation_hole(&self, origin: &Origin) -> bool {
        *origin == Origin::ValidationHole || self.parents.contains(&Origin::ValidationHole)
    }

    pub fn is_validation_only(&self, origin: &Origin) -> bool {
        self.has_validation_hole(origin)
            && self.endpoints.is_empty()
            && self.dependencies.is_empty()
            && self.opaque.is_empty()
    }

    pub fn join(&mut self, other: &Self) -> bool {
        let before = (
            self.parents.len(),
            self.endpoints.len(),
            self.dependencies.len(),
            self.opaque.len(),
        );
        self.parents.extend(other.parents.iter().cloned());
        self.endpoints.extend(other.endpoints.iter().cloned());
        self.dependencies.extend(other.dependencies.iter().cloned());
        self.opaque.extend(other.opaque.iter().copied());
        before
            != (
                self.parents.len(),
                self.endpoints.len(),
                self.dependencies.len(),
                self.opaque.len(),
            )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Value {
    pub leaves: FxHashMap<Contribution, Leaf>,
    pub sources: Sources,
    pub callees: FxHashMap<Path, super::callbacks::Targets>,
    pub queues: FxHashMap<Path, FxHashSet<super::channels::Handle>>,
    pub futures: FxHashMap<Path, FxHashSet<super::tasks::Handle>>,
    pub task_views: super::tasks::views::Views,
}

impl Value {
    pub(super) fn task_lineage(&self) -> Self {
        let mut result = Self::default();
        for ((_, origin, capture), leaf) in &self.leaves {
            result.insert_contribution(vec![Slot::Tail], origin.clone(), *capture, leaf);
        }
        for ((_, origin, capture), sources) in &self.sources {
            result.insert_source_contribution(
                vec![Slot::Tail],
                origin.clone(),
                *capture,
                sources.clone(),
            );
        }
        result
    }
    pub fn materialize_validation_holes(&mut self) {
        let paths: Vec<_> = self
            .leaves
            .iter()
            .filter(|((_, origin, _), leaf)| leaf.has_validation_hole(origin))
            .map(|((path, _, _), _)| path.clone())
            .collect();
        for path in paths {
            self.insert_leaf(path, Origin::ValidationHole, &Leaf::default());
        }
    }

    pub fn frame_sources(&self, function: DefId) -> impl Iterator<Item = super::Point> + '_ {
        self.leaves
            .values()
            .flat_map(|leaf| {
                leaf.endpoints
                    .iter()
                    .chain(
                        leaf.dependencies
                            .iter()
                            .filter_map(|dependency| match dependency {
                                Dependency::Address(endpoint) => Some(endpoint),
                                Dependency::InputValue(..) => None,
                            }),
                    )
            })
            .filter_map(move |endpoint| match endpoint {
                Endpoint::Frame {
                    lifetime: crate::provenance::address::FrameLifetime::Local,
                    function: owner,
                    root,
                    ..
                } if *owner == function => Some(*root),
                Endpoint::Frame { .. } | Endpoint::Input(..) => None,
            })
    }

    pub fn reference(origin: Origin) -> Self {
        let mut value = Self::default();
        value.insert(&[], std::iter::once(origin).collect());
        value
    }

    pub fn insert(&mut self, path: &[Slot], origins: Origins) {
        for origin in origins {
            self.leaves
                .entry((path.to_vec(), origin, None))
                .or_default();
        }
    }

    pub fn insert_leaf(&mut self, path: Path, origin: Origin, leaf: &Leaf) {
        self.insert_contribution(path, origin, None, leaf);
    }

    pub fn insert_contribution(
        &mut self,
        path: Path,
        origin: Origin,
        capture: Option<ReferenceCapture>,
        leaf: &Leaf,
    ) {
        self.leaves
            .entry((path, origin, capture))
            .or_default()
            .join(leaf);
    }

    pub fn insert_sourced(&mut self, path: Path, origin: Origin, sources: super::demand::Frontier) {
        self.insert_source_contribution(path, origin, None, sources);
    }

    pub fn insert_source_contribution(
        &mut self,
        path: Path,
        origin: Origin,
        capture: Option<ReferenceCapture>,
        sources: super::demand::Frontier,
    ) {
        self.leaves
            .entry((path.clone(), origin.clone(), capture))
            .or_default();
        if !sources.is_empty() {
            self.sources
                .entry((path, origin, capture))
                .or_default()
                .extend(sources);
        }
    }

    pub fn without_reference_captures(&self) -> Self {
        let mut result = Self {
            callees: self.callees.clone(),
            queues: self.queues.clone(),
            futures: self.futures.clone(),
            task_views: self.task_views.clone(),
            ..Self::default()
        };
        for ((path, origin, _), leaf) in &self.leaves {
            result.insert_leaf(path.clone(), origin.clone(), leaf);
        }
        for ((path, origin, _), sources) in &self.sources {
            result.insert_sourced(path.clone(), origin.clone(), sources.clone());
        }
        result
    }

    pub fn insert_addresses(&mut self, path: &[Slot], selected: &Self) {
        let selected = selected.map_borrows(|source| {
            (source.is_empty() || source == [Slot::Tail]).then(|| path.to_vec())
        });
        self.join(&selected);
    }

    pub fn all_sources(&self) -> super::demand::Frontier {
        self.sources.values().flatten().cloned().collect()
    }

    pub fn join(&mut self, other: &Self) -> bool {
        let mut changed = super::tasks::views::join(&mut self.task_views, &self.futures, other);
        for (key, leaf) in &other.leaves {
            let exists = self.leaves.contains_key(key);
            changed |= self.leaves.entry(key.clone()).or_default().join(leaf) || !exists;
        }
        for (key, sources) in &other.sources {
            let entry = self.sources.entry(key.clone()).or_default();
            let before = entry.len();
            entry.extend(sources.iter().cloned());
            changed |= before != entry.len();
        }
        for (path, targets) in &other.futures {
            let entry = self.futures.entry(path.clone()).or_default();
            let before = entry.len();
            entry.extend(targets.iter().cloned());
            changed |= before != entry.len();
        }
        for (path, targets) in &other.queues {
            let entry = self.queues.entry(path.clone()).or_default();
            let before = entry.len();
            entry.extend(targets.iter().cloned());
            changed |= before != entry.len();
        }
        for (path, callees) in &other.callees {
            let entry = self.callees.entry(path.clone()).or_default();
            changed |= entry.join(callees);
        }
        changed
    }

    pub fn sourced(mut self, storage: &Storage) -> Self {
        self.sources.clear();
        for (path, origin, capture) in self.leaves.keys() {
            if *origin == Origin::ValidationHole {
                continue;
            }
            self.sources
                .entry((path.clone(), origin.clone(), *capture))
                .or_default()
                .insert(Source::Current(Atom {
                    coordinate: Coordinate {
                        storage: storage.clone(),
                        path: path.clone(),
                    },
                    origin: origin.clone(),
                    capture: *capture,
                }));
        }
        self
    }

    pub fn function(def: DefId) -> Self {
        let mut value = Self::default();
        value
            .callees
            .insert(Vec::new(), super::callbacks::Targets::function(def));
        value
    }

    pub fn root_queue_handles(&self) -> impl Iterator<Item = &super::channels::Handle> {
        self.queues
            .iter()
            .filter(|(path, _)| path.is_empty() || **path == [Slot::Tail])
            .flat_map(|(_, handles)| handles)
    }

    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
            && self.callees.is_empty()
            && self.queues.is_empty()
            && self.futures.is_empty()
            && self.task_views.is_empty()
    }

    pub fn with_channel_capture(&self, capture: super::channels::Capture) -> Self {
        let mut result = self.clone();
        for (path, handles) in &mut result.queues {
            if path.is_empty() || *path == [Slot::Tail] {
                *handles = handles
                    .iter()
                    .map(|handle| super::channels::Handle {
                        target: handle.target.clone(),
                        capture: Some(capture),
                    })
                    .collect();
            }
        }
        result
    }

    pub fn select_channel_capture(&self, capture: super::channels::Capture) -> Self {
        let mut result = Self::default();
        for handle in self
            .queues
            .values()
            .flatten()
            .filter(|handle| handle.capture.is_none_or(|selected| selected == capture))
        {
            result
                .queues
                .entry(Vec::new())
                .or_default()
                .insert(handle.clone());
        }
        result
    }

    pub fn with_reference_capture(&self, capture: ReferenceCapture) -> Self {
        let mut result = Self {
            callees: self.callees.clone(),
            queues: self.queues.clone(),
            futures: self.futures.clone(),
            task_views: self.task_views.clone(),
            ..Self::default()
        };
        for ((path, origin, previous), leaf) in &self.leaves {
            let selected = if path.is_empty() || *path == [Slot::Tail] {
                Some(capture)
            } else {
                *previous
            };
            result.insert_contribution(path.clone(), origin.clone(), selected, leaf);
        }
        for ((path, origin, previous), sources) in &self.sources {
            let selected = if path.is_empty() || *path == [Slot::Tail] {
                Some(capture)
            } else {
                *previous
            };
            result
                .sources
                .entry((path.clone(), origin.clone(), selected))
                .or_default()
                .extend(sources.iter().cloned());
        }
        result
    }

    pub fn select_reference_capture(&self, capture: ReferenceCapture) -> Self {
        let mut result = Self::default();
        for ((path, origin, selected), leaf) in &self.leaves {
            if selected.is_none_or(|selected| selected == capture) {
                result.insert_contribution(Vec::new(), origin.clone(), *selected, leaf);
                if let Some(sources) = self.sources.get(&(path.clone(), origin.clone(), *selected))
                {
                    result
                        .sources
                        .entry((Vec::new(), origin.clone(), *selected))
                        .or_default()
                        .extend(sources.iter().cloned());
                }
            }
        }
        result
    }

    pub fn capture(&self, function: DefId, index: usize) -> Self {
        let mut result = self.map_borrows(|path| {
            if matches!(path.first(), Some(Slot::Capture(owner, i, _)) if *owner == function && *i == index) {
                Some(path[1..].to_vec())
            } else if path.first() == Some(&Slot::Tail) {
                Some(vec![Slot::Tail])
            } else {
                None
            }
        });
        for (path, callees) in &self.callees {
            if matches!(path.first(), Some(Slot::Capture(owner, i, _)) if *owner == function && *i == index)
            {
                result
                    .callees
                    .entry(path[1..].to_vec())
                    .or_default()
                    .join(callees);
            } else if path.first() == Some(&Slot::Tail) {
                result
                    .callees
                    .entry(vec![Slot::Tail])
                    .or_default()
                    .join(callees);
            }
        }
        result
    }

    pub fn origins(&self) -> Origins {
        self.leaves
            .keys()
            .map(|(_, origin, _)| origin.clone())
            .collect()
    }

    pub fn address_sources(&self) -> super::demand::Frontier {
        self.sources
            .iter()
            .filter(|((path, _, _), _)| path.is_empty() || *path == [Slot::Tail])
            .flat_map(|(_, sources)| sources.iter().cloned())
            .collect()
    }

    pub fn address_support(&self) -> Origins {
        let mut result = self.addresses();
        for ((path, _, _), leaf) in &self.leaves {
            if path.is_empty() || *path == [Slot::Tail] {
                result.extend(leaf.parents.iter().cloned());
            }
        }
        result
    }

    #[cfg(test)]
    pub fn with_address_support(mut self, parents: &Origins) -> Self {
        for ((path, _, _), leaf) in &mut self.leaves {
            if path.is_empty() || *path == [Slot::Tail] {
                leaf.parents.extend(parents.iter().cloned());
            }
        }
        self
    }

    pub fn with_metadata(mut self, metadata: &Leaf) -> Self {
        for ((path, _, _), leaf) in &mut self.leaves {
            if path.is_empty() || *path == [Slot::Tail] {
                leaf.join(metadata);
            }
        }
        self
    }

    pub fn addresses(&self) -> Origins {
        self.leaves
            .keys()
            .filter(|(path, _, _)| path.is_empty() || *path == [Slot::Tail])
            .map(|(_, origin, _)| origin.clone())
            .collect()
    }

    pub fn project(&self, slot: &Slot) -> Self {
        let mut result = self.map_borrows(|path| {
            if path.first() == Some(slot) {
                Some(path[1..].to_vec())
            } else if path.first() == Some(&Slot::Tail) {
                Some(vec![Slot::Tail])
            } else {
                None
            }
        });
        for (path, callees) in &self.callees {
            if path.first() == Some(slot) {
                result
                    .callees
                    .entry(path[1..].to_vec())
                    .or_default()
                    .join(callees);
            } else if path.first() == Some(&Slot::Tail) {
                result
                    .callees
                    .entry(vec![Slot::Tail])
                    .or_default()
                    .join(callees);
            }
        }
        result
    }

    pub fn prefixed(&self, slot: Slot, ty: TypeId, table: &TypeTable) -> Self {
        self.prefixed_path(&[slot], ty, table)
    }

    pub fn prefixed_path(&self, prefix: &[Slot], ty: TypeId, table: &TypeTable) -> Self {
        let mut result = self.map_borrows(|path| {
            let mut prefixed = prefix.to_vec();
            prefixed.extend_from_slice(path);
            Some(normalize(ty, prefixed, table))
        });
        for (path, callees) in &self.callees {
            let mut prefixed = prefix.to_vec();
            prefixed.extend_from_slice(path);
            result
                .callees
                .entry(normalize(ty, prefixed, table))
                .or_default()
                .join(callees);
        }
        result
    }

    fn map_borrows(&self, select: impl Fn(&[Slot]) -> Option<Path>) -> Self {
        let mut result = Self {
            task_views: super::tasks::views::map(self, &select, |_, handle| {
                super::tasks::views::Subject::from(handle)
            }),
            ..Self::default()
        };
        for (path, targets) in &self.futures {
            if let Some(path) = select(path) {
                result
                    .futures
                    .entry(path)
                    .or_default()
                    .extend(targets.iter().cloned());
            }
        }
        for (path, targets) in &self.queues {
            if let Some(path) = select(path) {
                result
                    .queues
                    .entry(path)
                    .or_default()
                    .extend(targets.iter().cloned());
            }
        }
        for ((path, origin, capture), leaf) in &self.leaves {
            if let Some(path) = select(path) {
                result.insert_contribution(path, origin.clone(), *capture, leaf);
            }
        }
        for ((path, origin, capture), sources) in &self.sources {
            if let Some(path) = select(path) {
                result
                    .sources
                    .entry((path, origin.clone(), *capture))
                    .or_default()
                    .extend(sources.iter().cloned());
            }
        }
        result
    }
}

pub(super) fn strip_mutable(mut ty: TypeId, table: &TypeTable) -> TypeId {
    while let Ty::Mutable(inner) = table.resolve(ty) {
        ty = *inner;
    }
    ty
}

pub(super) fn child_type(ty: TypeId, slot: &Slot, table: &TypeTable) -> Option<TypeId> {
    match (table.resolve(strip_mutable(ty, table)), slot) {
        (
            Ty::List(inner) | Ty::Set(inner) | Ty::Future { inner } | Ty::Channel { inner },
            Slot::Element,
        )
        | (Ty::Optional(inner), Slot::Optional)
        | (Ty::Ref { inner, .. }, Slot::Deref | Slot::ResidualDeref) => Some(*inner),
        (Ty::Map(key, _), Slot::Key) => Some(*key),
        (Ty::Map(_, value), Slot::Value) => Some(*value),
        (Ty::Union(members), Slot::Member(member)) if members.contains(member) => Some(*member),
        (Ty::Tuple(fields), Slot::Tuple(index)) => fields.get(*index).copied(),
        (Ty::Struct(def), Slot::Field(name)) => table
            .get_struct_fields(*def)?
            .iter()
            .find_map(|(field, ty)| (field == name).then_some(*ty)),
        (Ty::Applied { base, args }, Slot::Field(name)) => {
            let ty = table
                .get_struct_fields(*base)?
                .iter()
                .find_map(|(field, ty)| (field == name).then_some(*ty))?;
            let params = table.get_generic_params(*base).unwrap_or_default();
            let subst = spark_types::substitute::build_subst_map(params, args);
            Some(spark_types::substitute::substitute(ty, &subst, table))
        }
        (Ty::Enum(_), Slot::Variant(variant, index)) => table
            .get_variant_fields(*variant)?
            .get(*index)
            .map(|(_, ty)| *ty),
        (Ty::Applied { base, args }, Slot::Variant(variant, index)) => {
            let ty = table.get_variant_fields(*variant)?.get(*index)?.1;
            let subst = spark_types::substitute::build_subst_map(
                table.get_generic_params(*base).unwrap_or_default(),
                args,
            );
            Some(spark_types::substitute::substitute(ty, &subst, table))
        }
        (Ty::Function { .. }, Slot::Capture(_, _, ty)) => Some(*ty),
        _ => None,
    }
}

pub(super) fn normalize(mut ty: TypeId, path: Path, table: &TypeTable) -> Path {
    let mut seen = FxHashSet::default();
    let mut result = Vec::new();
    for slot in path {
        ty = strip_mutable(ty, table);
        if !seen.insert(ty) || slot == Slot::Tail {
            result.push(Slot::Tail);
            break;
        }
        let next = child_type(ty, &slot, table);
        result.push(slot);
        if let Some(next) = next {
            ty = next;
        } else {
            result.pop();
            result.push(Slot::Tail);
            break;
        }
    }
    result
}

pub(super) fn inputs(
    ty: TypeId,
    local: spark_mir::Local,
    source_ty: TypeId,
    prefix: &[Slot],
    table: &TypeTable,
) -> Value {
    let mut value = Value::default();
    super::channels::seed_inputs(&mut value, ty, local, source_ty, prefix, table);
    super::tasks::seed_inputs(&mut value, ty, local, source_ty, prefix, table);
    seed(
        ty,
        &mut Vec::new(),
        &mut FxHashSet::default(),
        table,
        &mut |path| {
            let mut source = prefix.to_vec();
            source.extend(path.iter().cloned());
            let capture = source.last().and_then(|slot| match slot {
                Slot::Capture(owner, index, ty)
                    if matches!(table.resolve(strip_mutable(*ty, table)), Ty::Ref { .. }) =>
                {
                    Some(ReferenceCapture {
                        owner: *owner,
                        index: *index,
                        ty: *ty,
                    })
                }
                _ => None,
            });
            let selection = normalize(source_ty, source, table);
            let origin = Origin::Input {
                local,
                path: selection.clone(),
            };
            let expected = path
                .iter()
                .try_fold(ty, |ty, slot| child_type(ty, slot, table))
                .filter(|ty| matches!(table.resolve(strip_mutable(*ty, table)), Ty::Ref { .. }));
            let leaf = value.leaves.entry((path, origin, None)).or_default();
            leaf.dependencies.insert(Dependency::InputValue(
                Selector {
                    channel_capture: None,
                    queue: None,
                    input: local,
                    selection,
                    intent: Intent::Value,
                    expected,
                    capture,
                },
                None,
            ));
        },
    );
    super::callbacks::callable_slots(
        &mut value,
        ty,
        table,
        &super::callbacks::Targets {
            open: true,
            ..Default::default()
        },
    );
    value
}

pub(super) fn tainted(ty: TypeId, origins: &Origins, table: &TypeTable) -> Value {
    let mut value = Value::default();
    seed(
        ty,
        &mut Vec::new(),
        &mut FxHashSet::default(),
        table,
        &mut |path| value.insert(&path, origins.clone()),
    );
    super::callbacks::callable_slots(
        &mut value,
        ty,
        table,
        &super::callbacks::Targets {
            open: origins.contains(&Origin::ValidationHole),
            unresolved: origins
                .iter()
                .any(|origin| *origin != Origin::ValidationHole),
            ..Default::default()
        },
    );
    value
}

pub(super) fn validation_hole(ty: TypeId, table: &TypeTable) -> Value {
    tainted(
        ty,
        &std::iter::once(Origin::ValidationHole).collect(),
        table,
    )
}

pub(super) fn tainted_from(ty: TypeId, source: &Value, table: &TypeTable) -> Value {
    let mut result = Value::default();
    for ((path, origin, capture), metadata) in &source.leaves {
        let mut origins = Origins::default();
        if metadata.has_validation_hole(origin) {
            origins.insert(Origin::ValidationHole);
        }
        if !metadata.is_validation_only(origin) {
            origins.insert(origin.clone());
        }
        let mut fragment = tainted(ty, &origins, table);
        for ((target, candidate, _), mut leaf) in std::mem::take(&mut fragment.leaves) {
            leaf.join(metadata);
            fragment.insert_contribution(target.clone(), candidate.clone(), *capture, &leaf);
            if let Some(sources) = source
                .sources
                .get(&(path.clone(), origin.clone(), *capture))
            {
                fragment
                    .sources
                    .entry((target, candidate, *capture))
                    .or_default()
                    .extend(sources.iter().cloned());
            }
        }
        result.join(&fragment);
    }
    // Target-only values still need the typed callable result shape.
    if source.leaves.is_empty() {
        return tainted(ty, &source.origins(), table);
    }
    result
}

fn seed(
    ty: TypeId,
    path: &mut Path,
    seen: &mut FxHashSet<TypeId>,
    table: &TypeTable,
    leaf: &mut impl FnMut(Path),
) {
    let ty = strip_mutable(ty, table);
    if !seen.insert(ty) {
        if super::super::check::can_carry_borrowed_reference(ty, table, &mut FxHashSet::default()) {
            let mut tail = path.clone();
            tail.push(Slot::Tail);
            leaf(tail);
        }
        return;
    }
    let mut child = |slot, ty| {
        path.push(slot);
        seed(ty, path, seen, table, leaf);
        path.pop();
    };
    match table.resolve(ty) {
        Ty::Ref { .. } => leaf(path.clone()),
        Ty::Function { .. } | Ty::Param(_) => {
            path.push(Slot::Tail);
            leaf(path.clone());
            path.pop();
        }
        Ty::List(inner) | Ty::Set(inner) => child(Slot::Element, *inner),
        Ty::Future { inner } => {
            child(Slot::Element, *inner);
            path.push(Slot::Tail);
            leaf(path.clone());
            path.pop();
        }
        Ty::Map(key, value) => {
            child(Slot::Key, *key);
            child(Slot::Value, *value);
        }
        Ty::Optional(_)
        | Ty::Union(_)
        | Ty::Tuple(_)
        | Ty::Struct(_)
        | Ty::Applied { .. }
        | Ty::Enum(_) => {
            for (slot, ty) in inline_slots(ty, table) {
                child(slot, ty);
            }
        }
        _ => {}
    }
    seen.remove(&ty);
}

pub(super) fn inline_slots(ty: TypeId, table: &TypeTable) -> Vec<(Slot, TypeId)> {
    match table.resolve(strip_mutable(ty, table)) {
        Ty::Optional(inner) => vec![(Slot::Optional, *inner)],
        Ty::Union(members) => members.iter().map(|ty| (Slot::Member(*ty), *ty)).collect(),
        Ty::Tuple(fields) => fields
            .iter()
            .enumerate()
            .map(|(index, ty)| (Slot::Tuple(index), *ty))
            .collect(),
        Ty::Struct(def) | Ty::Applied { base: def, .. } | Ty::Enum(def) => {
            if let Some(fields) = table.get_struct_fields(*def) {
                return fields
                    .iter()
                    .filter_map(|(name, _)| {
                        let slot = Slot::Field(*name);
                        Some((slot.clone(), child_type(ty, &slot, table)?))
                    })
                    .collect();
            }
            table
                .get_enum_variants(*def)
                .into_iter()
                .flatten()
                .flat_map(|(_, variant)| {
                    table
                        .get_variant_fields(*variant)
                        .into_iter()
                        .flatten()
                        .enumerate()
                        .filter_map(move |(index, _)| {
                            let slot = Slot::Variant(*variant, index);
                            Some((slot.clone(), child_type(ty, &slot, table)?))
                        })
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::Point;
    use spark_mir::BlockId;
    use spark_resolve::DefId;
    use spark_types::{IntWidth, LifetimeId};

    #[test]
    fn recursive_shapes_widen_without_losing_borrow_origins() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let reference = table.intern(Ty::Ref {
            inner: int,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let def = DefId::new(0);
        let node = table.intern(Ty::Struct(def));
        let children = table.intern(Ty::List(node));
        let child_field = spark_intern::sym("children");
        let borrow_field = spark_intern::sym("borrowed");
        table.set_struct_fields(
            def,
            vec![(child_field, children), (borrow_field, reference)],
        );
        let origins: Origins = [
            Origin::Loan(Point::new(BlockId(0), 0)),
            Origin::Loan(Point::new(BlockId(0), 1)),
        ]
        .into_iter()
        .collect();
        let mut value = Value::default();
        value.insert(&[Slot::Field(borrow_field)], origins.clone());
        for _ in 0..100 {
            value = value.prefixed(Slot::Element, children, &table).prefixed(
                Slot::Field(child_field),
                node,
                &table,
            );
            assert_eq!(value.origins(), origins);
            assert!(value.leaves.keys().all(|(path, _, _)| path.len() <= 3));
        }
        assert_eq!(
            value
                .project(&Slot::Field(child_field))
                .project(&Slot::Element)
                .project(&Slot::Field(borrow_field))
                .origins(),
            origins
        );
    }
    #[test]
    fn state_join_reports_endpoint_growth_without_new_origins_or_parents() {
        use super::super::{address::Endpoint, State};
        use spark_mir::Local;
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let owner_id = DefId::new(9700);
        let owner = table.intern(Ty::Struct(owner_id));
        let left = spark_intern::sym("left");
        let right = spark_intern::sym("right");
        table.set_struct_fields(owner_id, vec![(left, int), (right, int)]);
        let root = Point::new(BlockId(0), 0);
        let derived = Origin::Loan(Point::new(BlockId(0), 1));
        let value = |field| {
            Value::reference(derived.clone()).with_metadata(&Leaf {
                parents: std::iter::once(Origin::Loan(root)).collect(),
                endpoints: std::iter::once(Endpoint::Frame {
                    lifetime: crate::provenance::address::FrameLifetime::Local,
                    function: DefId::new(9701),
                    root,
                    local: Local(1),
                    root_ty: owner,
                    pointee: int,
                    location: vec![Slot::Field(field)],
                })
                .collect(),
                ..Default::default()
            })
        };
        let mut current = State::default();
        current.set(Local(2), value(left));
        let mut arriving = State::default();
        arriving.set(Local(2), value(right));
        assert!(
            current.join(&arriving),
            "endpoint-only growth must reschedule CFG successors"
        );
        assert!(
            !current.join(&arriving),
            "the joined endpoint state must reach a fixed point"
        );
        assert_eq!(
            current
                .value(Local(2))
                .leaves
                .values()
                .next()
                .unwrap()
                .endpoints
                .len(),
            2
        );
    }
}

#[cfg(test)]
mod capture_tests;

#[cfg(test)]
mod channel_tests;
