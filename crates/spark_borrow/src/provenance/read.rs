//! Address resolution and the pointee accesses performed by MIR reads.

use rustc_hash::FxHashSet;
use spark_mir::{Constant, Operand, Place, Projection};
use spark_types::{Ty, TypeId};

use super::{child_type, projection_slot, value, Flow, Origin, Path, Slot, State, Target, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum AccessKind {
    Address,
    Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Access {
    pub origin: Origin,
    pub selection: Path,
    pub kind: AccessKind,
}
#[derive(Clone, Debug, Default)]
pub(super) struct Accesses {
    events: FxHashSet<Access>,
    pub demands: super::demand::Demand,
}

impl Accesses {
    pub fn insert(&mut self, access: Access) {
        self.events.insert(access);
    }
    pub fn iter(&self) -> std::collections::hash_set::Iter<'_, Access> {
        self.events.iter()
    }
    pub fn len(&self) -> usize {
        self.events.len()
    }
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
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
            .sourced(super::demand::Bank::Logical, place.local);
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
                accesses.demands.extend(value.address_sources());
                for origin in value.addresses() {
                    accesses.insert(Access {
                        origin,
                        selection: selection.clone(),
                        kind,
                    });
                }
                self.dereference(&value, ty, state, &mut FxHashSet::default())
            } else {
                value.project(&slot)
            };
            ty = child_type(ty, &slot, self.table).unwrap_or(TypeId::ERROR);
        }
        value
    }

    pub(super) fn dereference(
        &self,
        value: &Value,
        ty: TypeId,
        state: &State,
        visited: &mut FxHashSet<Origin>,
    ) -> Value {
        let inner = child_type(ty, &Slot::Deref, self.table).unwrap_or(TypeId::ERROR);
        let mut result = Value::default();
        for origin in value.addresses() {
            result.join(&self.pointee(&origin, inner, value, state, visited));
        }
        result
    }

    pub(super) fn pointee(
        &self,
        origin: &Origin,
        ty: TypeId,
        reference: &Value,
        state: &State,
        visited: &mut FxHashSet<Origin>,
    ) -> Value {
        if !visited.insert(origin.clone()) {
            return Value::default();
        }
        let mut result = Value::default();
        match origin {
            Origin::Loan(point) => {
                for target in self.loans.get(point).into_iter().flatten() {
                    result.join(&self.target_value(target, ty, reference, state, visited));
                }
            }
            Origin::Frame { function, point } if *function == self.function.def_id => {
                for target in self.loans.get(point).into_iter().flatten() {
                    result.join(&self.target_value(target, ty, reference, state, visited));
                }
            }
            Origin::Frame { .. } | Origin::Opaque { .. } => {
                result = value::tainted(ty, &std::iter::once(origin.clone()).collect(), self.table);
            }
            Origin::Input { local, path } => {
                let mut path = path.clone();
                path.push(Slot::Deref);
                result = value::inputs(
                    ty,
                    *local,
                    self.function.locals[local.index()].ty,
                    &path,
                    self.table,
                );
            }
            Origin::InputAddress { local, path } => {
                result = value::inputs(
                    ty,
                    *local,
                    self.function.locals[local.index()].ty,
                    path,
                    self.table,
                );
            }
            Origin::Unproven(point) => {
                let mut origins = self.unproven.get(point).cloned().unwrap_or_default();
                origins.insert(origin.clone());
                result = value::tainted(ty, &origins, self.table);
                result.join(&value::tainted_from(ty, reference, self.table));
            }
        }
        visited.remove(origin);
        result
    }

    pub(super) fn target_value(
        &self,
        target: &Target,
        ty: TypeId,
        reference: &Value,
        state: &State,
        visited: &mut FxHashSet<Origin>,
    ) -> Value {
        let result = match target {
            Target::Frame { local, path } => self.project_path(
                state
                    .cell(*local)
                    .sourced(super::demand::Bank::Cell, *local),
                self.function.locals[local.index()].ty,
                path,
                state,
                visited,
            ),
            Target::Through { origin, ty, path } => {
                let value = self.pointee(origin, *ty, reference, state, visited);
                self.project_path(value, *ty, path, state, visited)
            }
        };
        if matches!(target, Target::Frame { path, .. } | Target::Through { path, .. } if path.contains(&Slot::Tail))
        {
            value::tainted_from(ty, &result, self.table)
        } else {
            result
        }
    }

    pub(super) fn project_path(
        &self,
        mut value: Value,
        mut ty: TypeId,
        path: &Path,
        state: &State,
        visited: &mut FxHashSet<Origin>,
    ) -> Value {
        for slot in path {
            if *slot == Slot::Tail {
                return value;
            }
            value = if *slot == Slot::Deref {
                self.dereference(&value, ty, state, visited)
            } else {
                value.project(slot)
            };
            ty = child_type(ty, slot, self.table).unwrap_or(TypeId::ERROR);
        }
        value
    }

    pub(super) fn reference_targets(&self, place: &Place, state: &State) -> FxHashSet<Target> {
        place
            .projections
            .iter()
            .rposition(|p| matches!(p, Projection::Deref))
            .map_or_else(
                || {
                    let (path, _) = self.projection_path(
                        self.function.locals[place.local.index()].ty,
                        &place.projections,
                    );
                    std::iter::once(Target::Frame {
                        local: place.local,
                        path,
                    })
                    .collect()
                },
                |index| {
                    let prefix = Place {
                        local: place.local,
                        projections: place.projections[..index].to_vec(),
                    };
                    let reference = self.read(&prefix, state);
                    let ty = child_type(self.place_type(&prefix), &Slot::Deref, self.table)
                        .unwrap_or(TypeId::ERROR);
                    let (path, _) = self.projection_path(ty, &place.projections[index + 1..]);
                    reference
                        .addresses()
                        .into_iter()
                        .map(|origin| Target::Through {
                            origin,
                            ty,
                            path: path.clone(),
                        })
                        .collect()
                },
            )
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
            for (path, origins) in &value.slots {
                let selected = self.selected_type(ty, path);
                for origin in origins {
                    let sources = value
                        .sources
                        .get(&(path.clone(), origin.clone()))
                        .cloned()
                        .unwrap_or_default();
                    let parents = value
                        .support
                        .get(&(path.clone(), origin.clone()))
                        .cloned()
                        .unwrap_or_default();
                    let pointees = if let Ty::Ref { inner, .. } = self
                        .table
                        .resolve(value::strip_mutable(selected, self.table))
                    {
                        vec![*inner]
                    } else {
                        self.pointee_types(origin)
                    };
                    if pointees.is_empty() {
                        result
                            .support
                            .entry((Vec::new(), origin.clone()))
                            .or_default()
                            .extend(parents);
                        result.insert_sourced(Vec::new(), origin.clone(), sources);
                        continue;
                    }
                    for pointee in pointees {
                        if expected.is_none_or(|expected| {
                            value::strip_mutable(expected, self.table)
                                == value::strip_mutable(pointee, self.table)
                        }) || matches!(self.table.resolve(pointee), Ty::Ref { .. })
                        {
                            result
                                .support
                                .entry((Vec::new(), origin.clone()))
                                .or_default()
                                .extend(parents.iter().cloned());
                            result.insert_sourced(Vec::new(), origin.clone(), sources.clone());
                        }
                        let mut reference =
                            Value::reference(origin.clone()).with_address_support(&parents);
                        reference.insert_sourced(Vec::new(), origin.clone(), sources.clone());
                        let context = seen.entry((origin.clone(), pointee)).or_default();
                        if !context.join(&reference) {
                            continue;
                        }
                        let payload = self.pointee(
                            origin,
                            pointee,
                            context,
                            state,
                            &mut FxHashSet::default(),
                        );
                        if !payload.is_empty() {
                            pending.push((payload, pointee));
                        }
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

    fn pointee_types(&self, origin: &Origin) -> Vec<TypeId> {
        match origin {
            Origin::Loan(point) => self.loan_types(*point),
            Origin::Frame { function, point } if *function == self.function.def_id => {
                self.loan_types(*point)
            }
            Origin::Input { local, path } => {
                let ty = self.selected_type(self.function.locals[local.index()].ty, path);
                if let Ty::Ref { inner, .. } =
                    self.table.resolve(value::strip_mutable(ty, self.table))
                {
                    vec![*inner]
                } else {
                    Vec::new()
                }
            }
            Origin::InputAddress { local, path } => {
                let ty = self.selected_type(self.function.locals[local.index()].ty, path);
                if ty.is_error() {
                    Vec::new()
                } else {
                    vec![ty]
                }
            }
            Origin::Unproven(_) | Origin::Frame { .. } | Origin::Opaque { .. } => Vec::new(),
        }
    }

    fn loan_types(&self, point: super::Point) -> Vec<TypeId> {
        self.loans
            .get(&point)
            .into_iter()
            .flatten()
            .filter_map(|target| {
                let ty = match target {
                    Target::Frame { local, path } => {
                        self.selected_type(self.function.locals[local.index()].ty, path)
                    }
                    Target::Through { ty, path, .. } => self.selected_type(*ty, path),
                };
                (!ty.is_error()).then_some(ty)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::Point;
    use spark_mir::{BlockId, Local, MirFunction};
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::{IntWidth, LifetimeId, TypeTable};

    #[test]
    fn tail_selection_joins_support_for_repeated_origins_in_different_slots() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let reference = table.intern(Ty::Ref {
            inner: int,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let tuple = table.intern(Ty::Tuple(vec![reference, reference]));
        let function = MirFunction {
            def_id: DefId::new(9200),
            param_count: 0,
            return_type: int,
            locals: vec![],
            blocks: vec![],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };
        let flow = Flow::new(&function, &table, None);
        let shared = Origin::Loan(Point::new(BlockId(0), 0));
        let parents: Vec<_> = (1..=2)
            .map(|index| Origin::Loan(Point::new(BlockId(0), index)))
            .collect();
        let mut value = Value::default();
        for (index, parent) in parents.iter().enumerate() {
            let contribution = Value::reference(shared.clone())
                .with_address_support(&std::iter::once(parent.clone()).collect());
            value.join(&contribution.prefixed(Slot::Tuple(index), tuple, &table));
        }
        let value = value.sourced(super::super::demand::Bank::Logical, Local(1));
        let selected = flow.tail_reference_value(value, tuple, Some(int), &State::default());
        assert_eq!(
            selected.address_sources().len(),
            2,
            "both reaching slots contribute to the selection"
        );
        let support = selected.address_support();
        for parent in parents {
            assert!(
                support.contains(&parent),
                "every selected contribution retains its own parent"
            );
        }
    }
    #[test]
    fn opaque_pointee_copy_preserves_reaching_support() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let int_ref = table.intern(Ty::Ref {
            inner: int,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let list = table.intern(Ty::List(int_ref));
        let list_ref = table.intern(Ty::Ref {
            inner: list,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let function = MirFunction {
            def_id: DefId::new(9201),
            param_count: 0,
            return_type: int,
            locals: vec![],
            blocks: vec![],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };
        let mut flow = Flow::new(&function, &table, None);
        let opaque = Origin::Unproven(Point::new(BlockId(0), 3));
        let child = Origin::Loan(Point::new(BlockId(0), 2));
        let parent = Origin::Loan(Point::new(BlockId(0), 1));
        flow.unproven
            .insert(Point::new(BlockId(0), 3), std::iter::once(child).collect());
        let wrapper = Value::reference(opaque)
            .with_address_support(&std::iter::once(parent.clone()).collect())
            .sourced(super::super::demand::Bank::Logical, Local(1));
        let payload = flow.dereference(
            &wrapper,
            list_ref,
            &State::default(),
            &mut FxHashSet::default(),
        );
        assert!(
            payload
                .support
                .values()
                .any(|parents| parents.contains(&parent)),
            "opaque copied payload must preserve parent support from this reaching wrapper"
        );
        assert!(
            !payload.all_sources().is_empty(),
            "opaque payload demand must trace back through its reaching wrapper"
        );
    }
    #[test]
    fn opaque_tail_selection_preserves_reaching_support() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let int_ref = table.intern(Ty::Ref {
            inner: int,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let list = table.intern(Ty::List(int_ref));
        let list_ref = table.intern(Ty::Ref {
            inner: list,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let function = MirFunction {
            def_id: DefId::new(9202),
            param_count: 0,
            return_type: int,
            locals: vec![],
            blocks: vec![],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };
        let mut flow = Flow::new(&function, &table, None);
        let opaque = Origin::Unproven(Point::new(BlockId(0), 3));
        let child = Origin::Loan(Point::new(BlockId(0), 2));
        let parent = Origin::Loan(Point::new(BlockId(0), 1));
        flow.unproven
            .insert(Point::new(BlockId(0), 3), std::iter::once(child).collect());
        let wrapper = Value::reference(opaque)
            .with_address_support(&std::iter::once(parent.clone()).collect())
            .sourced(super::super::demand::Bank::Logical, Local(1));
        let selected = flow.tail_reference_value(wrapper, list_ref, Some(int), &State::default());
        assert!(
            selected.address_support().contains(&parent),
            "typed opaque Tail selection must preserve parent support from its reaching wrapper"
        );
        assert!(
            !selected.address_sources().is_empty(),
            "typed opaque Tail selection must trace back through its reaching wrapper"
        );
    }
    #[test]
    fn opaque_tail_expansion_joins_distinct_reaching_contexts() {
        use spark_mir::LocalDecl;
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let reference = |inner| Ty::Ref {
            inner,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        };
        let int_ref = table.intern(reference(int));
        let list = table.intern(Ty::List(int_ref));
        let list_ref = table.intern(reference(list));
        let wrapper_ref = table.intern(reference(list_ref));
        let tuple = table.intern(Ty::Tuple(vec![wrapper_ref, wrapper_ref]));
        let locals = (0..=2)
            .map(|index| LocalDecl {
                local: Local(index),
                ty: list_ref,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            })
            .collect();
        let function = MirFunction {
            def_id: DefId::new(9203),
            param_count: 0,
            return_type: int,
            locals,
            blocks: vec![],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };
        let mut flow = Flow::new(&function, &table, None);
        let opaque_site = Point::new(BlockId(0), 3);
        let opaque = Origin::Unproven(opaque_site);
        let child = Origin::Loan(Point::new(BlockId(0), 2));
        flow.unproven
            .insert(opaque_site, std::iter::once(child).collect());
        let parents: Vec<_> = (10..=11)
            .map(|index| Origin::Loan(Point::new(BlockId(0), index)))
            .collect();
        let mut state = State::default();
        let mut value = Value::default();
        for (index, parent) in parents.iter().enumerate() {
            let local = Local(index as u32 + 1);
            state.set(
                local,
                Value::reference(opaque.clone())
                    .with_address_support(&std::iter::once(parent.clone()).collect()),
            );
            let point = Point::new(BlockId(0), index as u32 + 20);
            flow.loans.insert(
                point,
                std::iter::once(Target::Frame {
                    local,
                    path: vec![],
                })
                .collect(),
            );
            value.join(&Value::reference(Origin::Loan(point)).prefixed(
                Slot::Tuple(index),
                tuple,
                &table,
            ));
        }
        let selected = flow.tail_reference_value(value, tuple, Some(int), &state);
        let support = selected.address_support();
        for parent in parents {
            assert!(
                support.contains(&parent),
                "opaque graph expansion must preserve each distinct reaching context"
            );
        }
    }
}
