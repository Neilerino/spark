//! Direct function summaries use the same slot transfer engine as diagnostics.

use rustc_hash::{FxHashMap, FxHashSet};
use spark_diagnostics::Diagnostics;
use spark_mir::{BlockId, Local, Mir, MirFunction, Operand, Terminator};
use spark_resolve::DefId;
use spark_types::{TypeId, TypeTable};

use super::{
    child_type, value, Access, AccessKind, Accesses, Flow, Origin, Origins, Path, Point, Slot,
    State, Target, Value,
};

#[derive(Clone, Default)]
pub(super) struct Summary {
    returned: Value,
    observed: Accesses,
}

impl Summary {
    fn join(&mut self, other: &Self) -> bool {
        let returned = self.returned.join(&other.returned);
        let before = self.observed.len();
        self.observed.extend(other.observed.iter().cloned());
        returned || before != self.observed.len()
    }
}

#[derive(Clone, Copy)]
pub(super) struct Context<'a> {
    mir: &'a Mir,
    summaries: &'a FxHashMap<DefId, Summary>,
}

#[derive(Clone, Copy)]
struct CallActuals<'a> {
    callee: &'a Value,
    arguments: &'a [Value],
    ty: TypeId,
    point: Point,
    state: &'a State,
}

pub fn analyze_program(
    mir: &Mir,
    table: &TypeTable,
    diagnostics: &mut Diagnostics,
) -> FxHashMap<DefId, Vec<crate::regions::Region>> {
    let facts: FxHashMap<_, _> = mir
        .functions
        .iter()
        .map(|(def, function)| (*def, crate::facts::extract_facts(function)))
        .collect();
    let mut summaries: FxHashMap<_, _> = mir
        .functions
        .keys()
        .map(|def| (*def, Summary::default()))
        .collect();
    loop {
        let context = Context {
            mir,
            summaries: &summaries,
        };
        let mut next = summaries.clone();
        let mut changed = false;
        for (def, function) in &mir.functions {
            let mut flow = Flow::new(function, table, Some(context));
            flow.solve(&facts[def]);
            changed |= next.entry(*def).or_default().join(&flow.summary());
        }
        summaries = next;
        if !changed {
            break;
        }
    }
    let context = Context {
        mir,
        summaries: &summaries,
    };
    let mut regions = FxHashMap::default();
    for (def, function) in &mir.functions {
        let mut flow = Flow::new(function, table, Some(context));
        flow.solve(&facts[def]);
        flow.report_returns(&facts[def], diagnostics);
        flow.report_reads(diagnostics);
        regions.insert(*def, flow.carrier_regions(&facts[def]));
    }
    regions
}

