use rustc_hash::{FxHashMap, FxHashSet};
use spark_intern::Symbol;
use spark_resolve::DefId;
use spark_types::{Ty, TypeId, TypeTable};

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
    Capture(usize, TypeId),
    Tail,
}

pub(super) type Path = Vec<Slot>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Value {
    pub slots: FxHashMap<Path, Origins>,
    pub callees: FxHashMap<Path, FxHashSet<DefId>>,
}

impl Value {
    pub fn reference(origin: Origin) -> Self {
        let mut value = Self::default();
        value.insert(Vec::new(), std::iter::once(origin).collect());
        value
    }

    pub fn insert(&mut self, path: Path, origins: Origins) {
        if !origins.is_empty() {
            self.slots.entry(path).or_default().extend(origins);
        }
    }

    pub fn join(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (path, origins) in &other.slots {
            let entry = self.slots.entry(path.clone()).or_default();
            let before = entry.len();
            entry.extend(origins.iter().cloned());
            changed |= before != entry.len();
        }
        for (path, callees) in &other.callees {
            let entry = self.callees.entry(path.clone()).or_default();
            let before = entry.len();
            entry.extend(callees);
            changed |= before != entry.len();
        }
        changed
    }

    pub fn function(def: DefId) -> Self {
        let mut value = Self::default();
        value
            .callees
            .insert(Vec::new(), std::iter::once(def).collect());
        value
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty() && self.callees.is_empty()
    }

    pub fn capture(&self, index: usize) -> Self {
        let mut result = Self::default();
        for (path, origins) in &self.slots {
            if matches!(path.first(), Some(Slot::Capture(i, _)) if *i == index) {
                result.insert(path[1..].to_vec(), origins.clone());
            } else if path.first() == Some(&Slot::Tail) {
                result.insert(vec![Slot::Tail], origins.clone());
            }
        }
        for (path, callees) in &self.callees {
            if matches!(path.first(), Some(Slot::Capture(i, _)) if *i == index) {
                result
                    .callees
                    .entry(path[1..].to_vec())
                    .or_default()
                    .extend(callees);
            }
        }
        result
    }

    pub fn origins(&self) -> Origins {
        self.slots.values().flatten().cloned().collect()
    }

    pub fn addresses(&self) -> Origins {
        let mut origins = self.slots.get(&Vec::new()).cloned().unwrap_or_default();
        origins.extend(
            self.slots
                .get(&vec![Slot::Tail])
                .into_iter()
                .flatten()
                .cloned(),
        );
        origins
    }

    pub fn project(&self, slot: &Slot) -> Self {
        let mut result = Self::default();
        for (path, origins) in &self.slots {
            if path.first() == Some(slot) {
                result.insert(path[1..].to_vec(), origins.clone());
            } else if path.first() == Some(&Slot::Tail) {
                result.insert(vec![Slot::Tail], origins.clone());
            }
        }
        for (path, callees) in &self.callees {
            if path.first() == Some(slot) {
                result
                    .callees
                    .entry(path[1..].to_vec())
                    .or_default()
                    .extend(callees);
            } else if path.first() == Some(&Slot::Tail) {
                result
                    .callees
                    .entry(vec![Slot::Tail])
                    .or_default()
                    .extend(callees);
            }
        }
        result
    }

    pub fn prefixed(&self, slot: Slot, ty: TypeId, table: &TypeTable) -> Self {
        self.prefixed_path(&[slot], ty, table)
    }

    pub fn prefixed_path(&self, prefix: &[Slot], ty: TypeId, table: &TypeTable) -> Self {
        let mut result = Self::default();
        for (path, origins) in &self.slots {
            let mut prefixed = prefix.to_vec();
            prefixed.extend(path.iter().cloned());
            result.insert(normalize(ty, prefixed, table), origins.clone());
        }
        for (path, callees) in &self.callees {
            let mut prefixed = prefix.to_vec();
            prefixed.extend(path.iter().cloned());
            result
                .callees
                .entry(normalize(ty, prefixed, table))
                .or_default()
                .extend(callees);
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
        | (Ty::Ref { inner, .. }, Slot::Deref) => Some(*inner),
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
        (Ty::Function { .. }, Slot::Capture(_, ty)) => Some(*ty),
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
    seed(
        ty,
        &mut Vec::new(),
        &mut FxHashSet::default(),
        table,
        &mut |path| {
            let mut source = prefix.to_vec();
            source.extend(path.iter().cloned());
            value.insert(
                path,
                std::iter::once(Origin::Input {
                    local,
                    path: normalize(source_ty, source, table),
                })
                .collect(),
            );
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
        &mut |path| value.insert(path, origins.clone()),
    );
    value
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
        Ty::List(inner) | Ty::Set(inner) | Ty::Channel { inner } => child(Slot::Element, *inner),
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
        value.insert(vec![Slot::Field(borrow_field)], origins.clone());
        for _ in 0..100 {
            value = value.prefixed(Slot::Element, children, &table).prefixed(
                Slot::Field(child_field),
                node,
                &table,
            );
            assert_eq!(value.origins(), origins);
            assert!(value.slots.keys().all(|path| path.len() <= 3));
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
}
