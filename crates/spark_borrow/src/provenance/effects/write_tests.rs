use super::*;
use spark_mir::{AggregateKind, BasicBlock, Constant, LocalDecl, Place, Rvalue, Statement};
use spark_span::Span;
use spark_types::{IntWidth, LifetimeId, Ty};

#[test]
fn pending_memory_summary_is_not_a_returning_empty_clear() {
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
        mutable: true,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let void = table.intern(Ty::Void);
    let span = Span::DUMMY;
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
    let copy = |index| Operand::Copy(Place::from_local(Local(index)));
    let assign = |index, rvalue| Statement::Assign {
        place: Place::from_local(Local(index)),
        rvalue,
        span,
    };
    let helper = MirFunction {
        def_id: DefId::new(9800),
        param_count: 1,
        return_type: void,
        locals: locals(&[void, map_ref]),
        blocks: vec![BasicBlock {
            statements: vec![],
            terminator: Terminator::Return,
            is_cleanup: false,
        }],
        captures: vec![],
        is_async: false,
        span,
    };
    let caller = MirFunction {
        def_id: DefId::new(9801),
        param_count: 1,
        return_type: map,
        locals: locals(&[map, reference, map, map_ref, void]),
        blocks: vec![
            BasicBlock {
                statements: vec![
                    assign(
                        2,
                        Rvalue::Aggregate(
                            AggregateKind::Map,
                            vec![
                                Operand::Constant(Constant::String(spark_intern::sym("one"))),
                                copy(1),
                            ],
                        ),
                    ),
                    assign(
                        3,
                        Rvalue::Ref {
                            place: Place::from_local(Local(2)),
                            mutable: true,
                        },
                    ),
                ],
                terminator: Terminator::Call {
                    func: Operand::Constant(Constant::FunctionRef(helper.def_id)),
                    args: vec![copy(3)],
                    destination: Place::from_local(Local(4)),
                    target: BlockId(1),
                    span,
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![assign(0, Rvalue::Use(copy(2)))],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
        ],
        captures: vec![],
        is_async: false,
        span,
    };
    let mir = Mir {
        functions: std::collections::HashMap::from([(helper.def_id, helper)]),
    };
    let facts = crate::facts::extract_facts(&caller);
    let ports = FxHashMap::from_iter([(DefId::new(9800), Vec::new())]);
    for normal_return in [false, true] {
        let summaries = FxHashMap::from_iter([(
            Key::distinct(DefId::new(9800)),
            Summary {
                normal_return,
                memory: FxHashMap::from_iter([(Local(1), Value::default())]),
                modified: FxHashSet::from_iter([Local(1)]),
                ..Summary::default()
            },
        )]);
        let mut flow = Flow::new(
            &caller,
            &table,
            Some(Context {
                mir: &mir,
                summaries: &summaries,
                ports: &ports,
            }),
        );
        flow.solve(&facts);
        assert_eq!(
            flow.entries[1].is_some(),
            normal_return,
            "only an established normal return reaches the continuation"
        );
        let summary = flow.summary();
        assert_eq!(summary.normal_return, normal_return);
        assert!(
            summary.returned.is_empty(),
            "a returning empty clear removes the old map payload"
        );
    }
}

#[test]
fn empty_optional_write_footprints_reach_later_return_blocks() {
    use spark_mir::Projection;
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let string = table.intern(Ty::String);
    let boolean = table.intern(Ty::Bool);
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let map = table.intern(Ty::Map(string, reference));
    let map_ref = table.intern(Ty::Ref {
        inner: map,
        mutable: true,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let void = table.intern(Ty::Void);
    let span = Span::DUMMY;
    let block = |statements, terminator| BasicBlock {
        statements,
        terminator,
        is_cleanup: false,
    };
    let function = MirFunction {
        def_id: DefId::new(9803),
        param_count: 2,
        return_type: void,
        locals: [void, map_ref, boolean, map]
            .into_iter()
            .enumerate()
            .map(|(index, ty)| LocalDecl {
                local: Local(index as u32),
                ty,
                name: None,
                is_user_variable: false,
                mutable: true,
                span,
            })
            .collect(),
        blocks: vec![
            block(
                vec![Statement::Assign {
                    place: Place::from_local(Local(3)),
                    rvalue: Rvalue::Aggregate(AggregateKind::Map, vec![]),
                    span,
                }],
                Terminator::SwitchInt {
                    discriminant: Operand::Copy(Place::from_local(Local(2))),
                    targets: vec![(1, BlockId(1))],
                    otherwise: BlockId(2),
                },
            ),
            block(vec![], Terminator::Goto { target: BlockId(3) }),
            block(
                vec![Statement::Assign {
                    place: Place {
                        local: Local(1),
                        projections: vec![Projection::Deref],
                    },
                    rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(3)))),
                    span,
                }],
                Terminator::Goto { target: BlockId(4) },
            ),
            block(vec![], Terminator::Goto { target: BlockId(5) }),
            block(vec![], Terminator::Goto { target: BlockId(3) }),
            block(vec![], Terminator::Return),
        ],
        captures: vec![],
        is_async: false,
        span,
    };
    let mut flow = Flow::new(&function, &table, None);
    flow.solve(&crate::facts::extract_facts(&function));
    let summary = flow.summary();
    assert!(summary.normal_return);
    assert!(summary.modified.contains(&Local(1)), "a possible empty write is a footprint even without new borrowed payload or a definite-write mask");
}

#[test]
fn unused_reference_inputs_do_not_enumerate_alias_partitions() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let string = table.intern(Ty::String);
    let map = table.intern(Ty::Map(string, reference));
    let map_ref = table.intern(Ty::Ref {
        inner: map,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let void = table.intern(Ty::Void);
    let def = DefId::new(9811);
    let function = MirFunction {
        def_id: def,
        param_count: 20,
        return_type: void,
        locals: std::iter::once(void)
            .chain(std::iter::repeat_n(map_ref, 20))
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
        blocks: vec![BasicBlock {
            statements: vec![],
            terminator: Terminator::Return,
            is_cleanup: false,
        }],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let facts = FxHashMap::from_iter([(def, crate::facts::extract_facts(&function))]);
    let mir = Mir {
        functions: std::collections::HashMap::from([(def, function)]),
    };
    let (summaries, ports) = infer_summaries(&mir, &table, &facts);
    assert_eq!(
        summaries.len(),
        1,
        "unused signature roots must not trigger Bell-number context growth"
    );
    assert!(ports[&def].is_empty());
    assert!(summaries[&Key::distinct(def)].normal_return);
}

#[test]
fn readonly_observed_inputs_do_not_enumerate_alias_partitions() {
    use spark_mir::Projection;
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let string = table.intern(Ty::String);
    let map = table.intern(Ty::Map(string, reference));
    let map_ref = table.intern(Ty::Ref {
        inner: map,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let void = table.intern(Ty::Void);
    let def = DefId::new(9812);
    let span = Span::DUMMY;
    let function = MirFunction {
        def_id: def,
        param_count: 4,
        return_type: void,
        locals: [void, map_ref, map_ref, map_ref, map_ref, map]
            .into_iter()
            .enumerate()
            .map(|(index, ty)| LocalDecl {
                local: Local(index as u32),
                ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span,
            })
            .collect(),
        blocks: vec![BasicBlock {
            statements: (1..=4)
                .map(|index| Statement::Assign {
                    place: Place::from_local(Local(5)),
                    rvalue: Rvalue::Use(Operand::Copy(Place {
                        local: Local(index),
                        projections: vec![Projection::Deref],
                    })),
                    span,
                })
                .collect(),
            terminator: Terminator::Return,
            is_cleanup: false,
        }],
        captures: vec![],
        is_async: false,
        span,
    };
    let facts = FxHashMap::from_iter([(def, crate::facts::extract_facts(&function))]);
    let mir = Mir {
        functions: std::collections::HashMap::from([(def, function)]),
    };
    let (summaries, _) = infer_summaries(&mir, &table, &facts);
    assert_eq!(
        summaries.len(),
        1,
        "alias equality cannot affect entry-state reads without writes"
    );
}
