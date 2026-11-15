// TAG [SPARK-114]

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
fn successive_stores_at_one_point_reach_the_original_entry_source() {
    use spark_mir::{LocalDecl, MirFunction, Place};
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::{IntWidth, LifetimeId, Ty, TypeTable};

    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let function = MirFunction {
        def_id: DefId::new(9960),
        param_count: 0,
        return_type: int,
        locals: [int, reference, reference, reference]
            .into_iter()
            .enumerate()
            .map(|(index, ty)| LocalDecl {
                local: Local(index as u32),
                ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            })
            .collect(),
        blocks: Vec::new(),
        captures: Vec::new(),
        is_async: false,
        span: Span::DUMMY,
    };
    let point = Point::new(BlockId::ENTRY, 1);
    let origin = Origin::Loan(Point::new(BlockId::ENTRY, 0));
    let mut flow = super::super::Flow::new(&function, &table, None);
    let mut state = super::super::State::default();
    state.set(Local(1), super::super::Value::reference(origin.clone()));
    let first = state.cell(Local(1)).sourced(&Storage::Cell(Local(1)));
    flow.store(&Place::from_local(Local(2)), first, point, &mut state);
    let second = state.cell(Local(2)).sourced(&Storage::Cell(Local(2)));
    flow.store(&Place::from_local(Local(3)), second, point, &mut state);
    let demanded = Atom {
        coordinate: Coordinate {
            storage: Storage::Cell(Local(3)),
            path: Vec::new(),
        },
        origin: origin.clone(),
        capture: None,
    };
    let expected = Atom {
        coordinate: Coordinate {
            storage: Storage::Cell(Local(1)),
            path: Vec::new(),
        },
        origin,
        capture: None,
    };
    assert_eq!(
        flow.transfers[&point].before(&Demand::from_iter([demanded])),
        Demand::from_iter([expected])
    );
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
    let mut transfer = Transfer::at_entry(Demand::from_iter([retained.clone()]));
    transfer.define(&definition(Storage::External(owner)));
    let demand = Demand::from_iter([retained.clone()]);
    assert_eq!(
        transfer.before(&demand),
        demand,
        "replacing the pointer-containing value cannot kill demand in its old pointee cell"
    );
    transfer.define(&definition(Storage::External(pointee)));
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
    let mut transfer = Transfer::at_entry(Demand::from_iter([
        removed.clone(),
        old_cell.clone(),
        sibling.clone(),
    ]));
    transfer.define(&definition(Storage::Logical(Local(1))));
    transfer.define(&definition(Storage::Cell(Local(1))));
    let after = [removed, old_cell, sibling.clone()].into_iter().collect();
    assert_eq!(transfer.before(&after), [sibling].into_iter().collect());
}

#[test]
fn reissuing_a_reference_does_not_kill_an_old_carrier_with_the_same_origin() {
    let fresh = atom(Storage::Logical(Local(1)), Vec::new());
    let retained = atom(Storage::Logical(Local(2)), vec![Slot::Value]);
    assert_eq!(fresh.origin, retained.origin);
    let mut transfer = Transfer::at_entry(Demand::from_iter([fresh.clone(), retained.clone()]));
    transfer.define(&definition(Storage::Logical(Local(1))));
    transfer.define(&definition(Storage::Cell(Local(1))));
    let after = [fresh, retained.clone()].into_iter().collect();
    assert_eq!(transfer.before(&after), [retained].into_iter().collect());
}

#[test]
fn consuming_a_logical_value_preserves_its_addressable_contents() {
    let logical = atom(Storage::Logical(Local(1)), vec![Slot::Tuple(0)]);
    let cell = atom(Storage::Cell(Local(1)), vec![Slot::Tuple(0)]);
    let mut transfer = Transfer::at_entry(Demand::from_iter([logical.clone(), cell.clone()]));
    transfer.define(&definition(Storage::Logical(Local(1))));
    let after = [logical, cell.clone()].into_iter().collect();
    assert_eq!(transfer.before(&after), [cell].into_iter().collect());
}

