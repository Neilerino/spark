//! Backward demand over individual reaching reference contributions.

use rustc_hash::{FxHashMap, FxHashSet};
use spark_mir::Local;

use super::{aliases::Port, Origin, Path, Slot};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Storage {
    Logical(Local),
    Cell(Local),
    External(Port),
    Queue(super::channels::Target, spark_types::TypeId),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Coordinate {
    pub storage: Storage,
    pub path: Path,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Atom {
    pub coordinate: Coordinate,
    pub origin: Origin,
    pub capture: Option<super::value::ReferenceCapture>,
}

pub(super) type Demand = FxHashSet<Atom>;
pub(super) type Sources = FxHashMap<super::value::Contribution, Demand>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Definition {
    pub storage: Storage,
    pub prefix: Path,
}

impl Definition {
    fn includes(&self, atom: &Atom) -> bool {
        self.storage == atom.coordinate.storage
            && !self.prefix.contains(&Slot::Tail)
            && atom.coordinate.path.starts_with(&self.prefix)
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct Transfer {
    pub definitions: Vec<Definition>,
    pub dependencies: FxHashMap<Atom, Demand>,
    pub seeds: Demand,
}

impl Transfer {
    pub fn join_alternative(&mut self, other: &Self) {
        self.definitions
            .retain(|definition| other.definitions.contains(definition));
        self.seeds.extend(other.seeds.iter().cloned());
        for (destination, sources) in &other.dependencies {
            self.dependencies
                .entry(destination.clone())
                .or_default()
                .extend(sources.iter().cloned());
        }
    }

    pub fn before(&self, after: &Demand) -> Demand {
        let mut before = self.seeds.clone();
        for atom in after {
            if let Some(sources) = self.dependencies.get(atom) {
                before.extend(sources.iter().cloned());
            }
            if !self
                .definitions
                .iter()
                .any(|definition| definition.includes(atom))
            {
                before.insert(atom.clone());
            }
        }
        before
    }
}

impl super::Flow<'_> {
    pub(super) fn record_consumed(&mut self, point: super::Point, local: Local) {
        self.transfers
            .entry(point)
            .or_default()
            .definitions
            .push(Definition {
                storage: Storage::Logical(local),
                prefix: Vec::new(),
            });
    }

    pub(super) fn record_store(
        &mut self,
        point: super::Point,
        place: &spark_mir::Place,
        value: &super::Value,
    ) {
        let root = self.function.locals[place.local.index()].ty;
        let (prefix, _) = self.projection_path(root, &place.projections);
        for storage in [Storage::Logical(place.local), Storage::Cell(place.local)] {
            self.record_path_store(
                point,
                Coordinate {
                    storage,
                    path: prefix.clone(),
                },
                value,
                place.projections.is_empty(),
            );
        }
    }

    pub(super) fn record_path_store(
        &mut self,
        point: super::Point,
        destination: Coordinate,
        value: &super::Value,
        strong: bool,
    ) {
        let Coordinate {
            storage,
            path: prefix,
        } = destination;
        let root = match &storage {
            Storage::Logical(local) | Storage::Cell(local) => {
                self.function.locals[local.index()].ty
            }
            Storage::External(port) => port.pointee,
            Storage::Queue(_, ty) => *ty,
        };
        let transfer = self.transfers.entry(point).or_default();
        if strong {
            transfer.definitions.push(Definition {
                storage: storage.clone(),
                prefix: prefix.clone(),
            });
        }
        for ((path, origin, capture), sources) in &value.sources {
            let path =
                super::value::normalize(root, [prefix.clone(), path.clone()].concat(), self.table);
            let destination = Atom {
                coordinate: Coordinate {
                    storage: storage.clone(),
                    path,
                },
                origin: origin.clone(),
                capture: *capture,
            };
            transfer
                .dependencies
                .entry(destination)
                .or_default()
                .extend(sources.iter().cloned());
        }
    }

    pub(super) fn carrier_regions(
        &mut self,
        facts: &crate::facts::BorrowFacts,
    ) -> Vec<crate::regions::Region> {
        use super::Point;
        let mut exits = FxHashMap::default();
        self.transfers.clear();
        for index in 0..self.entries.len() {
            if let Some(mut state) = self.entries[index].clone() {
                let block = spark_mir::BlockId(index as u32);
                self.transfer_block(block, &mut state);
                exits.insert(block, state.atoms());
                if matches!(
                    self.function.blocks[index].terminator,
                    spark_mir::Terminator::Return
                ) {
                    let point =
                        Point::new(block, self.function.blocks[index].statements.len() as u32);
                    let returned = state
                        .value(Local::RETURN_PLACE)
                        .sourced(&Storage::Logical(Local::RETURN_PLACE));
                    let returned = self.reachable_queue_payloads(&returned, &state);
                    let external: Demand = state
                        .external
                        .iter()
                        .flat_map(|(local, value)| {
                            self.reachable_queue_payloads(
                                &value.clone().sourced(&Storage::External(local.clone())),
                                &state,
                            )
                            .sources
                            .into_values()
                            .flatten()
                        })
                        .collect();
                    self.transfers
                        .entry(point)
                        .or_default()
                        .seeds
                        .extend(returned.sources.into_values().flatten().chain(external));
                }
            }
        }
        let (before, after) = solve(&self.transfers, facts, &exits);
        let mut regions: Vec<_> = facts
            .loans
            .iter()
            .map(|loan| crate::regions::Region {
                loan: loan.id,
                points: FxHashSet::default(),
            })
            .collect();
        for index in 0..self.entries.len() {
            let Some(mut state) = self.entries[index].clone() else {
                continue;
            };
            let block = spark_mir::BlockId(index as u32);
            for statement_index in 0..=self.function.blocks[index].statements.len() {
                let point = Point::new(block, statement_index as u32);
                let mut active = super::Origins::default();
                for atom in before.get(&point).into_iter().flatten() {
                    if state.contains(atom) {
                        active.extend(state.support(atom));
                    }
                }
                if let Some(statement) = self.function.blocks[index].statements.get(statement_index)
                {
                    self.transfer_statement(statement, point, &mut state);
                } else {
                    self.transfer_terminator(block, &mut state);
                }
                for atom in after.get(&point).into_iter().flatten() {
                    if state.contains(atom) {
                        active.extend(state.support(atom));
                    }
                }
                for access in self.accesses.get(&point).into_iter().flatten() {
                    active.insert(access.origin.clone());
                }
                for (loan, region) in facts.loans.iter().zip(&mut regions) {
                    if loan.issued_at == point
                        || active.iter().any(|origin| {
                            matches!(origin, Origin::Loan(site) if *site == loan.issued_at)
                                || matches!(origin, Origin::Frame { function, point: site } if *function == self.function.def_id && *site == loan.issued_at)
                        })
                    {
                        region.points.insert(point);
                    }
                }
            }
        }
        regions
    }
}

type PointDemands = FxHashMap<super::Point, Demand>;

fn solve(
    transfers: &FxHashMap<super::Point, Transfer>,
    facts: &crate::facts::BorrowFacts,
    exits: &FxHashMap<spark_mir::BlockId, Demand>,
) -> (PointDemands, PointDemands) {
    use super::Point;
    let mut successors: FxHashMap<Point, Vec<Point>> = FxHashMap::default();
    let mut predecessors: FxHashMap<Point, Vec<Point>> = FxHashMap::default();
    for (from, to) in &facts.cfg_edges {
        if transfers.contains_key(from) && transfers.contains_key(to) {
            successors.entry(*from).or_default().push(*to);
            predecessors.entry(*to).or_default().push(*from);
        }
    }
    let mut before: FxHashMap<Point, Demand> = FxHashMap::default();
    let mut after: FxHashMap<Point, Demand> = FxHashMap::default();
    let mut pending: std::collections::VecDeque<_> = transfers.keys().copied().collect();
    let mut queued: FxHashSet<_> = pending.iter().copied().collect();
    while let Some(point) = pending.pop_front() {
        queued.remove(&point);
        let mut outgoing = Demand::default();
        for next in successors.get(&point).into_iter().flatten() {
            for atom in before.get(next).into_iter().flatten() {
                if next.index != 0 || exits[&point.block].contains(atom) {
                    outgoing.insert(atom.clone());
                }
            }
        }
        let incoming = transfers[&point].before(&outgoing);
        after.insert(point, outgoing);
        if before.get(&point) != Some(&incoming) {
            before.insert(point, incoming);
            for previous in predecessors.get(&point).into_iter().flatten() {
                if queued.insert(*previous) {
                    pending.push_back(*previous);
                }
            }
        }
    }
    (before, after)
}

impl super::State {
    fn atoms(&self) -> Demand {
        self.values
            .iter()
            .map(|(local, value)| (Storage::Logical(*local), value))
            .chain(
                self.cells
                    .iter()
                    .map(|(local, value)| (Storage::Cell(*local), value)),
            )
            .chain(
                self.external
                    .iter()
                    .map(|(port, value)| (Storage::External(port.clone()), value)),
            )
            .chain(
                self.queues
                    .iter()
                    .map(|((target, ty), value)| (Storage::Queue(target.clone(), *ty), value)),
            )
            .flat_map(|(storage, value)| {
                value
                    .clone()
                    .sourced(&storage)
                    .sources
                    .into_values()
                    .flatten()
            })
            .collect()
    }

    fn stored_value(&self, storage: &Storage) -> Option<&super::Value> {
        match storage {
            Storage::Logical(local) => self.values.get(local),
            Storage::Cell(local) => self.cells.get(local),
            Storage::External(port) => self.external.get(port),
            Storage::Queue(target, ty) => self.queues.get(&(target.clone(), *ty)),
        }
    }

    fn support(&self, atom: &Atom) -> super::Origins {
        let mut result = super::Origins::from_iter([atom.origin.clone()]);
        if let Some(parents) = self
            .stored_value(&atom.coordinate.storage)
            .and_then(|value| {
                value.leaves.get(&(
                    atom.coordinate.path.clone(),
                    atom.origin.clone(),
                    atom.capture,
                ))
            })
        {
            result.extend(parents.parents.iter().cloned());
        }
        result
    }

    fn contains(&self, atom: &Atom) -> bool {
        self.stored_value(&atom.coordinate.storage)
            .is_some_and(|value| {
                value.leaves.contains_key(&(
                    atom.coordinate.path.clone(),
                    atom.origin.clone(),
                    atom.capture,
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::Point;
    use spark_mir::BlockId;

    fn atom(storage: Storage, path: Path) -> Atom {
        Atom {
            coordinate: Coordinate { storage, path },
            origin: Origin::Loan(Point::new(BlockId(0), 0)),
            capture: None,
        }
    }

    fn definition(storage: Storage) -> Definition {
        Definition {
            storage,
            prefix: Vec::new(),
        }
    }

    #[test]
    fn replacing_external_owner_preserves_independent_pointee_demand() {
        let owner = Port::root(Local(1), spark_types::TypeId::ERROR);
        let pointee = Port {
            kind: crate::provenance::aliases::StorageKind::Reference,
            channel_capture: None,
            queue: None,
            capture: None,
            input: Local(1),
            selection: vec![Slot::Deref, Slot::Field(spark_intern::sym("target"))],
            pointee: spark_types::TypeId::ERROR,
        };
        let retained = atom(Storage::External(pointee.clone()), vec![Slot::Value]);
        let mut transfer = Transfer {
            definitions: vec![definition(Storage::External(owner))],
            ..Transfer::default()
        };
        let demand = Demand::from_iter([retained.clone()]);
        assert_eq!(
            transfer.before(&demand),
            demand,
            "replacing the pointer-containing value cannot kill demand in its old pointee cell"
        );
        transfer
            .definitions
            .push(definition(Storage::External(pointee)));
        assert!(
            transfer.before(&Demand::from_iter([retained])).is_empty(),
            "clearing the actual pointee cell ends that demand"
        );
    }

    #[test]
    fn clearing_a_carrier_preserves_demand_for_its_sibling_copy() {
        let removed = atom(Storage::Logical(Local(1)), vec![Slot::Value]);
        let old_cell = atom(Storage::Cell(Local(1)), vec![Slot::Value]);
        let sibling = atom(Storage::Logical(Local(2)), vec![Slot::Value]);
        let transfer = Transfer {
            definitions: vec![
                definition(Storage::Logical(Local(1))),
                definition(Storage::Cell(Local(1))),
            ],
            ..Transfer::default()
        };
        let after = [removed, old_cell, sibling.clone()].into_iter().collect();
        assert_eq!(transfer.before(&after), [sibling].into_iter().collect());
    }

    #[test]
    fn reissuing_a_reference_does_not_kill_an_old_carrier_with_the_same_origin() {
        let fresh = atom(Storage::Logical(Local(1)), Vec::new());
        let retained = atom(Storage::Logical(Local(2)), vec![Slot::Value]);
        assert_eq!(fresh.origin, retained.origin);
        let transfer = Transfer {
            definitions: vec![
                definition(Storage::Logical(Local(1))),
                definition(Storage::Cell(Local(1))),
            ],
            ..Transfer::default()
        };
        let after = [fresh, retained.clone()].into_iter().collect();
        assert_eq!(transfer.before(&after), [retained].into_iter().collect());
    }

    #[test]
    fn consuming_a_logical_value_preserves_its_addressable_contents() {
        let logical = atom(Storage::Logical(Local(1)), vec![Slot::Tuple(0)]);
        let cell = atom(Storage::Cell(Local(1)), vec![Slot::Tuple(0)]);
        let transfer = Transfer {
            definitions: vec![definition(Storage::Logical(Local(1)))],
            ..Transfer::default()
        };
        let after = [logical, cell.clone()].into_iter().collect();
        assert_eq!(transfer.before(&after), [cell].into_iter().collect());
    }

    #[test]
    fn copied_payload_depends_on_its_cell_and_the_copy_reads_its_wrapper() {
        let payload = atom(Storage::Logical(Local(3)), vec![Slot::Value]);
        let source = atom(Storage::Cell(Local(1)), vec![Slot::Value]);
        let wrapper = atom(Storage::Logical(Local(2)), Vec::new());
        let transfer = Transfer {
            definitions: vec![
                definition(Storage::Logical(Local(3))),
                definition(Storage::Cell(Local(3))),
            ],
            dependencies: FxHashMap::from_iter([(
                payload.clone(),
                [source.clone()].into_iter().collect(),
            )]),
            seeds: [wrapper.clone()].into_iter().collect(),
        };
        assert_eq!(
            transfer.before(&Demand::default()),
            [wrapper.clone()].into_iter().collect()
        );
        assert_eq!(
            transfer.before(&[payload].into_iter().collect()),
            [source, wrapper].into_iter().collect()
        );
    }

    #[test]
    fn widened_tail_does_not_authorize_a_strong_subpath_kill() {
        let retained = atom(Storage::Logical(Local(1)), vec![Slot::Value, Slot::Tail]);
        let transfer = Transfer {
            definitions: vec![Definition {
                storage: Storage::Logical(Local(1)),
                prefix: vec![Slot::Value, Slot::Tail],
            }],
            ..Transfer::default()
        };
        let after = [retained].into_iter().collect();
        assert_eq!(transfer.before(&after), after);
    }
    #[test]
    fn projected_helper_reborrows_keep_each_returned_parent_separate() {
        use spark_mir::{
            AggregateKind, BasicBlock, Constant, LocalDecl, Mir, MirFunction, Operand, Place,
            Projection, Rvalue, Statement, Terminator,
        };
        use spark_resolve::DefId;
        use spark_span::Span;
        use spark_types::{IntWidth, LifetimeId, Ty, TypeId, TypeTable};
        use std::collections::HashMap;
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let record_id = DefId::new(9100);
        let field = spark_intern::sym("value");
        table.set_struct_fields(record_id, vec![(field, int)]);
        let record = table.intern(Ty::Struct(record_id));
        let reference = |inner| Ty::Ref {
            inner,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        };
        let record_ref = table.intern(reference(record));
        let int_ref = table.intern(reference(int));
        let pair = table.intern(Ty::Tuple(vec![int_ref, int_ref]));
        let span = Span::DUMMY;
        let local = |index| Place::from_local(Local(index));
        let copy = |index| Operand::Copy(local(index));
        let assign = |index, rvalue| Statement::Assign {
            place: local(index),
            rvalue,
            span,
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
        let helper_id = DefId::new(9101);
        let caller_id = DefId::new(9102);
        let helper = MirFunction {
            def_id: helper_id,
            param_count: 2,
            return_type: pair,
            locals: locals(&[pair, record_ref, record_ref, int_ref, int_ref]),
            blocks: vec![block(
                vec![
                    assign(
                        3,
                        Rvalue::Ref {
                            place: Place {
                                local: Local(1),
                                projections: vec![Projection::Deref, Projection::Field(field)],
                            },
                            mutable: false,
                        },
                    ),
                    assign(
                        4,
                        Rvalue::Ref {
                            place: Place {
                                local: Local(2),
                                projections: vec![Projection::Deref, Projection::Field(field)],
                            },
                            mutable: false,
                        },
                    ),
                    assign(
                        0,
                        Rvalue::Aggregate(AggregateKind::Tuple, vec![copy(3), copy(4)]),
                    ),
                ],
                Terminator::Return,
            )],
            captures: vec![],
            is_async: false,
            span,
        };
        let value = |value| {
            Rvalue::Aggregate(
                AggregateKind::Struct {
                    def_id: record_id,
                    fields: vec![field],
                },
                vec![Operand::Constant(Constant::Int(value))],
            )
        };
        let caller = MirFunction {
            def_id: caller_id,
            param_count: 0,
            return_type: int,
            locals: locals(&[int, record, record, record_ref, record_ref, pair, int_ref]),
            blocks: vec![
                block(
                    vec![
                        assign(1, value(1)),
                        assign(2, value(2)),
                        assign(
                            3,
                            Rvalue::Ref {
                                place: local(1),
                                mutable: false,
                            },
                        ),
                        assign(
                            4,
                            Rvalue::Ref {
                                place: local(2),
                                mutable: false,
                            },
                        ),
                    ],
                    Terminator::Call {
                        func: Operand::Constant(Constant::FunctionRef(helper_id)),
                        args: vec![copy(3), copy(4)],
                        destination: local(5),
                        target: BlockId(1),
                        span,
                    },
                ),
                block(
                    vec![
                        assign(
                            6,
                            Rvalue::Use(Operand::Copy(Place {
                                local: Local(5),
                                projections: vec![Projection::Field(spark_intern::sym("0"))],
                            })),
                        ),
                        assign(2, value(9)),
                        assign(
                            0,
                            Rvalue::Use(Operand::Copy(Place {
                                local: Local(6),
                                projections: vec![Projection::Deref],
                            })),
                        ),
                    ],
                    Terminator::Return,
                ),
            ],
            captures: vec![],
            is_async: false,
            span,
        };
        let facts = crate::facts::extract_facts(&caller);
        let mir = Mir {
            functions: HashMap::from([(helper_id, helper), (caller_id, caller)]),
        };
        let mut diagnostics = spark_diagnostics::Diagnostics::new();
        let regions = super::super::effects::analyze_program(&mir, &table, None, &mut diagnostics);
        assert!(!diagnostics.has_errors());
        let read = Point::new(BlockId(1), 2);
        let replace_other = Point::new(BlockId(1), 1);
        assert!(
            regions[&caller_id]
                .iter()
                .all(|context| context[0].points.contains(&read)),
            "selected field must protect its actual source"
        );
        assert!(
            regions[&caller_id]
                .iter()
                .all(|context| !context[1].points.contains(&read)),
            "another field sharing the synthetic call origin must not extend this parent"
        );
        assert!(
            regions[&caller_id]
                .iter()
                .all(|context| !context[1].points.contains(&replace_other)),
            "unused field must not prevent replacing its source"
        );
        assert_eq!(facts.loans.len(), 2);
    }
    #[test]
    fn copied_opaque_payload_keeps_parent_region_live() {
        opaque_payload_region_case(false);
    }

    #[test]
    fn opaque_helper_payload_keeps_caller_parent_region_live() {
        opaque_payload_region_case(true);
    }

    fn opaque_payload_region_case(through_helper: bool) {
        use spark_mir::{
            AggregateKind, BasicBlock, Constant, LocalDecl, Mir, MirFunction, Operand, Place,
            Projection, Rvalue, Statement, Terminator,
        };
        use spark_resolve::DefId;
        use spark_span::Span;
        use spark_types::{IntWidth, LifetimeId, Ty, TypeId, TypeTable};
        use std::collections::HashMap;
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(IntWidth::I64));
        let list = table.intern(Ty::List(int));
        let reference = |inner| Ty::Ref {
            inner,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        };
        let list_ref = table.intern(reference(list));
        let carrier = table.intern(Ty::List(list_ref));
        let carrier_ref = table.intern(reference(carrier));
        let callback = table.intern(Ty::Function {
            params: vec![list_ref],
            ret: carrier_ref,
            is_async: false,
        });
        let span = Span::DUMMY;
        let local = |index| Place::from_local(Local(index));
        let copy = |index| Operand::Copy(local(index));
        let assign = |index, rvalue| Statement::Assign {
            place: local(index),
            rvalue,
            span,
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
        let invoke_id = DefId::new(9301);
        let invoke = MirFunction {
            def_id: invoke_id,
            param_count: 2,
            return_type: carrier_ref,
            locals: locals(&[carrier_ref, callback, list_ref]),
            blocks: vec![
                block(
                    vec![],
                    Terminator::Call {
                        func: copy(1),
                        args: vec![copy(2)],
                        destination: local(0),
                        target: BlockId(1),
                        span,
                    },
                ),
                block(vec![], Terminator::Return),
            ],
            captures: vec![],
            is_async: false,
            span,
        };
        let caller_id = DefId::new(9300);
        let caller = MirFunction {
            def_id: caller_id,
            param_count: 0,
            return_type: int,
            locals: locals(&[
                int,
                callback,
                list,
                list_ref,
                list_ref,
                carrier_ref,
                carrier,
                int,
                list_ref,
            ]),
            blocks: vec![
                block(
                    vec![
                        assign(
                            1,
                            Rvalue::Use(Operand::Constant(Constant::FunctionRef(DefId::new(9302)))),
                        ),
                        assign(
                            2,
                            Rvalue::Aggregate(
                                AggregateKind::List,
                                vec![Operand::Constant(Constant::Int(1))],
                            ),
                        ),
                        assign(
                            3,
                            Rvalue::Ref {
                                place: local(2),
                                mutable: false,
                            },
                        ),
                        assign(
                            4,
                            Rvalue::Ref {
                                place: Place {
                                    local: Local(3),
                                    projections: vec![Projection::Deref],
                                },
                                mutable: false,
                            },
                        ),
                        assign(7, Rvalue::Use(Operand::Constant(Constant::Int(0)))),
                    ],
                    Terminator::Call {
                        func: if through_helper {
                            Operand::Constant(Constant::FunctionRef(invoke_id))
                        } else {
                            copy(1)
                        },
                        args: if through_helper {
                            vec![copy(1), copy(4)]
                        } else {
                            vec![copy(4)]
                        },
                        destination: local(5),
                        target: BlockId(1),
                        span,
                    },
                ),
                block(
                    vec![
                        assign(
                            6,
                            Rvalue::Use(Operand::Copy(Place {
                                local: Local(5),
                                projections: vec![Projection::Deref],
                            })),
                        ),
                        Statement::Nop,
                        assign(
                            8,
                            Rvalue::Use(Operand::Copy(Place {
                                local: Local(6),
                                projections: vec![Projection::Index(Local(7))],
                            })),
                        ),
                        assign(
                            0,
                            Rvalue::Use(Operand::Copy(Place {
                                local: Local(8),
                                projections: vec![Projection::Deref, Projection::Index(Local(7))],
                            })),
                        ),
                    ],
                    Terminator::Return,
                ),
            ],
            captures: vec![],
            is_async: false,
            span,
        };
        let mir = Mir {
            functions: HashMap::from([(caller_id, caller), (invoke_id, invoke)]),
        };
        let mut diagnostics = spark_diagnostics::Diagnostics::new();
        let regions = super::super::effects::analyze_program(&mir, &table, None, &mut diagnostics);
        assert!(!diagnostics.has_errors());
        assert!(
            regions[&caller_id]
                .iter()
                .all(|context| context[0].points.contains(&Point::new(BlockId(1), 1))),
            "source parent must stay live between opaque payload copy and its later read"
        );
    }
}
