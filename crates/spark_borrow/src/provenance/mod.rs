//! Value-slot provenance for references carried across MIR control flow.

mod effects;
mod value;

use std::collections::VecDeque;

use rustc_hash::{FxHashMap, FxHashSet};
use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_mir::{
    AggregateKind, BlockId, Constant, Local, MirFunction, Operand, Place, Projection, Rvalue,
    Statement, Terminator,
};
use spark_resolve::DefId;
use spark_types::{Ty, TypeId, TypeTable};

use crate::facts::{BorrowFacts, Point};
use value::{child_type, strip_mutable, Path, Slot, Value};

// TAG [SPARK-114]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Origin {
    Loan(Point),
    Input { local: Local, path: Path },
    InputAddress { local: Local, path: Path },
    Unproven(Point),
    Frame { function: DefId, point: Point },
    Opaque { function: DefId, point: Point },
}
type Origins = FxHashSet<Origin>;
#[derive(Clone, Debug, Default)]
struct State(FxHashMap<Local, Value>);

impl State {
    fn value(&self, local: Local) -> Value {
        self.0.get(&local).cloned().unwrap_or_default()
    }

    fn set(&mut self, local: Local, value: Value) {
        if value.is_empty() {
            self.0.remove(&local);
        } else {
            self.0.insert(local, value);
        }
    }

    fn join(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (local, value) in &other.0 {
            changed |= self.0.entry(*local).or_default().join(value);
        }
        changed
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Target {
    Frame {
        local: Local,
        path: Path,
    },
    Through {
        origin: Origin,
        ty: TypeId,
        path: Path,
    },
}

struct Flow<'a> {
    function: &'a MirFunction,
    table: &'a TypeTable,
    loans: FxHashMap<Point, FxHashSet<Target>>,
    entries: Vec<Option<State>>,
    unproven: FxHashMap<Point, Origins>,
    changed: bool,
    program: Option<effects::Context<'a>>,
}

pub use effects::check_program_returns;

#[cfg(test)]
pub fn check_returns(
    function: &MirFunction,
    table: &TypeTable,
    facts: &BorrowFacts,
    diagnostics: &mut Diagnostics,
) {
    let mut flow = Flow::new(function, table, None);
    flow.solve(facts);
    flow.report_returns(facts, diagnostics);
}

impl<'a> Flow<'a> {
    fn new(
        function: &'a MirFunction,
        table: &'a TypeTable,
        program: Option<effects::Context<'a>>,
    ) -> Self {
        Self {
            function,
            table,
            loans: FxHashMap::default(),
            unproven: FxHashMap::default(),
            changed: false,
            program,
            entries: vec![None; function.blocks.len()],
        }
    }

    fn initial_state(&self) -> State {
        let mut state = State::default();
        for decl in &self.function.locals {
            let external = decl.local.0 > 0
                && decl.local.0 <= self.function.param_count + self.function.captures.len() as u32;
            if external {
                state.set(
                    decl.local,
                    value::inputs(decl.ty, decl.local, decl.ty, &[], self.table),
                );
            }
        }
        state
    }

    fn solve(&mut self, facts: &BorrowFacts) {
        if self.entries.is_empty() {
            return;
        }
        let mut successors = vec![Vec::new(); self.entries.len()];
        for (from, to) in &facts.cfg_edges {
            if from.index as usize == self.function.blocks[from.block.index()].statements.len()
                && to.index == 0
            {
                successors[from.block.index()].push(to.block);
            }
        }
        self.entries[0] = Some(self.initial_state());
        let mut pending = VecDeque::from([BlockId::ENTRY]);
        let mut queued = FxHashSet::from_iter([BlockId::ENTRY]);
        while let Some(block) = pending.pop_front() {
            queued.remove(&block);
            let Some(mut state) = self.entries[block.index()].clone() else {
                continue;
            };
            self.changed = false;
            self.transfer_block(block, &mut state);
            for successor in &successors[block.index()] {
                let entry = &mut self.entries[successor.index()];
                let changed = if let Some(entry) = entry {
                    entry.join(&state)
                } else {
                    *entry = Some(state.clone());
                    true
                };
                if changed && queued.insert(*successor) {
                    pending.push_back(*successor);
                }
            }
            if self.changed {
                for (index, entry) in self.entries.iter().enumerate() {
                    let block = BlockId(index as u32);
                    if entry.is_some()
                        && block_reads_loan_graph(&self.function.blocks[index])
                        && queued.insert(block)
                    {
                        pending.push_back(block);
                    }
                }
            }
        }
    }