impl Flow<'_> {
    pub(super) fn call_result(
        &mut self,
        func: &Operand,
        args: &[Operand],
        ty: TypeId,
        point: Point,
        state: &State,
    ) -> Value {
        let callee = self.operand(func, state);
        let arguments: Vec<_> = args.iter().map(|arg| self.operand(arg, state)).collect();
        let actuals = CallActuals {
            callee: &callee,
            arguments: &arguments,
            ty,
            point,
            state,
        };
        let targets = callee.callees.get(&Vec::new());
        let mut result = Value::default();
        let mut unknown = targets.is_none_or(FxHashSet::is_empty);
        for def in targets.into_iter().flatten() {
            if let Some((function, summary)) = self.program.and_then(|context| {
                Some((context.mir.functions.get(def)?, context.summaries.get(def)?))
            }) {
                self.observe(summary, function, actuals);
                result.join(&self.bind(&summary.returned, function, actuals));
            } else {
                unknown = true;
            }
        }
        if unknown {
            let mut origins = callee.origins();
            for arg in &arguments {
                origins.extend(arg.origins());
            }
            let mut accesses: Accesses = arguments
                .iter()
                .flat_map(Value::addresses)
                .map(|origin| Access {
                    origin,
                    selection: Vec::new(),
                    kind: AccessKind::Value,
                })
                .collect();
            for argument in &arguments {
                accesses.demands.extend(argument.address_sources());
            }
            self.record_accesses(point, accesses, state);
            self.retain_unknown(point, &origins);
            origins.insert(Origin::Unproven(point));
            let mut tainted = value::tainted(ty, &origins, self.table);
            let parents: Origins = arguments
                .iter()
                .chain(std::iter::once(&callee))
                .flat_map(|value| {
                    value
                        .origins()
                        .into_iter()
                        .chain(value.support.values().flatten().cloned())
                })
                .collect();
            for (path, origins) in &tainted.slots {
                for origin in origins {
                    tainted
                        .support
                        .insert((path.clone(), origin.clone()), parents.clone());
                }
            }
            let sources: super::demand::Demand = arguments
                .iter()
                .chain(std::iter::once(&callee))
                .flat_map(Value::all_sources)
                .collect();
            for (path, origins) in &tainted.slots {
                for origin in origins {
                    tainted
                        .sources
                        .insert((path.clone(), origin.clone()), sources.clone());
                }
            }
            result.join(&tainted);
        }
        result
    }

    fn retain_unknown(&mut self, point: Point, origins: &Origins) {
        let entry = self.unproven.entry(point).or_default();
        let before = entry.len();
        entry.extend(origins.iter().cloned());
        self.changed |= before != entry.len();
    }

    fn summary(&mut self) -> Summary {
        let mut summary = Summary::default();
        for index in 0..self.entries.len() {
            if !matches!(self.function.blocks[index].terminator, Terminator::Return) {
                continue;
            }
            let Some(mut state) = self.entries[index].clone() else {
                continue;
            };
            self.transfer_block(BlockId(index as u32), &mut state);
            let value = state.value(Local::RETURN_PLACE);
            for (path, origins) in &value.slots {
                let mut exported = Origins::default();
                for origin in origins {
                    exported.extend(self.boundary_origins(origin, &mut FxHashSet::default()));
                }
                summary.returned.insert(path.clone(), exported);
            }
            for (path, callees) in &value.callees {
                summary
                    .returned
                    .callees
                    .entry(path.clone())
                    .or_default()
                    .extend(callees);
            }
        }
        for access in self.accesses.values().flatten() {
            for origin in self.boundary_origins(&access.origin, &mut FxHashSet::default()) {
                if matches!(origin, Origin::Input { .. } | Origin::InputAddress { .. }) {
                    let ty = self.input_pointee_type(&origin);
                    summary.observed.insert(Access {
                        origin,
                        selection: value::normalize(ty, access.selection.clone(), self.table),
                        kind: access.kind,
                    });
                }
            }
        }
        summary
    }

    fn boundary_origins(&self, origin: &Origin, seen: &mut FxHashSet<Origin>) -> Origins {
        if !seen.insert(origin.clone()) {
            return Origins::default();
        }
        let mut result = Origins::default();
        match origin {
            Origin::Loan(point) => {
                for target in self.loans.get(point).into_iter().flatten() {
                    match target {
                        Target::Frame { .. } => {
                            result.insert(Origin::Frame {
                                function: self.function.def_id,
                                point: *point,
                            });
                        }
                        Target::Through { origin, path, .. } => {
                            for source in self.boundary_origins(origin, seen) {
                                let projected = match source {
                                    Origin::Input {
                                        local,
                                        path: mut source_path,
                                    } => {
                                        source_path.push(Slot::Deref);
                                        source_path.extend(path.iter().cloned());
                                        self.input_address(local, source_path)
                                    }
                                    Origin::InputAddress {
                                        local,
                                        path: mut source_path,
                                    } => {
                                        source_path.extend(path.iter().cloned());
                                        self.input_address(local, source_path)
                                    }
                                    other => other,
                                };
                                result.insert(projected);
                            }
                        }
                    }
                }
            }
            Origin::Unproven(point) => {
                result.insert(Origin::Opaque {
                    function: self.function.def_id,
                    point: *point,
                });
                for dependency in self.unproven.get(point).into_iter().flatten() {
                    result.extend(self.boundary_origins(dependency, seen));
                }
            }
            Origin::Input { .. }
            | Origin::InputAddress { .. }
            | Origin::Frame { .. }
            | Origin::Opaque { .. } => {
                result.insert(origin.clone());
            }
        }
        seen.remove(origin);
        result
    }

    fn input_address(&self, local: Local, path: Path) -> Origin {
        Origin::InputAddress {
            local,
            path: value::normalize(self.function.locals[local.index()].ty, path, self.table),
        }
    }

    fn bind(&mut self, summary: &Value, function: &MirFunction, actuals: CallActuals<'_>) -> Value {
        let CallActuals {
            ty, point, state, ..
        } = actuals;
        let mut result = Value::default();
        for (path, callees) in &summary.callees {
            result
                .callees
                .entry(path.clone())
                .or_default()
                .extend(callees);
        }
        for (path, origins) in &summary.slots {
            for origin in origins {
                match origin {
                    Origin::Input {
                        local,
                        path: source_path,
                    } => {
                        let source = Self::actual_input(*local, function, actuals);
                        let source_ty = function.locals[local.index()].ty;
                        let selected = self.select_input(source, source_ty, source_path, state);
                        if source_path.last() == Some(&Slot::Tail)
                            && path.last() == Some(&Slot::Tail)
                        {
                            result.join(&selected.prefixed_path(
                                &path[..path.len() - 1],
                                ty,
                                self.table,
                            ));
                        } else if source_path.contains(&Slot::Tail) {
                            let target = self.selected_type(ty, path);
                            let expected = if let spark_types::Ty::Ref { inner, .. } =
                                self.table.resolve(value::strip_mutable(target, self.table))
                            {
                                Some(*inner)
                            } else {
                                None
                            };
                            let selected_ty = self.selected_type(
                                source_ty,
                                &source_path
                                    .iter()
                                    .take_while(|slot| **slot != Slot::Tail)
                                    .cloned()
                                    .collect::<Vec<_>>(),
                            );
                            let selected =
                                self.tail_reference_value(selected, selected_ty, expected, state);
                            result.insert_addresses(path, &selected);
                        } else {
                            result.insert_addresses(path, &selected);
                        }
                    }
                    Origin::InputAddress {
                        local,
                        path: source_path,
                    } => {
                        let source = Self::actual_input(*local, function, actuals);
                        let source_ty = function.locals[local.index()].ty;
                        let selected = self.bind_address(source, source_ty, source_path, actuals);
                        result.insert_addresses(path, &selected);
                    }
                    Origin::Opaque { .. } => result.insert(
                        path.clone(),
                        std::iter::once(Origin::Unproven(point)).collect(),
                    ),
                    Origin::Frame { .. } => {
                        result.insert(path.clone(), std::iter::once(origin.clone()).collect());
                    }
                    Origin::Loan(_) | Origin::Unproven(_) => {
                        unreachable!("summary origins must be exported")
                    }
                }
            }
        }
        if result.origins().contains(&Origin::Unproven(point)) {
            self.retain_unknown(point, &result.origins());
        }
        result
    }

    fn input_pointee_type(&self, origin: &Origin) -> TypeId {
        let (local, path, address) = match origin {
            Origin::Input { local, path } => (*local, path, false),
            Origin::InputAddress { local, path } => (*local, path, true),
            _ => return TypeId::ERROR,
        };
        let mut ty = self.function.locals[local.index()].ty;
        for slot in path {
            ty = child_type(ty, slot, self.table).unwrap_or(TypeId::ERROR);
        }
        if address {
            ty
        } else {
            child_type(ty, &Slot::Deref, self.table).unwrap_or(TypeId::ERROR)
        }
    }

    fn observe(&mut self, summary: &Summary, function: &MirFunction, actuals: CallActuals<'_>) {
        let mut accesses = Accesses::default();
        for access in &summary.observed {
            match &access.origin {
                Origin::Input { local, path } => {
                    let source = Self::actual_input(*local, function, actuals);
                    let selected = self.select_input(
                        source,
                        function.locals[local.index()].ty,
                        path,
                        actuals.state,
                    );
                    let selected = if path.contains(&Slot::Tail) {
                        self.tail_reference_value(selected, TypeId::ERROR, None, actuals.state)
                    } else {
                        selected
                    };
                    accesses.demands.extend(selected.address_sources());
                    let origins = selected.addresses();
                    for origin in origins {
                        accesses.insert(Access {
                            origin,
                            selection: access.selection.clone(),
                            kind: access.kind,
                        });
                    }
                }
                Origin::InputAddress { local, path } => {
                    let source = Self::actual_input(*local, function, actuals);
                    let ty = function.locals[local.index()].ty;
                    if let Some(index) = path
                        .iter()
                        .rposition(|slot| *slot == Slot::Deref)
                        .filter(|_| !path.contains(&Slot::Tail))
                    {
                        let selected =
                            self.select_input(source, ty, &path[..index].to_vec(), actuals.state);
                        accesses.demands.extend(selected.address_sources());
                        for origin in selected.addresses() {
                            accesses.insert(Access {
                                origin,
                                selection: [path[index + 1..].to_vec(), access.selection.clone()]
                                    .concat(),
                                kind: access.kind,
                            });
                        }
                    } else {
                        accesses.demands.extend(source.all_sources());
                        for origin in source.origins() {
                            accesses.insert(Access {
                                origin,
                                selection: vec![Slot::Tail],
                                kind: access.kind,
                            });
                        }
                    }
                }
                Origin::Loan(_)
                | Origin::Unproven(_)
                | Origin::Frame { .. }
                | Origin::Opaque { .. } => {}
            }
        }
        self.record_accesses(actuals.point, accesses, actuals.state);
    }

    fn actual_input(local: Local, function: &MirFunction, actuals: CallActuals<'_>) -> Value {
        if local.0 > 0 && local.0 <= function.param_count {
            actuals
                .arguments
                .get(local.index() - 1)
                .cloned()
                .unwrap_or_default()
        } else if local.0 > function.param_count {
            actuals
                .callee
                .capture(local.index() - function.param_count as usize - 1)
        } else {
            Value::default()
        }
    }

    fn bind_address(
        &mut self,
        source: Value,
        mut ty: TypeId,
        path: &Path,
        actuals: CallActuals<'_>,
    ) -> Value {
        let deref = path.iter().rposition(|slot| *slot == Slot::Deref);
        let Some(index) = deref.filter(|_| !path.contains(&Slot::Tail)) else {
            let mut origins = source.origins();
            self.retain_unknown(actuals.point, &origins);
            origins.insert(Origin::Unproven(actuals.point));
            let mut result = Value::default();
            for origin in origins {
                result.insert_sourced(Vec::new(), origin, source.all_sources());
            }
            let parents: Origins = source
                .origins()
                .into_iter()
                .chain(source.support.values().flatten().cloned())
                .collect();
            return result.with_address_support(&parents);
        };
        let selected = self.select_input(source, ty, &path[..index].to_vec(), actuals.state);
        for slot in &path[..=index] {
            ty = child_type(ty, slot, self.table).unwrap_or(TypeId::ERROR);
        }
        let targets: FxHashSet<_> = selected
            .addresses()
            .into_iter()
            .map(|origin| Target::Through {
                origin,
                ty,
                path: path[index + 1..].to_vec(),
            })
            .collect();
        let entry = self.loans.entry(actuals.point).or_default();
        let before = entry.len();
        entry.extend(targets);
        self.changed |= before != entry.len();
        let mut accesses = Accesses::default();
        accesses.demands.extend(selected.address_sources());
        self.record_accesses(actuals.point, accesses, actuals.state);
        let mut result = Value::reference(Origin::Loan(actuals.point))
            .with_address_support(&selected.address_support());
        result.insert_sourced(
            Vec::new(),
            Origin::Loan(actuals.point),
            selected.address_sources(),
        );
        result
    }

    fn select_input(&self, mut value: Value, mut ty: TypeId, path: &Path, state: &State) -> Value {
        for slot in path {
            if *slot == Slot::Tail {
                break;
            }
            value = if *slot == Slot::Deref {
                self.dereference(&value, ty, state, &mut FxHashSet::default())
            } else {
                value.project(slot)
            };
            ty = child_type(ty, slot, self.table).unwrap_or(TypeId::ERROR);
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_binding_recomputes_when_reborrow_targets_grow() {
        use spark_mir::{
            AggregateKind, BasicBlock, Constant, LocalDecl, Place, Projection, Rvalue, Statement,
        };
        use spark_span::Span;
        use spark_types::{IntWidth, LifetimeId, Ty};
        use std::collections::HashMap;

        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let string = table.intern(Ty::String);
        let reference = table.intern(Ty::Ref {
            inner: int,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let map = table.intern(Ty::Map(string, reference));
        let map_ref = table.intern(Ty::Ref {
            inner: map,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let span = Span::DUMMY;
        let local = |index| Place::from_local(Local(index));
        let copy = |index| Operand::Copy(local(index));
        let assign = |index, rvalue| Statement::Assign {
            place: local(index),
            rvalue,
            span,
        };
        let borrow = |index| Rvalue::Ref {
            place: local(index),
            mutable: false,
        };
        let block = |statements, terminator| BasicBlock {
            statements,
            terminator,
            is_cleanup: false,
        };
        let locals = |types: &[TypeId]| {
            types
                .iter()
                .enumerate()
                .map(|(index, ty)| LocalDecl {
                    local: Local(index as u32),
                    ty: *ty,
                    name: None,
                    is_user_variable: false,
                    mutable: true,
                    span,
                })
                .collect()
        };
        let helper_id = DefId::new(9000);
        let caller_id = DefId::new(9001);
        let helper = MirFunction {
            def_id: helper_id,
            param_count: 1,
            return_type: map,
            locals: locals(&[map, map_ref]),
            blocks: vec![block(
                vec![assign(
                    0,
                    Rvalue::Use(Operand::Copy(Place {
                        local: Local(1),
                        projections: vec![Projection::Deref],
                    })),
                )],
                Terminator::Return,
            )],
            captures: vec![],
            is_async: false,
            span,
        };
        let aggregate = |reference_local| {
            Rvalue::Aggregate(
                AggregateKind::Map,
                vec![
                    Operand::Constant(Constant::String(spark_intern::sym("one"))),
                    copy(reference_local),
                ],
            )
        };
        let caller = MirFunction {
            def_id: caller_id,
            param_count: 1,
            return_type: map,
            // Return, input Ref, local Int, local Ref, safe/unsafe maps,
            // safe/unsafe map Refs, selected Ref, reborrow, call output.
            locals: locals(&[
                map, reference, int, reference, map, map, map_ref, map_ref, map_ref, map_ref, map,
            ]),
            blocks: vec![
                block(
                    vec![
                        assign(2, Rvalue::Use(Operand::Constant(Constant::Int(7)))),
                        assign(3, borrow(2)),
                        assign(4, aggregate(1)),
                        assign(5, aggregate(3)),
                        assign(6, borrow(4)),
                        assign(7, borrow(5)),
                        assign(8, Rvalue::Use(copy(6))),
                    ],
                    Terminator::Goto { target: BlockId(1) },
                ),
                block(
                    vec![
                        assign(
                            9,
                            Rvalue::Ref {
                                place: Place {
                                    local: Local(8),
                                    projections: vec![Projection::Deref],
                                },
                                mutable: false,
                            },
                        ),
                        assign(8, Rvalue::Use(copy(6))),
                        assign(10, Rvalue::Aggregate(AggregateKind::Map, vec![])),
                    ],
                    Terminator::Goto { target: BlockId(2) },
                ),
                block(
                    vec![],
                    Terminator::Call {
                        func: Operand::Constant(Constant::FunctionRef(helper_id)),
                        args: vec![copy(9)],
                        destination: local(10),
                        target: BlockId(3),
                        span,
                    },
                ),
                block(
                    vec![],
                    Terminator::SwitchInt {
                        discriminant: Operand::Constant(Constant::Bool(true)),
                        targets: vec![(1, BlockId(4))],
                        otherwise: BlockId(5),
                    },
                ),
                block(
                    vec![assign(8, Rvalue::Use(copy(7)))],
                    Terminator::Goto { target: BlockId(1) },
                ),
                block(vec![assign(0, Rvalue::Use(copy(10)))], Terminator::Return),
            ],
            captures: vec![],
            is_async: false,
            span,
        };
        let mir = Mir {
            functions: HashMap::from([(helper_id, helper), (caller_id, caller)]),
        };
        let mut diagnostics = Diagnostics::new();
        analyze_program(&mir, &table, &mut diagnostics);
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic.message
                == "returned value contains a reference to local storage"),
            "helper must see the unsafe pointee added on the backedge"
        );
    }
    #[test]
    fn projected_helper_reference_preserves_pointee_selector() {
        for borrow_local in [false, true] {
            projected_helper_reference_case(borrow_local);
        }
    }

    fn projected_helper_reference_case(borrow_local: bool) {
        use spark_mir::{
            AggregateKind, BasicBlock, Constant, LocalDecl, Place, Projection, Rvalue, Statement,
        };
        use spark_span::Span;
        use spark_types::{IntWidth, LifetimeId, Ty};
        use std::collections::HashMap;

        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let string = table.intern(Ty::String);
        let reference = table.intern(Ty::Ref {
            inner: int,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let map = table.intern(Ty::Map(string, reference));
        let map_ref = table.intern(Ty::Ref {
            inner: map,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let span = Span::DUMMY;
        let local = |index| Place::from_local(Local(index));
        let copy = |index| Operand::Copy(local(index));
        let assign = |index, rvalue| Statement::Assign {
            place: local(index),
            rvalue,
            span,
        };
        let borrow = |index| Rvalue::Ref {
            place: local(index),
            mutable: false,
        };
        let block = |statements, terminator| BasicBlock {
            statements,
            terminator,
            is_cleanup: false,
        };
        let locals = |types: &[TypeId]| {
            types
                .iter()
                .enumerate()
                .map(|(index, ty)| LocalDecl {
                    local: Local(index as u32),
                    ty: *ty,
                    name: None,
                    is_user_variable: false,
                    mutable: true,
                    span,
                })
                .collect()
        };
        let first = spark_intern::sym("first");
        let pair_id = DefId::new(9003);
        table.set_struct_fields(pair_id, vec![(first, map)]);
        let pair = table.intern(Ty::Struct(pair_id));
        let pair_ref = table.intern(Ty::Ref {
            inner: pair,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let select_id = DefId::new(9000);
        let copy_id = DefId::new(9001);
        let caller_id = DefId::new(9002);
        let select = MirFunction {
            def_id: select_id,
            param_count: 1,
            return_type: map_ref,
            locals: locals(&[map_ref, pair_ref]),
            blocks: vec![block(
                vec![assign(
                    0,
                    Rvalue::Ref {
                        place: Place {
                            local: Local(1),
                            projections: vec![Projection::Deref, Projection::Field(first)],
                        },
                        mutable: false,
                    },
                )],
                Terminator::Return,
            )],
            captures: vec![],
            is_async: false,
            span,
        };
        let copier = MirFunction {
            def_id: copy_id,
            param_count: 1,
            return_type: map,
            locals: locals(&[map, map_ref]),
            blocks: vec![block(
                vec![assign(
                    0,
                    Rvalue::Use(Operand::Copy(Place {
                        local: Local(1),
                        projections: vec![Projection::Deref],
                    })),
                )],
                Terminator::Return,
            )],
            captures: vec![],
            is_async: false,
            span,
        };
        let caller = MirFunction {
            def_id: caller_id,
            param_count: u32::from(!borrow_local),
            return_type: map,
            locals: locals(&[
                map,
                if borrow_local { int } else { reference },
                reference,
                map,
                pair,
                pair_ref,
                map_ref,
            ]),
            blocks: vec![
                block(
                    vec![
                        if borrow_local {
                            assign(1, Rvalue::Use(Operand::Constant(Constant::Int(7))))
                        } else {
                            Statement::Nop
                        },
                        assign(
                            2,
                            if borrow_local {
                                borrow(1)
                            } else {
                                Rvalue::Use(copy(1))
                            },
                        ),
                        assign(
                            3,
                            Rvalue::Aggregate(
                                AggregateKind::Map,
                                vec![
                                    Operand::Constant(Constant::String(spark_intern::sym("one"))),
                                    copy(2),
                                ],
                            ),
                        ),
                        assign(
                            4,
                            Rvalue::Aggregate(
                                AggregateKind::Struct {
                                    def_id: pair_id,
                                    fields: vec![first],
                                },
                                vec![copy(3)],
                            ),
                        ),
                        assign(5, borrow(4)),
                    ],
                    Terminator::Call {
                        func: Operand::Constant(Constant::FunctionRef(select_id)),
                        args: vec![copy(5)],
                        destination: local(6),
                        target: BlockId(1),
                        span,
                    },
                ),
                block(
                    vec![],
                    Terminator::Call {
                        func: Operand::Constant(Constant::FunctionRef(copy_id)),
                        args: vec![copy(6)],
                        destination: local(0),
                        target: BlockId(2),
                        span,
                    },
                ),
                block(vec![], Terminator::Return),
            ],
            captures: vec![],
            is_async: false,
            span,
        };
        let mir = Mir {
            functions: HashMap::from([(select_id, select), (copy_id, copier), (caller_id, caller)]),
        };
        let mut diagnostics = Diagnostics::new();
        analyze_program(&mir, &table, &mut diagnostics);
        assert_eq!(diagnostics.has_errors(), borrow_local, "projected helper Ref must preserve payload lifetime independently of the wrapper address");
    }
}
