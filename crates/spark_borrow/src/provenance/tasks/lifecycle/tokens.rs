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
pub(in crate::provenance) enum Token {
    Provided(Local),
    Derived(Local),
}

pub(in crate::provenance) type Tags = FxHashSet<Token>;

impl Token {
    pub const fn derived(self) -> Self {
        match self {
            Self::Provided(input) | Self::Derived(input) => Self::Derived(input),
        }
    }
    pub const fn input(self) -> Local {
        match self {
            Self::Provided(input) | Self::Derived(input) => input,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(in crate::provenance) struct InputOwners {
    locations: FxHashMap<Role, Tags>,
    ambient: FxHashMap<Role, FxHashSet<Local>>,
    unresolved_environments: FxHashSet<Local>,
    escaped: Tags,
}

impl InputOwners {
    pub fn seed(&mut self, input: Local) {
        self.locations.insert(
            Role::root(input),
            FxHashSet::from_iter([Token::Provided(input)]),
        );
    }
    pub fn unresolved_environment(&self, local: Local) -> bool {
        self.unresolved_environments.contains(&local)
    }
    pub fn mark_unresolved_environment(&mut self, local: Local) {
        self.unresolved_environments.insert(local);
    }
    pub fn escape_tags(&mut self, tags: Tags) {
        self.escaped.extend(tags);
    }
    pub fn retained_tags(&self) -> Tags {
        self.tokens().copied().collect()
    }
    pub fn all_value_tags(&self) -> Tags {
        self.tokens()
            .copied()
            .chain(self.ambient.values().flatten().copied().map(Token::Derived))
            .collect()
    }
    pub fn seed_ambient(&mut self, input: Local) {
        self.ambient
            .insert(Role::root(input), FxHashSet::from_iter([input]));
    }
    fn ambient_tags(&self, role: &Role) -> Tags {
        self.ambient
            .get(role)
            .into_iter()
            .flatten()
            .copied()
            .map(Token::Derived)
            .collect()
    }
    pub fn value_tags(&self, role: &Role) -> Tags {
        let mut tags = self.tags(role);
        tags.extend(self.ambient_tags(role));
        tags
    }
    pub fn tags(&self, role: &Role) -> Tags {
        self.locations.get(role).cloned().unwrap_or_default()
    }
    pub fn drop_local(&mut self, local: Local) {
        self.unresolved_environments.remove(&local);
        self.locations.retain(|role, _| role.local() != local);
        self.ambient.retain(|role, _| role.local() != local);
    }
    pub fn escape_local(&mut self, local: Local) {
        self.unresolved_environments.remove(&local);
        self.ambient.retain(|role, inputs| {
            if role.local() != local {
                return true;
            }
            self.escaped
                .extend(inputs.iter().copied().map(Token::Derived));
            false
        });
        self.locations.retain(|role, tags| {
            if role.local() != local {
                return true;
            }
            self.escaped.extend(tags.iter().copied());
            false
        });
    }
    pub fn escape_execution(&mut self, local: Local, ty: TypeId) {
        self.locations.retain(|role, tags| {
            if role.local() != local || role.callback_type() != Some(ty) {
                return true;
            }
            self.escaped.extend(tags.iter().copied());
            false
        });
    }

    pub fn execution_outputs(
        &self,
        local: Local,
        ty: TypeId,
    ) -> FxHashMap<crate::provenance::Path, Tags> {
        self.locations
            .iter()
            .filter(|(role, _)| role.local() == local && role.callback_type() == Some(ty))
            .map(|(role, tags)| (role.selection().to_vec(), tags.clone()))
            .collect()
    }

    pub fn add_output(&mut self, role: Role, tags: Tags) {
        if !tags.is_empty() {
            self.locations.entry(role).or_default().extend(tags);
        }
    }

    pub fn retains_only(&self, input: Local, roles: &FxHashSet<Role>) -> bool {
        !self.escaped.iter().any(|tag| tag.input() == input)
            && self.locations.iter().all(|(role, tags)| {
                roles.contains(role) || !tags.iter().any(|tag| tag.input() == input)
            })
    }

    pub fn join(&mut self, other: &Self) -> bool {
        let before = self.escaped.len();
        self.escaped.extend(other.escaped.iter().copied());
        let mut changed = before != self.escaped.len();
        let before = self.unresolved_environments.len();
        self.unresolved_environments
            .extend(other.unresolved_environments.iter().copied());
        changed |= before != self.unresolved_environments.len();
        for (role, inputs) in &other.ambient {
            let current = self.ambient.entry(role.clone()).or_default();
            let before = current.len();
            current.extend(inputs.iter().copied());
            changed |= before != current.len();
        }
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
    unresolved_environments: FxHashSet<Local>,
    values: FxHashMap<Role, Tags>,
    escaped: Tags,
}

impl Transfer {
    fn environment(source: Local, destination: Local, owners: &InputOwners, moving: bool) -> Self {
        let mut result = Self::default();
        if owners.unresolved_environment(source) {
            result.unresolved_environments.insert(destination);
        }
        for (role, tags) in owners
            .locations
            .iter()
            .filter(|(role, _)| role.local() == source)
        {
            if let Some(target) = role.capture_at(destination) {
                if moving {
                    result.moved.insert(role.clone());
                }
                result.values.insert(target, tags.clone());
            }
        }
        result
    }

    pub fn execution(
        source: Local,
        destination: Local,
        ty: TypeId,
        owners: &InputOwners,
        adopting: bool,
    ) -> Self {
        let mut result = Self::default();
        for (role, tags) in &owners.locations {
            if role.local() == source
                && (if adopting {
                    role.callback_type().is_none()
                } else {
                    role.callback_type() == Some(ty)
                })
            {
                if let Some(target) = role.in_execution(destination, ty) {
                    result.moved.insert(role.clone());
                    result.values.insert(target, tags.clone());
                }
            }
        }
        result
    }

    pub fn reanchor(&mut self, destination: Local) {
        self.values = std::mem::take(&mut self.values)
            .into_iter()
            .map(|(role, tags)| (role.reanchor(destination), tags))
            .collect();
    }

    pub fn detach(&self, owners: &mut InputOwners) {
        for role in &self.moved {
            owners.locations.remove(role);
            owners.ambient.remove(role);
        }
        owners.escaped.extend(self.escaped.iter().copied());
    }
    pub fn install(self, owners: &mut InputOwners) {
        owners
            .unresolved_environments
            .extend(self.unresolved_environments);
        for (role, tags) in self.values {
            if !tags.is_empty() {
                owners.locations.entry(role).or_default().extend(tags);
            }
        }
    }
    fn assign(&mut self, source: Role, destination: Role, operand: &Operand, owners: &InputOwners) {
        let mut tags = owners.tags(&source);
        let ambient = owners.ambient_tags(&source);
        if matches!(operand, Operand::Move(_)) {
            self.escaped.extend(ambient.iter().copied());
            self.moved.insert(source);
        } else {
            tags = tags.into_iter().map(Token::derived).collect();
        }
        tags.extend(ambient);
        self.values.entry(destination).or_default().extend(tags);
    }
}

impl Flow<'_> {
    fn owner_roots(&self, operand: &Operand, state: &State) -> FxHashSet<Local> {
        let (Operand::Copy(place) | Operand::Move(place)) = operand else {
            return FxHashSet::default();
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
        roots
    }

    pub(in crate::provenance) fn clobber_input_owners(&self, local: Local, state: &mut State) {
        state.input_owners.escape_local(local);
        if matches!(
            self.table.resolve(value::strip_mutable(
                self.function.locals[local.index()].ty,
                self.table
            )),
            Ty::Function { .. }
        ) {
            state.input_owners.mark_unresolved_environment(local);
        }
        state.future_owners.invalidate(local, &state.future_copies);
        state.future_copies.forget(local);
    }

    pub(in crate::provenance) fn owner_tags(&self, operand: &Operand, state: &State) -> Tags {
        let roots = self.owner_roots(operand, state);
        let mut tags: Tags = state
            .input_owners
            .locations
            .iter()
            .filter(|(role, _)| roots.contains(&role.local()))
            .flat_map(|(_, tags)| tags.iter().copied())
            .collect();
        for (role, inputs) in &state.input_owners.ambient {
            if roots.contains(&role.local()) {
                tags.extend(inputs.iter().copied().map(Token::Derived));
            }
        }
        tags
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

    pub(in crate::provenance) fn environment_sources(
        &self,
        operand: &Operand,
        state: &State,
    ) -> (FxHashSet<Local>, bool) {
        let roots: FxHashSet<_> = self
            .owner_roots(operand, state)
            .into_iter()
            .filter(|local| {
                matches!(
                    self.table.resolve(value::strip_mutable(
                        self.function.locals[local.index()].ty,
                        self.table
                    )),
                    Ty::Function { .. }
                )
            })
            .collect();
        let function_reference = matches!(
            operand,
            Operand::Constant(spark_mir::Constant::FunctionRef(_))
        );
        let unresolved = (!function_reference && roots.is_empty())
            || roots.iter().any(|root| {
                !state.live.contains(root) || state.input_owners.unresolved_environment(*root)
            });
        (roots, unresolved)
    }

    fn read_environment(&self, operand: &Operand, destination: Local, state: &State) -> Transfer {
        let (roots, unresolved) = self.environment_sources(operand, state);
        let mut result = Transfer::default();
        if unresolved {
            result.unresolved_environments.insert(destination);
        }
        for (role, tags) in &state.input_owners.locations {
            if roots.contains(&role.local()) {
                if let Some(target) = role.capture_at(destination) {
                    result
                        .values
                        .entry(target)
                        .or_default()
                        .extend(tags.iter().copied());
                }
            }
        }
        result
            .escaped
            .extend(self.escaped_owner_tags(operand, state));
        result
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
        if identity_cast
            && destination.projections.is_empty()
            && matches!(self.table.resolve(destination_ty), Ty::Function { .. })
        {
            return self.read_environment(operand, destination.local, state);
        }
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
            return Transfer::environment(source, destination.local, &state.input_owners, true);
        }
        if let Rvalue::Use(operand @ Operand::Copy(source)) = rvalue {
            if let Some(source) = self.environment_source(source, destination) {
                result =
                    Transfer::environment(source, destination.local, &state.input_owners, false);
                result
                    .escaped
                    .extend(self.escaped_owner_tags(operand, state));
                return result;
            }
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
                        let ty = crate::provenance::operand_type(operand, self.function);
                        if super::may_own_future(ty, self.table, &mut FxHashSet::default()) {
                            result.unresolved_environments.insert(destination.local);
                        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ambient_view_never_becomes_a_provided_owner() {
        for moving in [false, true] {
            let input = Local(1);
            let copy = Local(2);
            let mut owners = InputOwners::default();
            owners.seed_ambient(input);
            assert!(owners.tags(&Role::root(input)).is_empty());
            assert!(owners.retains_no_owner(input));
            let source = Place::from_local(input);
            let operand = if moving {
                Operand::Move(source)
            } else {
                Operand::Copy(source)
            };
            let mut transfer = Transfer::default();
            transfer.assign(Role::root(input), Role::root(copy), &operand, &owners);
            transfer.detach(&mut owners);
            transfer.install(&mut owners);
            assert_eq!(
                owners.tags(&Role::root(copy)),
                Tags::from_iter([Token::Derived(input)])
            );
            assert!(owners.released_provided(input));
            assert!(!owners.retains_no_owner(input));
            owners.drop_local(copy);
            assert_eq!(
                owners.retains_no_owner(input),
                !moving,
                "a malformed Move from a borrowed view cannot certify cleanup"
            );
        }
    }

    #[test]
    fn a_branch_retaining_a_copy_preserves_its_ambient_lineage() {
        let mut left = InputOwners::default();
        left.seed_ambient(Local(1));
        let mut right = left.clone();
        let mut transfer = Transfer::default();
        transfer.assign(
            Role::root(Local(1)),
            Role::root(Local(2)),
            &Operand::Copy(Place::from_local(Local(1))),
            &right,
        );
        transfer.detach(&mut right);
        transfer.install(&mut right);
        right.drop_local(Local(1));
        assert!(left.join(&right));
        assert!(!left.retains_no_owner(Local(1)));
        assert_eq!(
            left.value_tags(&Role::root(Local(1))),
            Tags::from_iter([Token::Derived(Local(1))])
        );
        left.drop_local(Local(2));
        assert!(left.retains_no_owner(Local(1)));
    }
}