#[test]
fn copied_payload_depends_on_its_cell_and_the_copy_reads_its_wrapper() {
    let payload = atom(Storage::Logical(Local(3)), vec![Slot::Value]);
    let source = atom(Storage::Cell(Local(1)), vec![Slot::Value]);
    let wrapper = atom(Storage::Logical(Local(2)), Vec::new());
    let mut transfer = Transfer::at_entry(Demand::from_iter([source.clone(), wrapper.clone()]));
    transfer.store(
        &definition(Storage::Logical(Local(3))),
        FxHashMap::from_iter([(
            payload.clone(),
            Frontier::from_iter([Source::Current(source.clone())]),
        )]),
        true,
    );
    transfer.seed(&Frontier::from_iter([Source::Current(wrapper.clone())]));
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
    let mut transfer = Transfer::at_entry(Demand::from_iter([retained.clone()]));
    transfer.define(&Definition {
        storage: Storage::Logical(Local(1)),
        prefix: vec![Slot::Value, Slot::Tail],
    });
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

fn write(transfer: &mut Transfer, destination: &Atom, sources: Frontier, strong: bool) {
    transfer.store(
        &Definition {
            storage: destination.coordinate.storage.clone(),
            prefix: destination.coordinate.path.clone(),
        },
        FxHashMap::from_iter([(destination.clone(), sources)]),
        strong,
    );
}

#[test]
fn weak_same_point_publications_have_complete_entry_rows() {
    let source = atom(Storage::Cell(Local(1)), Vec::new());
    let observed = atom(Storage::Cell(Local(2)), Vec::new());
    let output = atom(Storage::Cell(Local(3)), Vec::new());
    let mut transfer = Transfer::at_entry(Demand::from_iter([source.clone()]));
    write(
        &mut transfer,
        &observed,
        Frontier::from_iter([Source::Current(source.clone())]),
        false,
    );
    write(
        &mut transfer,
        &output,
        Frontier::from_iter([Source::Current(observed)]),
        false,
    );
    assert_eq!(
        transfer.before(&Demand::from_iter([output])),
        Demand::from_iter([source])
    );
}

#[test]
fn historical_entry_sources_survive_a_current_cell_clear() {
    let current = atom(Storage::Cell(Local(1)), Vec::new());
    let history = atom(Storage::Cell(Local(2)), Vec::new());
    let output = atom(Storage::Cell(Local(3)), Vec::new());
    let mut transfer = Transfer::at_entry(Demand::from_iter([current.clone(), history.clone()]));
    transfer.define(&definition(current.coordinate.storage));
    write(
        &mut transfer,
        &output,
        Frontier::from_iter([Source::Current(history.clone())]),
        false,
    );
    assert_eq!(
        transfer.before(&Demand::from_iter([output])),
        Demand::from_iter([history])
    );
}

#[test]
fn frozen_entry_sources_are_terminal_even_after_the_coordinate_is_rewritten() {
    let source = atom(Storage::Cell(Local(1)), Vec::new());
    let replacement = atom(Storage::Cell(Local(2)), Vec::new());
    let output = atom(Storage::Cell(Local(3)), Vec::new());
    let mut transfer = Transfer::at_entry(Demand::from_iter([source.clone(), replacement.clone()]));
    let frozen = transfer
        .resolve(&Frontier::from_iter([Source::Current(source.clone())]))
        .into_iter()
        .map(Source::Entry)
        .collect();
    write(
        &mut transfer,
        &source,
        Frontier::from_iter([Source::Current(replacement)]),
        true,
    );
    write(&mut transfer, &output, frozen, false);
    assert_eq!(
        transfer.before(&Demand::from_iter([output])),
        Demand::from_iter([source])
    );
}

#[test]
fn unsupported_alternatives_cannot_invent_old_destination_identity() {
    let source = atom(Storage::Cell(Local(1)), Vec::new());
    let output = atom(Storage::Cell(Local(2)), Vec::new());
    let absent = Transfer::at_entry(Demand::from_iter([source.clone()]));
    let mut present = absent.clone();
    write(
        &mut present,
        &output,
        Frontier::from_iter([Source::Current(source.clone())]),
        false,
    );
    present.join_alternative(&absent);
    assert_eq!(
        present.before(&Demand::from_iter([output])),
        Demand::from_iter([source])
    );
}

#[test]
fn supported_old_alternatives_retain_their_real_entry_identity() {
    let source = atom(Storage::Cell(Local(1)), Vec::new());
    let output = atom(Storage::Cell(Local(2)), Vec::new());
    let original = Transfer::at_entry(Demand::from_iter([source.clone(), output.clone()]));
    let mut replaced = original.clone();
    write(
        &mut replaced,
        &output,
        Frontier::from_iter([Source::Current(source.clone())]),
        true,
    );
    replaced.join_alternative(&original);
    assert_eq!(
        replaced.before(&Demand::from_iter([output.clone()])),
        Demand::from_iter([source, output])
    );
}

#[test]
fn new_loan_contributions_have_empty_complete_rows_until_the_next_point() {
    let fresh = atom(Storage::Cell(Local(1)), Vec::new());
    let mut transfer = Transfer::default();
    write(&mut transfer, &fresh, Frontier::default(), false);
    assert!(transfer
        .before(&Demand::from_iter([fresh.clone()]))
        .is_empty());
    transfer.seed(&Frontier::from_iter([Source::Current(fresh)]));
    assert!(transfer.before(&Demand::default()).is_empty());
}

#[test]
fn a_frozen_call_rhs_survives_argument_consumption() {
    let argument = atom(Storage::Logical(Local(1)), Vec::new());
    let output = atom(Storage::Cell(Local(2)), Vec::new());
    let mut transfer = Transfer::at_entry(Demand::from_iter([argument.clone()]));
    let value = super::super::Value::reference(argument.origin.clone())
        .sourced(&argument.coordinate.storage);
    let frozen = transfer.freeze(value);
    transfer.define(&definition(argument.coordinate.storage.clone()));
    write(&mut transfer, &output, frozen.all_sources(), false);
    assert_eq!(
        transfer.before(&Demand::from_iter([output])),
        Demand::from_iter([argument])
    );
}

#[test]
fn observation_seeds_are_resolved_before_a_later_same_point_overwrite() {
    let source = atom(Storage::Cell(Local(1)), Vec::new());
    let temporary = atom(Storage::Cell(Local(2)), Vec::new());
    let replacement = atom(Storage::Cell(Local(3)), Vec::new());
    let mut transfer = Transfer::at_entry(Demand::from_iter([source.clone(), replacement.clone()]));
    write(
        &mut transfer,
        &temporary,
        Frontier::from_iter([Source::Current(source.clone())]),
        false,
    );
    transfer.seed(&Frontier::from_iter([Source::Current(temporary.clone())]));
    write(
        &mut transfer,
        &temporary,
        Frontier::from_iter([Source::Current(replacement)]),
        true,
    );
    assert_eq!(
        transfer.before(&Demand::default()),
        Demand::from_iter([source])
    );
}

#[test]
fn cyclic_publication_relations_keep_finite_entry_sources() {
    let input = atom(Storage::Cell(Local(1)), Vec::new());
    let relay = atom(Storage::Cell(Local(2)), Vec::new());
    let mut transfer = Transfer::at_entry(Demand::from_iter([input.clone()]));
    for _ in 0..4 {
        write(
            &mut transfer,
            &relay,
            Frontier::from_iter([Source::Current(input.clone())]),
            false,
        );
        write(
            &mut transfer,
            &input,
            Frontier::from_iter([Source::Current(relay.clone())]),
            false,
        );
    }
    assert_eq!(
        transfer.before(&Demand::from_iter([relay])),
        Demand::from_iter([input])
    );
    assert_eq!(transfer.rows.len(), 2);
}

#[test]
fn call_transaction_preserves_moved_argument_sources_in_return_and_queue() {
    use super::super::{
        channels, effects::CallOutcome, tasks::proofs::CallProof, Flow, State, Value,
    };
    use spark_mir::{LocalDecl, MirFunction, Operand, Place};
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::{IntWidth, LifetimeId, Ty, TypeTable};

    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let channel = table.intern(Ty::Channel { inner: reference });
    let function = MirFunction {
        def_id: DefId::new(9961),
        param_count: 0,
        return_type: int,
        locals: [int, reference, reference]
            .into_iter()
            .enumerate()
            .map(|(index, ty)| LocalDecl {
                local: Local(index as u32),
                ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            })
            .collect(),
        blocks: Vec::new(),
        captures: Vec::new(),
        is_async: false,
        span: Span::DUMMY,
    };
    let point = Point::new(BlockId::ENTRY, 1);
    let source = atom(Storage::Logical(Local(1)), Vec::new());
    let queue = channels::Target::Allocation {
        ty: channel,
        function: function.def_id,
        point,
        constructor: None,
    };
    let mut state = State::default();
    state.set(Local(1), Value::reference(source.origin.clone()));
    let value = state.value(Local(1)).sourced(&source.coordinate.storage);
    let mut flow = Flow::new(&function, &table, None);
    flow.ensure_transfer(point, &state);
    let outcome = CallOutcome {
        returned: value.clone(),
        tasks: Vec::new(),
        completed_task: None,
        clobbered_copies: Vec::new(),
        future_proof: CallProof::default(),
        place_writes: Vec::new(),
        memory: Vec::new(),
        queue_contents: Vec::new(),
        queues: vec![channels::Write {
            target: queue.clone(),
            ty: reference,
            value,
        }],
    };
    flow.apply_call_outcomes(
        &[Operand::Move(Place::from_local(Local(1)))],
        &Place::from_local(Local(2)),
        vec![outcome],
        point,
        &mut state,
    );
    assert!(!state.live.contains(&Local(1)));
    assert_eq!(
        state.value(Local(2)).origins(),
        FxHashSet::from_iter([source.origin.clone()])
    );
    let queued = &state.queues[&(queue.clone(), reference)];
    assert_eq!(
        queued.origins(),
        FxHashSet::from_iter([source.origin.clone()])
    );
    assert!(queued.sources.is_empty());
    let returned = Atom {
        coordinate: Coordinate {
            storage: Storage::Logical(Local(2)),
            path: Vec::new(),
        },
        ..source.clone()
    };
    let published = Atom {
        coordinate: Coordinate {
            storage: Storage::Queue(queue, reference),
            path: Vec::new(),
        },
        ..source.clone()
    };
    assert_eq!(
        flow.transfers[&point].before(&Demand::from_iter([returned, published])),
        Demand::from_iter([source])
    );
}
