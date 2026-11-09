use super::*;
use crate::facts::Point;
use spark_mir::{BlockId, Local, MirFunction};
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::{IntWidth, LifetimeId, TypeTable};

#[test]
fn tail_selection_joins_support_for_repeated_origins_in_different_slots() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let tuple = table.intern(Ty::Tuple(vec![reference, reference]));
    let function = MirFunction {
        def_id: DefId::new(9200),
        param_count: 0,
        return_type: int,
        locals: vec![],
        blocks: vec![],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let flow = Flow::new(&function, &table, None);
    let shared = Origin::Loan(Point::new(BlockId(0), 0));
    let parents: Vec<_> = (1..=2)
        .map(|index| Origin::Loan(Point::new(BlockId(0), index)))
        .collect();
    let mut value = Value::default();
    for (index, parent) in parents.iter().enumerate() {
        let contribution = Value::reference(shared.clone())
            .with_address_support(&std::iter::once(parent.clone()).collect());
        value.join(&contribution.prefixed(Slot::Tuple(index), tuple, &table));
    }
    let value = value.sourced(super::super::demand::Bank::Logical, Local(1));
    let selected = flow.tail_reference_value(value, tuple, Some(int), &State::default());
    assert_eq!(
        selected.address_sources().len(),
        2,
        "both reaching slots contribute to the selection"
    );
    let support = selected.address_support();
    for parent in parents {
        assert!(
            support.contains(&parent),
            "every selected contribution retains its own parent"
        );
    }
}
#[test]
fn opaque_pointee_copy_preserves_reaching_support() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let int_ref = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let list = table.intern(Ty::List(int_ref));
    let list_ref = table.intern(Ty::Ref {
        inner: list,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let function = MirFunction {
        def_id: DefId::new(9201),
        param_count: 0,
        return_type: int,
        locals: vec![],
        blocks: vec![],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let flow = Flow::new(&function, &table, None);
    let opaque = Origin::Unproven(Point::new(BlockId(0), 3));
    let parent = Origin::Loan(Point::new(BlockId(0), 1));
    let wrapper = Value::reference(opaque)
        .with_metadata(&super::super::value::Leaf {
            opaque: std::iter::once(super::super::address::OpaqueSite {
                function: function.def_id,
                point: Point::new(BlockId(0), 3),
            })
            .collect(),
            ..Default::default()
        })
        .with_address_support(&std::iter::once(parent.clone()).collect())
        .sourced(super::super::demand::Bank::Logical, Local(1));
    let payload = flow.dereference(&wrapper, list_ref, &State::default());
    assert!(
        payload
            .leaves
            .values()
            .any(|leaf| leaf.parents.contains(&parent)),
        "opaque copied payload must preserve parent support from this reaching wrapper"
    );
    assert!(
        !payload.all_sources().is_empty(),
        "opaque payload demand must trace back through its reaching wrapper"
    );
}
#[test]
fn opaque_tail_selection_preserves_reaching_support() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let int_ref = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let list = table.intern(Ty::List(int_ref));
    let list_ref = table.intern(Ty::Ref {
        inner: list,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let function = MirFunction {
        def_id: DefId::new(9202),
        param_count: 0,
        return_type: int,
        locals: vec![],
        blocks: vec![],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let flow = Flow::new(&function, &table, None);
    let opaque = Origin::Unproven(Point::new(BlockId(0), 3));
    let parent = Origin::Loan(Point::new(BlockId(0), 1));
    let wrapper = Value::reference(opaque)
        .with_metadata(&super::super::value::Leaf {
            opaque: std::iter::once(super::super::address::OpaqueSite {
                function: function.def_id,
                point: Point::new(BlockId(0), 3),
            })
            .collect(),
            ..Default::default()
        })
        .with_address_support(&std::iter::once(parent.clone()).collect())
        .sourced(super::super::demand::Bank::Logical, Local(1));
    let selected = flow.tail_reference_value(wrapper, list_ref, Some(int), &State::default());
    assert!(
        selected.address_support().contains(&parent),
        "typed opaque Tail selection must preserve parent support from its reaching wrapper"
    );
    assert!(
        !selected.address_sources().is_empty(),
        "typed opaque Tail selection must trace back through its reaching wrapper"
    );
}
#[test]
fn opaque_tail_expansion_joins_distinct_reaching_contexts() {
    use spark_mir::LocalDecl;
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = |inner| Ty::Ref {
        inner,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    };
    let int_ref = table.intern(reference(int));
    let list = table.intern(Ty::List(int_ref));
    let list_ref = table.intern(reference(list));
    let wrapper_ref = table.intern(reference(list_ref));
    let tuple = table.intern(Ty::Tuple(vec![wrapper_ref, wrapper_ref]));
    let locals = (0..=2)
        .map(|index| LocalDecl {
            local: Local(index),
            ty: list_ref,
            name: None,
            is_user_variable: false,
            mutable: false,
            span: Span::DUMMY,
        })
        .collect();
    let function = MirFunction {
        def_id: DefId::new(9203),
        param_count: 0,
        return_type: int,
        locals,
        blocks: vec![],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let flow = Flow::new(&function, &table, None);
    let opaque_site = Point::new(BlockId(0), 3);
    let opaque = Origin::Unproven(opaque_site);
    let parents: Vec<_> = (10..=11)
        .map(|index| Origin::Loan(Point::new(BlockId(0), index)))
        .collect();
    let mut state = State::default();
    let mut value = Value::default();
    for (index, parent) in parents.iter().enumerate() {
        let local = Local(index as u32 + 1);
        state.set(
            local,
            Value::reference(opaque.clone())
                .with_metadata(&super::super::value::Leaf {
                    opaque: std::iter::once(super::super::address::OpaqueSite {
                        function: function.def_id,
                        point: opaque_site,
                    })
                    .collect(),
                    ..Default::default()
                })
                .with_address_support(&std::iter::once(parent.clone()).collect()),
        );
        let point = Point::new(BlockId(0), index as u32 + 20);
        let metadata = super::super::value::Leaf {
            endpoints: std::iter::once(super::super::address::Endpoint::Frame {
                function: function.def_id,
                root: point,
                local,
                root_ty: list_ref,
                pointee: list_ref,
                location: Vec::new(),
            })
            .collect(),
            ..Default::default()
        };
        value.join(
            &Value::reference(Origin::Loan(point))
                .with_metadata(&metadata)
                .prefixed(Slot::Tuple(index), tuple, &table),
        );
    }
    let selected = flow.tail_reference_value(value, tuple, Some(int), &state);
    let support = selected.address_support();
    for parent in parents {
        assert!(
            support.contains(&parent),
            "opaque graph expansion must preserve each distinct reaching context"
        );
    }
}
#[test]
fn reborrow_snapshots_survive_holder_overwrite_while_cell_addresses_follow_the_cell() {
    use super::super::address::Endpoint;
    use spark_mir::{BasicBlock, Constant, LocalDecl, Operand, Rvalue, Statement, Terminator};
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let pointer = table.intern(Ty::Ref {
        inner: reference,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let types = [
        int, int, int, reference, reference, reference, reference, reference, pointer, reference,
    ];
    let local = |index| Place::from_local(Local(index));
    let copy = |index| Rvalue::Use(Operand::Copy(local(index)));
    let assign = |index, rvalue| Statement::Assign {
        place: local(index),
        rvalue,
        span: Span::DUMMY,
    };
    let borrow = |index| Rvalue::Ref {
        place: local(index),
        mutable: false,
    };
    let reborrow = || Rvalue::Ref {
        place: Place {
            local: Local(5),
            projections: vec![Projection::Deref],
        },
        mutable: false,
    };
    let function = MirFunction {
        def_id: DefId::new(9204),
        param_count: 0,
        return_type: int,
        locals: types
            .iter()
            .enumerate()
            .map(|(index, ty)| LocalDecl {
                local: Local(index as u32),
                ty: *ty,
                name: None,
                is_user_variable: false,
                mutable: true,
                span: Span::DUMMY,
            })
            .collect(),
        blocks: vec![BasicBlock {
            statements: vec![
                assign(1, Rvalue::Use(Operand::Constant(Constant::Int(7)))),
                assign(2, Rvalue::Use(Operand::Constant(Constant::Int(9)))),
                assign(3, borrow(1)),
                assign(4, borrow(2)),
                assign(5, copy(3)),
                assign(6, reborrow()),
                assign(7, copy(6)),
                assign(8, borrow(5)),
                assign(5, copy(4)),
                assign(9, reborrow()),
            ],
            terminator: Terminator::Return,
            is_cleanup: false,
        }],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let facts = crate::facts::extract_facts(&function);
    let mut flow = Flow::new(&function, &table, None);
    let mut state = flow.initial_state();
    flow.transfer_block(BlockId(0), &mut state);
    let original = Endpoint::Frame {
        function: function.def_id,
        root: facts.loans[0].issued_at,
        local: Local(1),
        root_ty: int,
        pointee: int,
        location: vec![],
    };
    let replacement = Endpoint::Frame {
        function: function.def_id,
        root: facts.loans[1].issued_at,
        local: Local(2),
        root_ty: int,
        pointee: int,
        location: vec![],
    };
    let endpoints = |value: Value| {
        value
            .leaves
            .into_values()
            .flat_map(|leaf| leaf.endpoints)
            .collect::<FxHashSet<_>>()
    };
    assert_eq!(
        endpoints(flow.read(&local(7), &state)),
        std::iter::once(original).collect(),
        "copying the reborrow preserves the original selected pointee"
    );
    assert_eq!(
        endpoints(flow.read(&local(9), &state)),
        std::iter::once(replacement.clone()).collect(),
        "a new reborrow observes the overwritten holder"
    );
    let cell = Place {
        local: Local(8),
        projections: vec![Projection::Deref],
    };
    assert_eq!(
        endpoints(flow.read(&cell, &state)),
        std::iter::once(replacement).collect(),
        "a reference to the holder cell observes its current contents"
    );
}
#[test]
fn typed_tail_selection_sheds_a_projected_map_wrapper_with_a_widened_location() {
    use spark_mir::{BasicBlock, LocalDecl, Rvalue, Statement, Terminator};
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
    let children = spark_intern::sym("children");
    let payload = spark_intern::sym("payload");
    let wanted = spark_intern::sym("wanted");
    let wrapper = spark_intern::sym("wrapper");
    let node_id = DefId::new(9600);
    let node = table.intern(Ty::Struct(node_id));
    let node_list = table.intern(Ty::List(node));
    table.set_struct_fields(node_id, vec![(children, node_list), (payload, map)]);
    let query_id = DefId::new(9601);
    let query = table.intern(Ty::Struct(query_id));
    let query_list = table.intern(Ty::List(query));
    table.set_struct_fields(
        query_id,
        vec![
            (children, query_list),
            (wanted, int_ref),
            (wrapper, map_ref),
        ],
    );
    let types = [int, int_ref, node, map_ref, int];
    let function = MirFunction {
        def_id: DefId::new(9602),
        param_count: 1,
        return_type: int,
        locals: types
            .iter()
            .enumerate()
            .map(|(index, ty)| LocalDecl {
                local: Local(index as u32),
                ty: *ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            })
            .collect(),
        blocks: vec![BasicBlock {
            statements: vec![Statement::Assign {
                place: Place::from_local(Local(3)),
                rvalue: Rvalue::Ref {
                    place: Place {
                        local: Local(2),
                        projections: vec![
                            Projection::Field(children),
                            Projection::Index(Local(4)),
                            Projection::Field(payload),
                        ],
                    },
                    mutable: false,
                },
                span: Span::DUMMY,
            }],
            terminator: Terminator::Return,
            is_cleanup: false,
        }],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let mut flow = Flow::new(&function, &table, None);
    let incoming = value::inputs(int_ref, Local(1), int_ref, &[], &table);
    let node_value = incoming
        .prefixed(Slot::Value, map, &table)
        .prefixed(Slot::Field(payload), node, &table)
        .prefixed(Slot::Element, node_list, &table)
        .prefixed(Slot::Field(children), node, &table);
    let mut state = flow.initial_state();
    state.set(Local(2), node_value);
    flow.transfer_block(BlockId(0), &mut state);
    let mut query_value = state
        .value(Local(3))
        .prefixed(Slot::Field(wrapper), query, &table)
        .prefixed(Slot::Element, query_list, &table)
        .prefixed(Slot::Field(children), query, &table);
    query_value.join(&incoming.prefixed(Slot::Field(wanted), query, &table));
    let selected = flow.tail_reference_value(query_value, query, Some(int), &state);
    assert_eq!(selected.origins(), incoming.origins(), "typed selection of Ref[Int] must read the map payload and shed its local Ref[Map] wrapper even when the wrapper location widens to Tail");
}
#[test]
fn typed_tail_selection_traverses_but_does_not_select_a_ref_to_ref_wrapper() {
    use spark_mir::{BasicBlock, LocalDecl, Rvalue, Statement, Terminator};
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = |inner| Ty::Ref {
        inner,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    };
    let int_ref = table.intern(reference(int));
    let wrapper_ref = table.intern(reference(int_ref));
    let children = spark_intern::sym("children");
    let wanted = spark_intern::sym("wanted");
    let wrapper = spark_intern::sym("wrapper");
    let query_id = DefId::new(9603);
    let query = table.intern(Ty::Struct(query_id));
    let query_list = table.intern(Ty::List(query));
    table.set_struct_fields(
        query_id,
        vec![
            (children, query_list),
            (wanted, int_ref),
            (wrapper, wrapper_ref),
        ],
    );
    let types = [int, int_ref, int_ref, wrapper_ref];
    let function = MirFunction {
        def_id: DefId::new(9604),
        param_count: 1,
        return_type: int,
        locals: types
            .iter()
            .enumerate()
            .map(|(index, ty)| LocalDecl {
                local: Local(index as u32),
                ty: *ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            })
            .collect(),
        blocks: vec![BasicBlock {
            statements: vec![Statement::Assign {
                place: Place::from_local(Local(3)),
                rvalue: Rvalue::Ref {
                    place: Place::from_local(Local(2)),
                    mutable: false,
                },
                span: Span::DUMMY,
            }],
            terminator: Terminator::Return,
            is_cleanup: false,
        }],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let mut flow = Flow::new(&function, &table, None);
    let incoming = value::inputs(int_ref, Local(1), int_ref, &[], &table);
    let mut state = flow.initial_state();
    state.set(Local(2), incoming.clone());
    flow.transfer_block(BlockId(0), &mut state);
    let mut query_value = state
        .value(Local(3))
        .prefixed(Slot::Field(wrapper), query, &table)
        .prefixed(Slot::Element, query_list, &table)
        .prefixed(Slot::Field(children), query, &table);
    query_value.join(&incoming.prefixed(Slot::Field(wanted), query, &table));
    let selected = flow.tail_reference_value(query_value, query, Some(int), &state);
    assert_eq!(
        selected.origins(),
        incoming.origins(),
        "a known Ref[Ref[Int]] wrapper must be traversed but must not become a Ref[Int] result"
    );
}

#[test]
fn typed_tail_selection_preserves_a_layout_preserving_reference_cast() {
    use spark_mir::{AggregateKind, BasicBlock, LocalDecl, Operand, Rvalue, Statement, Terminator};
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let mutable_int = table.intern(Ty::Mutable(int));
    let source_list = table.intern(Ty::List(mutable_int));
    let target_list = table.intern(Ty::List(int));
    let reference = |inner| Ty::Ref {
        inner,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    };
    let source_ref = table.intern(reference(source_list));
    let target_ref = table.intern(reference(target_list));
    assert_eq!(
        spark_types::check_expr::cast_kind(source_ref, target_ref, &table),
        Ok(spark_types::check_expr::CastKind::Identity)
    );
    let children = spark_intern::sym("children");
    let pointer = spark_intern::sym("pointer");
    let query_id = DefId::new(9800);
    let query = table.intern(Ty::Struct(query_id));
    let query_list = table.intern(Ty::List(query));
    table.set_struct_fields(
        query_id,
        vec![(children, query_list), (pointer, target_ref)],
    );
    let local = |index| Place::from_local(Local(index));
    let assign = |index, rvalue| Statement::Assign {
        place: local(index),
        rvalue,
        span: Span::DUMMY,
    };
    let types = [int, source_list, source_ref, target_ref];
    let function = MirFunction {
        def_id: DefId::new(9801),
        param_count: 0,
        return_type: int,
        locals: types
            .iter()
            .enumerate()
            .map(|(index, ty)| LocalDecl {
                local: Local(index as u32),
                ty: *ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            })
            .collect(),
        blocks: vec![BasicBlock {
            statements: vec![
                assign(1, Rvalue::Aggregate(AggregateKind::List, vec![])),
                assign(
                    2,
                    Rvalue::Ref {
                        place: local(1),
                        mutable: false,
                    },
                ),
                assign(
                    3,
                    Rvalue::Cast {
                        operand: Operand::Copy(local(2)),
                        target_ty: target_ref,
                    },
                ),
            ],
            terminator: Terminator::Return,
            is_cleanup: false,
        }],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let mut flow = Flow::new(&function, &table, None);
    let mut state = flow.initial_state();
    flow.transfer_block(BlockId(0), &mut state);
    let borrowed = state.value(Local(3));
    let carrier = borrowed
        .prefixed(Slot::Field(pointer), query, &table)
        .prefixed(Slot::Element, query_list, &table)
        .prefixed(Slot::Field(children), query, &table);
    let selected = flow.tail_reference_value(carrier, query, Some(target_list), &state);
    assert_eq!(selected.origins(), borrowed.origins(), "a supported identity reference cast must not erase the local loan during typed Tail filtering");
}
