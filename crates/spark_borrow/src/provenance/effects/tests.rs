use super::*;

#[test]
fn helper_binding_tracks_endpoint_growth_across_backedges() {
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
    analyze_program(&mir, &table, None, &mut diagnostics);
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
    analyze_program(&mir, &table, None, &mut diagnostics);
    assert_eq!(
        diagnostics.has_errors(),
        borrow_local,
        "projected helper Ref must preserve payload lifetime independently of the wrapper address"
    );
}

#[test]
fn projected_helper_results_preserve_incoming_target() {
    projected_helper_result_case(false, HelperUse::Return);
}

#[test]
fn projected_helper_results_reject_local_target() {
    projected_helper_result_case(true, HelperUse::Return);
}

enum HelperUse {
    Return,
    ReadAfterDrop,
    ForwardReturn,
}

fn projected_helper_result_case(local_result: bool, usage: HelperUse) {
    let read_after_drop = matches!(usage, HelperUse::ReadAfterDrop);
    let forwarded = matches!(usage, HelperUse::ForwardReturn);
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
    let field_type = if read_after_drop {
        table.intern(Ty::String)
    } else {
        int
    };
    let record_id = DefId::new(9100);
    let field = spark_intern::sym("value");
    table.set_struct_fields(record_id, vec![(field, field_type)]);
    let record = table.intern(Ty::Struct(record_id));
    let reference = |inner| Ty::Ref {
        inner,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    };
    let record_ref = table.intern(reference(record));
    let int_ref = table.intern(reference(field_type));
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
            vec![Operand::Constant(if read_after_drop {
                Constant::String(spark_intern::sym("owned"))
            } else {
                Constant::Int(value)
            })],
        )
    };
    let forward_id = DefId::new(9103);
    let forwarder = MirFunction {
        def_id: forward_id,
        param_count: 2,
        return_type: pair,
        locals: locals(&[pair, record_ref, record_ref]),
        blocks: vec![
            block(
                vec![],
                Terminator::Call {
                    func: Operand::Constant(Constant::FunctionRef(helper_id)),
                    args: vec![copy(1), copy(2)],
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
    let result_type = if read_after_drop { field_type } else { int_ref };
    let caller = MirFunction {
        def_id: caller_id,
        param_count: 1,
        return_type: result_type,
        locals: locals(&[
            result_type,
            record_ref,
            record,
            record_ref,
            record_ref,
            pair,
            int_ref,
        ]),
        blocks: vec![
            block(
                vec![
                    assign(2, value(2)),
                    assign(
                        3,
                        Rvalue::Ref {
                            place: local(2),
                            mutable: false,
                        },
                    ),
                    assign(4, Rvalue::Use(copy(1))),
                ],
                Terminator::Call {
                    func: Operand::Constant(Constant::FunctionRef(if forwarded {
                        forward_id
                    } else {
                        helper_id
                    })),
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
                            projections: vec![Projection::Field(spark_intern::sym(
                                if local_result { "0" } else { "1" },
                            ))],
                        })),
                    ),
                    if read_after_drop {
                        Statement::Drop {
                            place: local(2),
                            span,
                        }
                    } else {
                        Statement::Nop
                    },
                    assign(
                        0,
                        Rvalue::Use(Operand::Copy(Place {
                            local: Local(6),
                            projections: if read_after_drop {
                                vec![Projection::Deref]
                            } else {
                                vec![]
                            },
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
        functions: HashMap::from([
            (helper_id, helper),
            (forward_id, forwarder),
            (caller_id, caller),
        ]),
    };
    let mut diagnostics = spark_diagnostics::Diagnostics::new();
    analyze_program(&mir, &table, None, &mut diagnostics);
    assert_eq!(
        diagnostics.has_errors(),
        local_result,
        "only the selected local reference may cause an escape diagnostic"
    );
}

#[test]
fn projected_helper_read_ignores_dropped_unselected_owner() {
    projected_helper_result_case(false, HelperUse::ReadAfterDrop);
}

#[test]
fn projected_helper_read_rejects_dropped_selected_owner() {
    projected_helper_result_case(true, HelperUse::ReadAfterDrop);
}

#[test]
fn projected_helper_payload_preserves_incoming_target() {
    projected_helper_payload_case(false);
}

#[test]
fn projected_helper_payload_rejects_local_target() {
    projected_helper_payload_case(true);
}

fn projected_helper_payload_case(local_result: bool) {
    use spark_mir::{
        AggregateKind, BasicBlock, Constant, LocalDecl, Place, Projection, Rvalue, Statement,
    };
    use spark_span::Span;
    use spark_types::{IntWidth, LifetimeId, Ty};
    use std::collections::HashMap;

    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let string = table.intern(Ty::String);
    let reference = |inner| Ty::Ref {
        inner,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    };
    let int_ref = table.intern(reference(int));
    let map = table.intern(Ty::Map(string, int_ref));
    let map_ref = table.intern(reference(map));
    let record_id = DefId::new(9400);
    let field = spark_intern::sym("value");
    table.set_struct_fields(record_id, vec![(field, map)]);
    let record = table.intern(Ty::Struct(record_id));
    let record_ref = table.intern(reference(record));
    let pair = table.intern(Ty::Tuple(vec![map_ref, map_ref]));
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
    let select_id = DefId::new(9401);
    let copy_id = DefId::new(9402);
    let caller_id = DefId::new(9403);
    let select = MirFunction {
        def_id: select_id,
        param_count: 2,
        return_type: pair,
        locals: locals(&[pair, record_ref, record_ref, map_ref, map_ref]),
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
    let map_value = |index| {
        Rvalue::Aggregate(
            AggregateKind::Map,
            vec![
                Operand::Constant(Constant::String(spark_intern::sym("one"))),
                copy(index),
            ],
        )
    };
    let record_value = |index| {
        Rvalue::Aggregate(
            AggregateKind::Struct {
                def_id: record_id,
                fields: vec![field],
            },
            vec![copy(index)],
        )
    };
    let caller =
        MirFunction {
            def_id: caller_id,
            param_count: 1,
            return_type: map,
            locals: locals(&[
                map, int_ref, int, int_ref, map, map, record, record, record_ref, record_ref, pair,
                map_ref,
            ]),
            blocks: vec![
                block(
                    vec![
                        assign(2, Rvalue::Use(Operand::Constant(Constant::Int(7)))),
                        assign(3, borrow(2)),
                        assign(4, map_value(3)),
                        assign(5, map_value(1)),
                        assign(6, record_value(4)),
                        assign(7, record_value(5)),
                        assign(8, borrow(6)),
                        assign(9, borrow(7)),
                    ],
                    Terminator::Call {
                        func: Operand::Constant(Constant::FunctionRef(select_id)),
                        args: vec![copy(8), copy(9)],
                        destination: local(10),
                        target: BlockId(1),
                        span,
                    },
                ),
                block(
                    vec![assign(
                        11,
                        Rvalue::Use(Operand::Copy(Place {
                            local: Local(10),
                            projections: vec![Projection::Field(spark_intern::sym(
                                if local_result { "0" } else { "1" },
                            ))],
                        })),
                    )],
                    Terminator::Call {
                        func: Operand::Constant(Constant::FunctionRef(copy_id)),
                        args: vec![copy(11)],
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
    analyze_program(&mir, &table, None, &mut diagnostics);
    assert_eq!(
        diagnostics.has_errors(),
        local_result,
        "copying a selected payload must retain only that payload's borrowed references"
    );
}

#[test]
fn projected_forwarder_preserves_incoming_target() {
    projected_helper_result_case(false, HelperUse::ForwardReturn);
}

#[test]
fn projected_forwarder_rejects_local_target() {
    projected_helper_result_case(true, HelperUse::ForwardReturn);
}

#[test]
fn projected_opaque_result_retains_deferred_capture_dependencies() {
    use spark_mir::{
        AggregateKind, BasicBlock, Constant, LocalDecl, Place, Projection, Rvalue, Statement,
    };
    use spark_span::Span;
    use spark_types::{IntWidth, LifetimeId, Ty};
    use std::collections::HashMap;
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let string = table.intern(Ty::String);
    let reference = |inner| Ty::Ref {
        inner,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    };
    let int_ref = table.intern(reference(int));
    let string_ref = table.intern(reference(string));
    let record_id = DefId::new(9500);
    let field = spark_intern::sym("text");
    table.set_struct_fields(record_id, vec![(field, string)]);
    let record = table.intern(Ty::Struct(record_id));
    let record_ref = table.intern(reference(record));
    let callback = table.intern(Ty::Function {
        params: vec![],
        ret: record_ref,
        is_async: false,
    });
    let helper_id = DefId::new(9501);
    let caller_id = DefId::new(9502);
    let opaque_id = DefId::new(9503);
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
                mutable: false,
                span,
            })
            .collect()
    };
    let helper = MirFunction {
        def_id: helper_id,
        param_count: 1,
        return_type: string_ref,
        locals: locals(&[string_ref, callback, record_ref]),
        blocks: vec![
            block(
                vec![],
                Terminator::Call {
                    func: copy(1),
                    args: vec![],
                    destination: local(2),
                    target: BlockId(1),
                    span,
                },
            ),
            block(
                vec![assign(
                    0,
                    Rvalue::Ref {
                        place: Place {
                            local: Local(2),
                            projections: vec![Projection::Deref, Projection::Field(field)],
                        },
                        mutable: false,
                    },
                )],
                Terminator::Return,
            ),
        ],
        captures: vec![],
        is_async: false,
        span,
    };
    let caller = MirFunction {
        def_id: caller_id,
        param_count: 0,
        return_type: string_ref,
        locals: locals(&[string_ref, int, int_ref, callback]),
        blocks: vec![
            block(
                vec![
                    assign(1, Rvalue::Use(Operand::Constant(Constant::Int(7)))),
                    assign(
                        2,
                        Rvalue::Ref {
                            place: local(1),
                            mutable: false,
                        },
                    ),
                    assign(
                        3,
                        Rvalue::Aggregate(
                            AggregateKind::Closure {
                                func_def_id: opaque_id,
                            },
                            vec![copy(2)],
                        ),
                    ),
                ],
                Terminator::Call {
                    func: Operand::Constant(Constant::FunctionRef(helper_id)),
                    args: vec![copy(3)],
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
    let mir = Mir {
        functions: HashMap::from([(helper_id, helper), (caller_id, caller)]),
    };
    let mut diagnostics = Diagnostics::new();
    analyze_program(&mir, &table, None, &mut diagnostics);
    assert!(diagnostics.iter().any(|diagnostic| diagnostic.message.starts_with("cannot return reference to local variable")), "projecting an opaque result must retain the captured frame dependency despite its different pointee type");
}

#[test]
fn binding_a_queued_read_discovers_its_input_storage_port() {
    use spark_mir::LocalDecl;
    use spark_span::Span;
    use spark_types::{IntWidth, LifetimeId, Ty};

    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let channel = table.intern(Ty::Channel { inner: reference });
    let function = MirFunction {
        def_id: DefId::new(9905),
        param_count: 1,
        return_type: reference,
        locals: [reference, channel]
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
    let mut flow = Flow::new(&function, &table, None);
    let state = flow.initial_state();
    let arguments = [state.value(Local(1))];
    let callee = Value::default();
    let selector = Selector {
        input: Local(1),
        selection: Vec::new(),
        intent: Intent::Value,
        expected: Some(reference),
        capture: None,
        channel_capture: None,
        queue: Some(super::super::address::QueueSelection {
            capture: None,
            channel,
            projection: Vec::new(),
        }),
    };
    let selected = flow.select_selector(
        &selector,
        &function,
        CallActuals {
            callee: &callee,
            arguments: &arguments,
            ty: reference,
            point: Point::new(BlockId::ENTRY, 0),
            state: &state,
            aliases: None,
        },
    );
    assert!(!selected.is_empty());
    let mut ports = Vec::new();
    flow.discover_ports(&Summary::default(), &mut ports);
    assert_eq!(ports.len(), 1);
    assert_eq!(
        ports[0].kind,
        super::super::aliases::StorageKind::Channel(channel)
    );
    assert_eq!(ports[0].input, Local(1));
    assert!(ports[0].selection.is_empty());
}
