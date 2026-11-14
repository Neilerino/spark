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
pub(super) type Accesses = FxHashSet<Access>;

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
        let mut value = state.value(place.local);
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
            result.join(&self.pointee(&origin, inner, state, visited));
        }
        result
    }

    pub(super) fn pointee(
        &self,
        origin: &Origin,
        ty: TypeId,
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
                    result.join(&self.target_value(target, ty, state, visited));
                }
            }
            Origin::Frame { function, point } if *function == self.function.def_id => {
                for target in self.loans.get(point).into_iter().flatten() {
                    result.join(&self.target_value(target, ty, state, visited));
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
            }
        }
        visited.remove(origin);
        result
    }

    pub(super) fn target_value(
        &self,
        target: &Target,
        ty: TypeId,
        state: &State,
        visited: &mut FxHashSet<Origin>,
    ) -> Value {
        let result = match target {
            Target::Frame { local, path } => self.project_path(
                state.cell(*local),
                self.function.locals[local.index()].ty,
                path,
                state,
                visited,
            ),
            Target::Through { origin, ty, path } => {
                let value = self.pointee(origin, *ty, state, visited);
                self.project_path(value, *ty, path, state, visited)
            }
        };
        if matches!(target, Target::Frame { path, .. } | Target::Through { path, .. } if path.contains(&Slot::Tail))
        {
            value::tainted(ty, &result.origins(), self.table)
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
    pub(super) fn tail_references(
        &self,
        value: Value,
        ty: TypeId,
        expected: Option<TypeId>,
        state: &State,
    ) -> super::Origins {
        let mut result = super::Origins::default();
        let mut pending = vec![(value, ty)];
        let mut seen = FxHashSet::default();
        while let Some((value, ty)) = pending.pop() {
            for (path, origins) in value.slots {
                let selected = self.selected_type(ty, &path);
                for origin in origins {
                    let pointees = if let Ty::Ref { inner, .. } = self
                        .table
                        .resolve(value::strip_mutable(selected, self.table))
                    {
                        vec![*inner]
                    } else {
                        self.pointee_types(&origin)
                    };
                    if pointees.is_empty() {
                        result.insert(origin);
                        continue;
                    }
                    for pointee in pointees {
                        if !seen.insert((origin.clone(), pointee)) {
                            continue;
                        }
                        if expected.is_none_or(|expected| {
                            value::strip_mutable(expected, self.table)
                                == value::strip_mutable(pointee, self.table)
                        }) || matches!(self.table.resolve(pointee), Ty::Ref { .. })
                        {
                            result.insert(origin.clone());
                        }
                        let payload =
                            self.pointee(&origin, pointee, state, &mut FxHashSet::default());
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
