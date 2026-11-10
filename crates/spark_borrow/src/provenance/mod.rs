//! Value-slot provenance for references carried across MIR control flow.

mod address;
mod aliases;
mod callbacks;
mod channels;
mod collections;
mod demand;
mod effects;
mod obligations;
mod read;
mod storage;
mod tasks;
mod value;
mod writes;

use std::collections::VecDeque;

use rustc_hash::{FxHashMap, FxHashSet};
use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_mir::{
    AggregateKind, BlockId, Local, MirFunction, Operand, Place, Projection, Rvalue, Statement,
    Terminator,
};
use spark_resolve::DefId;
use spark_types::{Ty, TypeId, TypeTable};

use crate::facts::{BorrowFacts, Point};
use read::{AccessKind, Accesses};
use value::{child_type, strip_mutable, Path, Slot, Value};

// TAG [SPARK-114]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Origin {
    ValidationHole,
    Loan(Point),
    Input { local: Local, path: Path },
    Unproven(Point),
    Frame { function: DefId, point: Point },
    Opaque { function: DefId, point: Point },
}
type Origins = FxHashSet<Origin>;
#[derive(Clone, Debug, Default)]
struct State {
    queues: FxHashMap<(channels::Target, TypeId), Value>,
    tasks: FxHashMap<tasks::Target, tasks::Record>,
    future_copies: tasks::Copies,
    exposed_queues: FxHashSet<channels::Target>,
    values: FxHashMap<Local, Value>,
    live: FxHashSet<Local>,
    cells: FxHashMap<Local, Value>,
    external: FxHashMap<aliases::Port, Value>,
    external_defined: writes::Coverage,
    modified_views: FxHashSet<aliases::View>,
    invalidated: FxHashMap<(Local, Path), FxHashSet<Point>>,
}

impl State {
    fn value(&self, local: Local) -> Value {
        self.values.get(&local).cloned().unwrap_or_default()
    }

    fn cell(&self, local: Local) -> Value {
        self.cells.get(&local).cloned().unwrap_or_default()
    }

    fn set(&mut self, local: Local, mut value: Value) {
        value.sources.clear();
        self.live.insert(local);
        Self::set_value(&mut self.cells, local, value.clone());
        Self::set_value(&mut self.values, local, value);
    }

    fn set_value(values: &mut FxHashMap<Local, Value>, local: Local, value: Value) {
        if value.is_empty() {
            values.remove(&local);
        } else {
            values.insert(local, value);
        }
    }

    fn consume(&mut self, local: Local) {
        self.values.remove(&local);
        self.live.remove(&local);
        self.future_copies.forget(local);
    }

    fn join(&mut self, other: &Self) -> bool {
        let before = self.live.len();
        self.live.extend(other.live.iter().copied());
        let mut changed = before != self.live.len();
        changed |= self.future_copies.intersect(&other.future_copies);
        for (target, record) in &other.tasks {
            if let Some(current) = self.tasks.get_mut(target) {
                changed |= current.join(record);
            } else {
                self.tasks.insert(target.clone(), record.clone());
                changed = true;
            }
        }
        let before = self.exposed_queues.len();
        self.exposed_queues
            .extend(other.exposed_queues.iter().cloned());
        changed |= before != self.exposed_queues.len();
        for (target, value) in &other.queues {
            changed |= self.queues.entry(target.clone()).or_default().join(value);
        }
        for (local, value) in &other.values {
            changed |= self.values.entry(*local).or_default().join(value);
        }
        for (local, value) in &other.cells {
            changed |= self.cells.entry(*local).or_default().join(value);
        }
        for (local, value) in &other.external {
            let existed = self.external.contains_key(local);
            changed |= self.external.entry(local.clone()).or_default().join(value) || !existed;
        }
        let before = self.modified_views.len();
        self.modified_views
            .extend(other.modified_views.iter().cloned());
        changed |= before != self.modified_views.len();
        changed |= self.external_defined.intersect(&other.external_defined);
        for (storage, points) in &other.invalidated {
            let entry = self.invalidated.entry(storage.clone()).or_default();
            let before = entry.len();
            entry.extend(points);
            changed |= before != entry.len();
        }
        changed
    }
}