    fn transfer_block(&mut self, block: BlockId, state: &mut State) {
        let body = &self.function.blocks[block.index()];
        for (index, statement) in body.statements.iter().enumerate() {
            let point = Point::new(block, index as u32);
            self.transfer_statement(statement, point, state);
        }
        if let Terminator::Call {
            func,
            args,
            destination,
            ..
        } = &body.terminator
        {
            let point = Point::new(block, body.statements.len() as u32);
            let ty = self.place_type(destination);
            let result = self.call_result(func, args, ty, point, state);
            for arg in args {
                consume(arg, state);
            }
            self.store(destination, result, state);
        }
    }

    fn transfer_statement(&mut self, statement: &Statement, point: Point, state: &mut State) {
        match statement {
            Statement::Assign { place, rvalue, .. } => {
                let result = self.rvalue(rvalue, self.place_type(place), point, state);
                consume_rvalue(rvalue, state);
                self.store(place, result, state);
            }
            Statement::IndexedAssign {
                root,
                indices,
                value,
                ..
            } => {
                let mut ty = self.function.locals[root.index()].ty;
                let mut path = Vec::new();
                for _ in indices {
                    let slot = index_slot(ty, self.table);
                    ty = child_type(ty, &slot, self.table).unwrap_or(TypeId::ERROR);
                    path.push(slot);
                }
                let stored = state.value(*value).prefixed_path(
                    &path,
                    self.function.locals[root.index()].ty,
                    self.table,
                );
                let mut current = state.value(*root);
                current.join(&stored);
                state.set(*root, current);
                state.set(*value, Value::default());
            }
            Statement::Drop { place, .. } if place.projections.is_empty() => {
                state.set(place.local, Value::default());
            }
            Statement::Drop { .. } | Statement::Nop => {}
        }
    }

    fn rvalue(&mut self, rvalue: &Rvalue, ty: TypeId, point: Point, state: &State) -> Value {
        match rvalue {
            Rvalue::Use(operand) => self.operand(operand, state),
            Rvalue::Ref { place, .. } => {
                let targets = self.reference_targets(place, state);
                let entry = self.loans.entry(point).or_default();
                let before = entry.len();
                entry.extend(targets);
                self.changed |= before != entry.len();
                Value::reference(Origin::Loan(point))
            }
            Rvalue::Aggregate(kind, operands) => self.aggregate(kind, operands, ty, state),
            Rvalue::Cast { operand, .. } => {
                let origins = self.operand(operand, state).origins();
                value::tainted(ty, &origins, self.table)
            }
            Rvalue::CollectionGetOptional { collection, .. } => {
                let slot = index_slot(self.place_type(collection), self.table);
                self.read(collection, state)
                    .project(&slot)
                    .prefixed(Slot::Optional, ty, self.table)
            }
            Rvalue::BinaryOp(..)
            | Rvalue::UnaryOp(..)
            | Rvalue::Discriminant(_)
            | Rvalue::StringInterp(_)
            | Rvalue::ListRange { .. }
            | Rvalue::RangeLen { .. } => Value::default(),
        }
    }

    fn aggregate(
        &self,
        kind: &AggregateKind,
        operands: &[Operand],
        ty: TypeId,
        state: &State,
    ) -> Value {
        let mut result = if let AggregateKind::Closure { func_def_id } = kind {
            Value::function(*func_def_id)
        } else {
            Value::default()
        };
        for (index, operand) in operands.iter().enumerate() {
            let slot = match kind {
                AggregateKind::List | AggregateKind::Set => Slot::Element,
                AggregateKind::Map => {
                    if index % 2 == 0 {
                        Slot::Key
                    } else {
                        Slot::Value
                    }
                }
                AggregateKind::Tuple => Slot::Tuple(index),
                AggregateKind::Struct { fields, .. } => Slot::Field(fields[index]),
                AggregateKind::EnumVariant { def_id, variant } => {
                    let variant = self
                        .table
                        .get_enum_variants(*def_id)
                        .into_iter()
                        .flatten()
                        .find_map(|(name, def)| (name == variant).then_some(*def));
                    variant.map_or(Slot::Tail, |variant| Slot::Variant(variant, index))
                }
                AggregateKind::Union { member_ty, .. } => {
                    if matches!(
                        self.table.resolve(strip_mutable(ty, self.table)),
                        Ty::Optional(_)
                    ) {
                        Slot::Optional
                    } else {
                        Slot::Member(*member_ty)
                    }
                }
                AggregateKind::Closure { .. } => {
                    Slot::Capture(index, operand_type(operand, self.function))
                }
                AggregateKind::Channel => continue,
            };
            result.join(&self.operand(operand, state).prefixed(slot, ty, self.table));
        }
        result
    }

