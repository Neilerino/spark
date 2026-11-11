// TAG [SPARK-114]
//! MAY locations of provided owners and retained copies of formal inputs.

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::{AggregateKind, Local, Operand, Place, Rvalue};
use spark_types::{Ty, TypeId};

use super::super::Role;
use crate::provenance::{
    address::{Dependency, Endpoint, FrameLifetime},
    value, Flow, State,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Token {
    Provided(Local),
    Derived(Local),
}

type Tags = FxHashSet<Token>;

impl Token {
    const fn derived(self) -> Self {
        match self {
            Self::Provided(input) | Self::Derived(input) => Self::Derived(input),
        }
    }
    const fn input(self) -> Local {
        match self {
            Self::Provided(input) | Self::Derived(input) => input,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(in crate::provenance) struct InputOwners {
    locations: FxHashMap<Role, Tags>,
    escaped: Tags,
}

impl InputOwners {
    pub fn seed(&mut self, input: Local) {
        self.locations.insert(
            Role::root(input),
            FxHashSet::from_iter([Token::Provided(input)]),
        );
    }
    fn tags(&self, role: &Role) -> Tags {
        self.locations.get(role).cloned().unwrap_or_default()
    }
    pub fn drop_local(&mut self, local: Local) {
        self.locations.retain(|role, _| role.local() != local);
    }
    pub fn escape_local(&mut self, local: Local) {
        self.locations.retain(|role, tags| {
            if role.local() != local {
                return true;
            }
            self.escaped.extend(tags.iter().copied());
            false
        });
    }
    pub fn join(&mut self, other: &Self) -> bool {
        let before = self.escaped.len();
        self.escaped.extend(other.escaped.iter().copied());
        let mut changed = before != self.escaped.len();
        for (role, tags) in &other.locations {
            let current = self.locations.entry(role.clone()).or_default();
            let before = current.len();
            current.extend(tags.iter().copied());
            changed |= before != current.len();
        }
        changed
    }
    fn tokens(&self) -> impl Iterator<Item = &Token> {
        self.locations.values().flatten().chain(self.escaped.iter())
    }
    pub fn released_provided(&self, input: Local) -> bool {
        !self.tokens().any(|token| *token == Token::Provided(input))
    }
    pub fn retains_no_owner(&self, input: Local) -> bool {
        !self.tokens().any(|token| token.input() == input)
    }
}

#[derive(Default)]
pub(in crate::provenance) struct Transfer {
    moved: FxHashSet<Role>,
    values: FxHashMap<Role, Tags>,
    escaped: Tags,
}

impl Transfer {
    fn move_environment(source: Local, destination: Local, owners: &InputOwners) -> Self {
        let mut result = Self::default();
        for (role, tags) in owners
            .locations
            .iter()
            .filter(|(role, _)| role.local() == source)
        {
            if let Some(target) = role.capture_at(destination) {
                result.moved.insert(role.clone());
                result.values.insert(target, tags.clone());
            }
        }
        result
    }

    pub fn detach(&self, owners: &mut InputOwners) {
        for role in &self.moved {
            owners.locations.remove(role);
        }
        owners.escaped.extend(self.escaped.iter().copied());
    }
    pub fn install(self, owners: &mut InputOwners) {
        for (role, tags) in self.values {
            if !tags.is_empty() {
                owners.locations.entry(role).or_default().extend(tags);
            }
        }
    }
    fn assign(&mut self, source: Role, destination: Role, operand: &Operand, owners: &InputOwners) {
        let mut tags = owners.tags(&source);
        if matches!(operand, Operand::Move(_)) {
            self.moved.insert(source);
        } else {
            tags = tags.into_iter().map(Token::derived).collect();
        }
        self.values.entry(destination).or_default().extend(tags);
    }
}

impl Flow<'_> {
    fn owner_tags(&self, operand: &Operand, state: &State) -> Tags {
        let (Operand::Copy(place) | Operand::Move(place)) = operand else {
            return Tags::default();
        };
        let mut roots = FxHashSet::from_iter([place.local]);
        let mut pending = vec![place.local];
        while let Some(root) = pending.pop() {
            for leaf in state.value(root).leaves.values() {
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
                    if let Endpoint::Frame {
                        lifetime: FrameLifetime::Local,
                        function,
                        local,
                        ..
                    } = endpoint
                    {
                        if *function == self.function.def_id && roots.insert(*local) {
                            pending.push(*local);
                        }
                    }
                }
            }
        }
        state
            .input_owners
            .locations
            .iter()
            .filter(|(role, _)| roots.contains(&role.local()))
            .flat_map(|(_, tags)| tags.iter().copied())
            .collect()
    }

    fn escaped_owner_tags(&self, operand: &Operand, state: &State) -> Tags {
        let tags = self.owner_tags(operand, state);
        let Operand::Copy(place) = operand else {
            return tags;
        };
        let ty = self.place_type(place);
        if matches!(
            self.table.resolve(value::strip_mutable(ty, self.table)),
            Ty::Future { .. }
        ) {
            tags.into_iter().map(Token::derived).collect()
        } else {
            tags
        }
    }

    pub(in crate::provenance) fn escape_input_owner_operand(
        &self,
        operand: &Operand,
        state: &mut State,
    ) {
        let tags = self.escaped_owner_tags(operand, state);
        state.input_owners.escaped.extend(tags);
    }

    fn input_value_transfer(
        &self,
        operand: &Operand,
        destination: &Place,
        target: Option<TypeId>,
        state: &State,
    ) -> Transfer {
        let mut result = Transfer::default();
        let destination_ty = value::strip_mutable(self.place_type(destination), self.table);
        let identity_cast =
            target.is_none_or(|target| value::strip_mutable(target, self.table) == destination_ty);
        if let Operand::Copy(source) | Operand::Move(source) = operand {
            if identity_cast
                && matches!(
                    self.table
                        .resolve(value::strip_mutable(self.place_type(source), self.table)),
                    Ty::Ref { .. }
                )
                && matches!(self.table.resolve(destination_ty), Ty::Ref { .. })
            {
                return result;
            }
        }
        if let Some((source, ty)) = self.captured_future(operand) {
            if identity_cast
                && destination.projections.is_empty()
                && matches!(self.table.resolve(destination_ty), Ty::Future { .. })
                && value::strip_mutable(ty, self.table) == destination_ty
            {
                result.assign(
                    Role::root(source),
                    Role::root(destination.local),
                    operand,
                    &state.input_owners,
                );
                return result;
            }
        }
        result
            .escaped
            .extend(self.escaped_owner_tags(operand, state));
        result
    }

    pub(in crate::provenance) fn input_owner_transfer(
        &self,
        rvalue: &Rvalue,
        destination: &Place,
        state: &State,
    ) -> Transfer {
        let mut result = Transfer::default();
        if let Some(source) = self.moving_environment(rvalue, destination) {
            return Transfer::move_environment(source, destination.local, &state.input_owners);
        }
        match rvalue {
            Rvalue::Use(operand) => {
                return self.input_value_transfer(operand, destination, None, state)
            }
            Rvalue::Cast { operand, target_ty } => {
                return self.input_value_transfer(operand, destination, Some(*target_ty), state)
            }
            Rvalue::Aggregate(AggregateKind::Closure { func_def_id }, operands)
                if destination.projections.is_empty() =>
            {
                for (index, operand) in operands.iter().enumerate() {
                    if let Some((source, ty)) = self.captured_future(operand) {
                        result.assign(
                            Role::root(source),
                            Role::capture(destination.local, *func_def_id, index, ty),
                            operand,
                            &state.input_owners,
                        );
                    } else {
                        result
                            .escaped
                            .extend(self.escaped_owner_tags(operand, state));
                    }
                }
            }
            Rvalue::Aggregate(_, operands) | Rvalue::StringInterp(operands) => {
                for operand in operands {
                    result
                        .escaped
                        .extend(self.escaped_owner_tags(operand, state));
                }
            }
            Rvalue::UnaryOp(_, operand) => {
                result
                    .escaped
                    .extend(self.escaped_owner_tags(operand, state));
            }
            Rvalue::BinaryOp(_, left, right) => {
                for operand in [left, right] {
                    result
                        .escaped
                        .extend(self.escaped_owner_tags(operand, state));
                }
            }
            Rvalue::ListRange {
                start, end, step, ..
            }
            | Rvalue::RangeLen {
                start, end, step, ..
            } => {
                for operand in [start, end, step] {
                    result
                        .escaped
                        .extend(self.escaped_owner_tags(operand, state));
                }
            }
            Rvalue::Ref { .. }
            | Rvalue::Discriminant(_)
            | Rvalue::CollectionGetOptional { .. }
            | Rvalue::ReadFutureResult { .. } => {}
        }
        result
    }
}