struct Flow<'a> {
    function: &'a MirFunction,
    table: &'a TypeTable,
    entries: Vec<Option<State>>,
    program: Option<effects::Context<'a>>,
    aliases: aliases::Aliases,
    callables: callbacks::Inputs,
    requests: FxHashSet<aliases::Key>,
    written_inputs: FxHashSet<aliases::View>,
    unknown_copy_clobber: bool,
    obligations: FxHashMap<(Point, obligations::Boundary), Value>,
    queue_sends: FxHashMap<(Point, channels::Target, TypeId), Value>,
    queue_ports: FxHashSet<aliases::Port>,
    queue_escapes: FxHashMap<Point, Value>,
    accesses: FxHashMap<Point, Accesses>,
    transfers: FxHashMap<Point, demand::Transfer>,
    invalid_reads: FxHashSet<(Point, Point, Origin)>,
}

pub use effects::analyze_program;

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
    flow.report_obligations(diagnostics);
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
            program,
            aliases: aliases::Aliases::default(),
            callables: callbacks::Inputs::default(),
            requests: FxHashSet::default(),
            written_inputs: FxHashSet::default(),
            unknown_copy_clobber: false,
            obligations: FxHashMap::default(),
            queue_sends: FxHashMap::default(),
            queue_ports: FxHashSet::default(),
            queue_escapes: FxHashMap::default(),
            accesses: FxHashMap::default(),
            transfers: FxHashMap::default(),
            invalid_reads: FxHashSet::default(),
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
                    self.symbolic_input(decl.ty, decl.local, decl.ty, &[]),
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
            if !self.transfer_block(block, &mut state) {
                continue;
            }
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
        }
    }

    fn transfer_block(&mut self, block: BlockId, state: &mut State) -> bool {
        let body = &self.function.blocks[block.index()];
        for (index, statement) in body.statements.iter().enumerate() {
            let point = Point::new(block, index as u32);
            self.transfers.insert(point, demand::Transfer::default());
            self.transfer_statement(statement, point, state);
        }
        self.transfer_terminator(block, state)
    }

    fn transfer_terminator(&mut self, block: BlockId, state: &mut State) -> bool {
        let body = &self.function.blocks[block.index()];
        let point = Point::new(block, body.statements.len() as u32);
        self.transfers.insert(point, demand::Transfer::default());
        match &body.terminator {
            Terminator::Call {
                func,
                args,
                destination,
                ..
            } => {
                let ty = self.place_type(destination);
                let mut accesses = Accesses::default();
                self.read_operand(func, state, &mut accesses);
                let exclusive = args.iter().any(|argument| match argument {
                    Operand::Copy(place) | Operand::Move(place) => {
                        let ty = self.place_type(place);
                        matches!(self.table.resolve(strip_mutable(ty, self.table)), Ty::Ref { mutable, .. }
                            if *mutable || matches!(self.table.resolve(ty), Ty::Mutable(_)))
                    }
                    Operand::Constant(_) => false,
                });
                for argument in args {
                    let value = self.read_operand(argument, state, &mut accesses);
                    if exclusive {
                        accesses.demands.extend(value.address_sources());
                    }
                }
                self.record_accesses(point, accesses, state);
                let Some(outcome) = self.call_result(func, args, ty, point, state) else {
                    return false;
                };
                for arg in args {
                    if let Some(local) = consume(arg, state) {
                        self.record_consumed(point, local);
                    }
                }
                let common = self.transfers[&point].clone();
                let mut alternatives: Option<(State, demand::Transfer)> = None;
                for alternative in outcome {
                    self.transfers.insert(point, common.clone());
                    let mut after = state.clone();
                    for local in alternative.clobbered_copies {
                        after.future_copies.forget(local);
                    }
                    self.import_tasks(alternative.tasks, point, &mut after);
                    if let Some((local, result)) = alternative.completed_task {
                        self.complete_future(local, &result, &mut after);
                    }

                    for write in alternative.queue_contents {
                        self.import_queue_contents(
                            &write.target,
                            &write.value,
                            write.ty,
                            point,
                            &mut after,
                        );
                    }
                    for write in alternative.queues {
                        self.queue_send(&write.target, &write.value, write.ty, point, &mut after);
                    }
                    for write in alternative.memory {
                        self.store_references(
                            &write.target,
                            &write.value,
                            write.ty,
                            point,
                            &mut after,
                        );
                    }
                    for (place, value) in alternative.place_writes {
                        self.store(&place, value, point, &mut after);
                    }
                    self.store(destination, alternative.returned, point, &mut after);
                    let transfer = self.transfers[&point].clone();
                    if let Some((joined, ledger)) = &mut alternatives {
                        joined.join(&after);
                        ledger.join_alternative(&transfer);
                    } else {
                        alternatives = Some((after, transfer));
                    }
                }
                if let Some((after, transfer)) = alternatives {
                    *state = after;
                    self.transfers.insert(point, transfer);
                }
            }
            Terminator::SwitchInt { discriminant, .. } => {
                let mut accesses = Accesses::default();
                self.read_operand(discriminant, state, &mut accesses);
                self.record_accesses(point, accesses, state);
                if let Some(local) = consume(discriminant, state) {
                    self.record_consumed(point, local);
                }
            }
            Terminator::Goto { .. } | Terminator::Return | Terminator::Unreachable => {}
        }
        true
    }

    fn transfer_statement(&mut self, statement: &Statement, point: Point, state: &mut State) {
        match statement {
            Statement::Assign { place, rvalue, .. } => {
                let future_copy = self.future_copy_witness(rvalue, state);
                let result = self.rvalue(rvalue, self.place_type(place), point, state);
                let mut accesses = Accesses::default();
                self.read_access(place, state, AccessKind::Address, &mut accesses);
                self.record_accesses(point, accesses, state);
                for local in consume_rvalue(rvalue, state) {
                    self.record_consumed(point, local);
                }
                self.store(place, result, point, state);
                self.store_future_copy(place, future_copy, state);
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
                state.consume(*value);
                self.record_consumed(point, *value);
            }
            Statement::Drop { place, .. } => {
                self.observe_pending_tasks(point, state);
                let mut accesses = Accesses::default();
                self.read_access(place, state, AccessKind::Value, &mut accesses);
                self.record_accesses(point, accesses, state);
                self.invalidate(place, point, state);
                if place.projections.is_empty() {
                    state.consume(place.local);
                    self.record_consumed(point, place.local);
                }
                self.observe_pending_tasks(point, state);
            }
            Statement::Nop => {}
        }
    }

    fn rvalue(&mut self, rvalue: &Rvalue, ty: TypeId, point: Point, state: &State) -> Value {
        let mut accesses = Accesses::default();
        let value = match rvalue {
            Rvalue::Use(operand) => self.read_operand(operand, state, &mut accesses),
            Rvalue::Ref { place, .. } => {
                self.read_access(place, state, AccessKind::Address, &mut accesses);
                let metadata = self.reference_metadata(place, point, state);
                let result = Value::reference(Origin::Loan(point)).with_metadata(&metadata);
                accesses.observe_reference(&result, Some(ty), &[], AccessKind::Address);
                result
            }
            Rvalue::Aggregate(AggregateKind::Channel, _) => {
                channels::allocation(self.function.def_id, point, ty)
            }
            Rvalue::Aggregate(kind, operands) => {
                self.aggregate(kind, operands, ty, state, &mut accesses)
            }
            Rvalue::Cast { operand, .. } => {
                let source = self.read_operand(operand, state, &mut accesses);
                let source_ty = match operand {
                    Operand::Copy(place) | Operand::Move(place) => self.place_type(place),
                    Operand::Constant(_) => TypeId::ERROR,
                };
                if strip_mutable(source_ty, self.table) == strip_mutable(ty, self.table) {
                    source
                } else {
                    value::tainted_from(ty, &source, self.table)
                }
            }
            Rvalue::CollectionGetOptional { collection, index } => {
                self.read_access(index, state, AccessKind::Value, &mut accesses);
                let slot = index_slot(self.place_type(collection), self.table);
                self.read_access(collection, state, AccessKind::Value, &mut accesses)
                    .project(&slot)
                    .prefixed(Slot::Optional, ty, self.table)
            }
            Rvalue::BinaryOp(_, left, right) => {
                self.read_operand(left, state, &mut accesses);
                self.read_operand(right, state, &mut accesses);
                Value::default()
            }
            Rvalue::UnaryOp(_, operand) => {
                self.read_operand(operand, state, &mut accesses);
                Value::default()
            }
            Rvalue::Discriminant(place) => {
                self.read_access(place, state, AccessKind::Address, &mut accesses);
                Value::default()
            }
            Rvalue::StringInterp(operands) => {
                for operand in operands {
                    self.read_operand(operand, state, &mut accesses);
                }
                Value::default()
            }
            Rvalue::ListRange {
                start, end, step, ..
            }
            | Rvalue::RangeLen {
                start, end, step, ..
            } => {
                for operand in [start, end, step] {
                    self.read_operand(operand, state, &mut accesses);
                }
                Value::default()
            }
        };
        self.record_accesses(point, accesses, state);
        value
    }

    fn aggregate(
        &self,
        kind: &AggregateKind,
        operands: &[Operand],
        ty: TypeId,
        state: &State,
        accesses: &mut Accesses,
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
                AggregateKind::Closure { func_def_id } => {
                    Slot::Capture(*func_def_id, index, operand_type(operand, self.function))
                }
                AggregateKind::Channel => continue,
            };
            let mut value = self.read_operand(operand, state, accesses);
            if let Slot::Capture(owner, index, capture_ty) = slot {
                if matches!(
                    self.table.resolve(strip_mutable(capture_ty, self.table)),
                    Ty::Ref { .. }
                ) {
                    value = value.with_reference_capture(value::ReferenceCapture {
                        owner,
                        index,
                        ty: capture_ty,
                    });
                } else if matches!(
                    self.table
                        .resolve(value::strip_mutable(capture_ty, self.table)),
                    Ty::Channel { .. }
                ) {
                    value = value.with_channel_capture(channels::Capture {
                        owner,
                        index,
                        ty: capture_ty,
                    });
                } else if matches!(
                    self.table.resolve(strip_mutable(capture_ty, self.table)),
                    Ty::Future { .. }
                ) {
                    value = value.with_future_capture(tasks::Capture {
                        owner,
                        index,
                        ty: capture_ty,
                    });
                }
            }
            result.join(&value.prefixed(slot, ty, self.table));
        }
        result
    }

    fn store(&mut self, place: &Place, value: Value, point: Point, state: &mut State) {
        if place
            .projections
            .iter()
            .any(|projection| matches!(projection, Projection::Deref))
        {
            let targets = self.reference_metadata(place, point, state);
            self.store_references(&targets, &value, self.place_type(place), point, state);
            return;
        }
        state.future_copies.forget(place.local);
        self.record_store(point, place, &value);
        if place.projections.is_empty() {
            state
                .invalidated
                .retain(|(local, _), _| *local != place.local);
            state.set(place.local, value);
            return;
        }
        let (path, _) = self.projection_path(
            self.function.locals[place.local.index()].ty,
            &place.projections,
        );
        state.invalidated.retain(|(local, invalid), _| {
            *local != place.local || !storage::prefix(&path, invalid)
        });
        let value = value.prefixed_path(
            &path,
            self.function.locals[place.local.index()].ty,
            self.table,
        );
        let mut current = state.cell(place.local);
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
            self.report_external_stores(&state, block, diagnostics);
            let value = self.reachable_queue_payloads(&state.value(Local::RETURN_PLACE), &state);
            for root in value.frame_sources(self.function.def_id) {
                if reported.insert(root) {
                    self.report_return(root, &value, block, facts, diagnostics);
                }
            }
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
        if value.leaves.keys().any(|(path, _, _)| path.is_empty()) {
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

fn consume(operand: &Operand, state: &mut State) -> Option<Local> {
    if let Operand::Move(place) = operand {
        if place.projections.is_empty() {
            state.consume(place.local);
            return Some(place.local);
        }
    }
    None
}

fn consume_rvalue(rvalue: &Rvalue, state: &mut State) -> Vec<Local> {
    match rvalue {
        Rvalue::Use(operand) | Rvalue::Cast { operand, .. } | Rvalue::UnaryOp(_, operand) => {
            consume(operand, state).into_iter().collect()
        }
        Rvalue::Aggregate(_, operands) | Rvalue::StringInterp(operands) => operands
            .iter()
            .filter_map(|operand| consume(operand, state))
            .collect(),
        _ => Vec::new(),
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