    fn operand(&self, operand: &Operand, state: &State) -> Value {
        match operand {
            Operand::Copy(place) | Operand::Move(place) => self.read(place, state),
            Operand::Constant(Constant::FunctionRef(def)) => Value::function(*def),
            Operand::Constant(_) => Value::default(),
        }
    }

    fn read(&self, place: &Place, state: &State) -> Value {
        let mut value = state.value(place.local);
        let mut ty = self.function.locals[place.local.index()].ty;
        for projection in &place.projections {
            let slot = projection_slot(projection, ty, self.table);
            value = if slot == Slot::Deref {
                self.dereference(&value, ty, state, &mut FxHashSet::default())
            } else {
                value.project(&slot)
            };
            ty = child_type(ty, &slot, self.table).unwrap_or(TypeId::ERROR);
        }
        value
    }

    fn dereference(
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

    fn pointee(
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

    fn target_value(
        &self,
        target: &Target,
        ty: TypeId,
        state: &State,
        visited: &mut FxHashSet<Origin>,
    ) -> Value {
        let result = match target {
            Target::Frame { local, path } => self.project_path(
                state.value(*local),
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

    fn project_path(
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

    fn reference_targets(&self, place: &Place, state: &State) -> FxHashSet<Target> {
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

    fn projection_path(&self, mut ty: TypeId, projections: &[Projection]) -> (Path, TypeId) {
        let mut path = Vec::new();
        for projection in projections {
            let slot = projection_slot(projection, ty, self.table);
            ty = child_type(ty, &slot, self.table).unwrap_or(TypeId::ERROR);
            path.push(slot);
        }
        (path, ty)
    }

    fn place_type(&self, place: &Place) -> TypeId {
        self.projection_path(
            self.function.locals[place.local.index()].ty,
            &place.projections,
        )
        .1
    }

    fn store(&self, place: &Place, value: Value, state: &mut State) {
        if place.projections.is_empty() {
            state.set(place.local, value);
            return;
        }
        let (path, _) = self.projection_path(
            self.function.locals[place.local.index()].ty,
            &place.projections,
        );
        let value = value.prefixed_path(
            &path,
            self.function.locals[place.local.index()].ty,
            self.table,
        );
        let mut current = state.value(place.local);
        current.join(&value);
        state.set(place.local, current);
    }

    fn report_returns(&mut self, facts: &BorrowFacts, diagnostics: &mut Diagnostics) {
        let mut reported = FxHashSet::default();
        for index in 0..self.entries.len() {
            let Some(mut state) = self.entries[index].clone() else {
                continue;
            };
            let block = BlockId(index as u32);
            if !matches!(self.function.blocks[index].terminator, Terminator::Return) {
                continue;
            }
            self.transfer_block(block, &mut state);
            let value = state.value(Local::RETURN_PLACE);
            for origin in value.origins() {
                if let Some(point) = self.frame_loan(&origin, &mut FxHashSet::default()) {
                    if reported.insert(point) {
                        self.report_return(point, &value, block, facts, diagnostics);
                    }
                }
            }
        }
    }

    fn frame_loan(&self, origin: &Origin, seen: &mut FxHashSet<Origin>) -> Option<Point> {
        if !seen.insert(origin.clone()) {
            return None;
        }
        match origin {
            Origin::Loan(point) => {
                for target in self.loans.get(point).into_iter().flatten() {
                    match target {
                        Target::Frame { .. } => return Some(*point),
                        Target::Through { origin, .. } => {
                            if let Some(found) = self.frame_loan(origin, seen) {
                                return Some(found);
                            }
                        }
                    }
                }
                None
            }
            Origin::Unproven(point) => self
                .unproven
                .get(point)
                .into_iter()
                .flatten()
                .find_map(|origin| self.frame_loan(origin, seen)),
            Origin::Frame { function, point } if *function == self.function.def_id => Some(*point),
            Origin::Input { .. }
            | Origin::InputAddress { .. }
            | Origin::Frame { .. }
            | Origin::Opaque { .. } => None,
        }
    }

    fn report_return(
        &self,
        point: Point,
        value: &Value,
        block: BlockId,
        facts: &BorrowFacts,
        diagnostics: &mut Diagnostics,
    ) {
        let Some(loan) = facts.loans.iter().find(|loan| loan.issued_at == point) else {
            return;
        };
        if value.slots.contains_key(&Vec::new()) {
            crate::errors::emit_dangling_return(diagnostics, loan, self.function);
            return;
        }
        let escape_span = self.function.blocks[block.index()]
            .statements
            .iter()
            .rev()
            .find_map(|statement| match statement {
                Statement::Assign { place, span, .. } if place.local == Local::RETURN_PLACE => {
                    Some(*span)
                }
                _ => None,
            })
            .unwrap_or(self.function.span);
        diagnostics.push(
            Diagnostic::error("returned value contains a reference to local storage")
                .with_label(Label::primary(
                    escape_span,
                    "this value carries a borrow out of the function",
                ))
                .with_label(Label::secondary(
                    crate::errors::span_for_point(point, self.function),
                    "borrowed storage belongs to this function",
                )),
        );
    }
}

fn consume(operand: &Operand, state: &mut State) {
    if let Operand::Move(place) = operand {
        if place.projections.is_empty() {
            state.set(place.local, Value::default());
        }
    }
}

fn consume_rvalue(rvalue: &Rvalue, state: &mut State) {
    match rvalue {
        Rvalue::Use(operand) | Rvalue::Cast { operand, .. } | Rvalue::UnaryOp(_, operand) => {
            consume(operand, state);
        }
        Rvalue::Aggregate(_, operands) | Rvalue::StringInterp(operands) => {
            for operand in operands {
                consume(operand, state);
            }
        }
        _ => {}
    }
}

fn operand_type(operand: &Operand, function: &MirFunction) -> TypeId {
    match operand {
        Operand::Copy(place) | Operand::Move(place) => function.locals[place.local.index()].ty,
        Operand::Constant(_) => TypeId::ERROR,
    }
}

fn index_slot(ty: TypeId, table: &TypeTable) -> Slot {
    if matches!(table.resolve(strip_mutable(ty, table)), Ty::Map(..)) {
        Slot::Value
    } else {
        Slot::Element
    }
}

fn projection_slot(projection: &Projection, ty: TypeId, table: &TypeTable) -> Slot {
    match projection {
        Projection::Field(name) => {
            if matches!(table.resolve(strip_mutable(ty, table)), Ty::Tuple(_)) {
                name.as_str().parse().map_or(Slot::Tail, Slot::Tuple)
            } else {
                Slot::Field(*name)
            }
        }
        Projection::Index(_) => index_slot(ty, table),
        Projection::Deref => Slot::Deref,
        Projection::UnionPayload(member) => {
            if matches!(table.resolve(strip_mutable(ty, table)), Ty::Optional(_)) {
                Slot::Optional
            } else {
                Slot::Member(*member)
            }
        }
        Projection::EnumField { variant, index } => Slot::Variant(*variant, *index as usize),
    }
}

fn operand_dereferences(operand: &Operand) -> bool {
    match operand {
        Operand::Copy(place) | Operand::Move(place) => place
            .projections
            .iter()
            .any(|projection| matches!(projection, Projection::Deref)),
        Operand::Constant(_) => false,
    }
}

fn block_reads_loan_graph(block: &spark_mir::BasicBlock) -> bool {
    let statements = block.statements.iter().any(|statement| match statement {
        Statement::Assign { rvalue, .. } => match rvalue {
            Rvalue::Use(operand) | Rvalue::Cast { operand, .. } | Rvalue::UnaryOp(_, operand) => {
                operand_dereferences(operand)
            }
            Rvalue::Aggregate(_, operands) | Rvalue::StringInterp(operands) => {
                operands.iter().any(operand_dereferences)
            }
            Rvalue::Ref { place, .. } | Rvalue::Discriminant(place) => place
                .projections
                .iter()
                .any(|projection| matches!(projection, Projection::Deref)),
            Rvalue::CollectionGetOptional { collection, index } => {
                [collection, index].iter().any(|place| {
                    place
                        .projections
                        .iter()
                        .any(|projection| matches!(projection, Projection::Deref))
                })
            }
            _ => false,
        },
        _ => false,
    });
    statements
        // Callee summaries can dereference an input without a Deref projection at the call site.
        || matches!(&block.terminator, Terminator::Call { .. })
}
