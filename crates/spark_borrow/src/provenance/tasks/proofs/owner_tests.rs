// TAG [SPARK-114]

use super::*;
use spark_mir::{AggregateKind, BasicBlock, Constant, LocalDecl, Place, Rvalue, Statement};
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::{IntWidth, TypeId};

fn assign(local: u32, operand: Operand) -> Statement {
    Statement::Assign {
        place: Place::from_local(Local(local)),
        rvalue: Rvalue::Use(operand),
        span: Span::DUMMY,
    }
}
fn moved(local: u32) -> Operand {
    Operand::Move(Place::from_local(Local(local)))
}
fn copied(local: u32) -> Operand {
    Operand::Copy(Place::from_local(Local(local)))
}
fn drop_owner(local: u32) -> Statement {
    Statement::Drop {
        place: Place::from_local(Local(local)),
        span: Span::DUMMY,
    }
}
fn block(statements: Vec<Statement>, terminator: Terminator) -> BasicBlock {
    BasicBlock {
        statements,
        terminator,
        is_cleanup: false,
    }
}
fn guarantee(
    table: &TypeTable,
    types: &[TypeId],
    parameters: u32,
    blocks: Vec<BasicBlock>,
) -> Guarantee {
    let function = MirFunction {
        def_id: DefId::new(9960),
        param_count: parameters,
        return_type: types[0],
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
        blocks,
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let mut flow = Flow::new(&function, table, None);
    flow.solve(&crate::facts::extract_facts(&function));
    flow.future_guarantee()
}
fn returns(table: &TypeTable, types: &[TypeId], statements: Vec<Statement>) -> Certificate {
    let Guarantee::Returns(certificate) =
        guarantee(table, types, 1, vec![block(statements, Terminator::Return)])
    else {
        panic!("expected a normal return")
    };
    assert!(
        certificate.completed.is_empty(),
        "release evidence cannot manufacture a join"
    );
    certificate
}
fn fixture() -> (TypeTable, TypeId, TypeId, TypeId) {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: int,
        is_async: false,
    });
    (table, int, future, callable)
}

#[test]
fn returning_a_copy_releases_only_the_provided_owner() {
    let (table, _, future, _) = fixture();
    let certificate = returns(
        &table,
        &[future, future, future],
        vec![assign(2, copied(1)), drop_owner(1), assign(0, moved(2))],
    );
    assert_eq!(
        certificate.released_provided,
        FxHashSet::from_iter([Local(1)])
    );
    assert!(certificate.retains_no_owner.is_empty());
    assert_eq!(certificate.returned, FxHashSet::from_iter([Local(1)]));
}

#[test]
fn returning_the_provided_owner_does_not_certify_its_release() {
    let (table, _, future, _) = fixture();
    let certificate = returns(
        &table,
        &[future, future, future],
        vec![assign(2, moved(1)), assign(0, moved(2))],
    );
    assert!(certificate.released_provided.is_empty());
    assert!(certificate.retains_no_owner.is_empty());
    assert_eq!(certificate.returned, FxHashSet::from_iter([Local(1)]));
}

#[test]
fn dropping_every_copy_retains_no_input_owner() {
    let (table, int, future, _) = fixture();
    let certificate = returns(
        &table,
        &[int, future, future],
        vec![
            assign(2, copied(1)),
            drop_owner(1),
            drop_owner(2),
            assign(0, Operand::Constant(Constant::Int(0))),
        ],
    );
    assert_eq!(
        certificate.released_provided,
        FxHashSet::from_iter([Local(1)])
    );
    assert_eq!(
        certificate.retains_no_owner,
        FxHashSet::from_iter([Local(1)])
    );
}

fn capture(operand: Operand) -> Statement {
    Statement::Assign {
        place: Place::from_local(Local(3)),
        rvalue: Rvalue::Aggregate(
            AggregateKind::Closure {
                func_def_id: DefId::new(9961),
            },
            vec![operand],
        ),
        span: Span::DUMMY,
    }
}

#[test]
fn moving_an_environment_preserves_provided_token_cleanup() {
    let (table, int, future, callable) = fixture();
    let certificate = returns(
        &table,
        &[int, future, future, callable, callable],
        vec![
            capture(moved(1)),
            assign(4, moved(3)),
            assign(4, moved(4)),
            drop_owner(4),
            assign(0, Operand::Constant(Constant::Int(0))),
        ],
    );
    assert_eq!(
        certificate.released_provided,
        FxHashSet::from_iter([Local(1)])
    );
    assert_eq!(
        certificate.retains_no_owner,
        FxHashSet::from_iter([Local(1)])
    );
}

#[test]
fn a_returned_shared_environment_can_keep_the_provided_token() {
    let (table, _, future, callable) = fixture();
    let certificate = returns(
        &table,
        &[callable, future, future, callable, callable],
        vec![
            capture(moved(1)),
            assign(4, copied(3)),
            drop_owner(3),
            assign(0, moved(4)),
        ],
    );
    assert!(certificate.released_provided.is_empty());
    assert!(certificate.retains_no_owner.is_empty());
}

#[test]
fn release_must_hold_on_every_normal_return_path() {
    let (mut table, _, future, _) = fixture();
    let flag = table.intern(Ty::Bool);
    let result = guarantee(
        &table,
        &[future, future, future, flag],
        3,
        vec![
            block(
                vec![],
                Terminator::SwitchInt {
                    discriminant: copied(3),
                    targets: vec![(1, BlockId(1))],
                    otherwise: BlockId(2),
                },
            ),
            block(
                vec![drop_owner(1), assign(0, moved(2))],
                Terminator::Goto { target: BlockId(3) },
            ),
            block(
                vec![drop_owner(2), assign(0, moved(1))],
                Terminator::Goto { target: BlockId(3) },
            ),
            block(vec![], Terminator::Return),
        ],
    );
    let Guarantee::Returns(certificate) = result else {
        panic!("expected a normal return")
    };
    assert!(certificate.released_provided.is_empty());
    assert!(certificate.retains_no_owner.is_empty());
}

#[test]
fn a_diverging_body_does_not_certify_input_release() {
    let (table, int, future, _) = fixture();
    assert_eq!(
        guarantee(
            &table,
            &[int, future],
            1,
            vec![
                block(vec![drop_owner(1)], Terminator::Goto { target: BlockId(1) }),
                block(vec![], Terminator::Goto { target: BlockId(1) }),
            ]
        ),
        Guarantee::NoReturn
    );
}

#[test]
fn unknown_calls_preserve_escaped_owner_possibilities() {
    let (table, int, future, _) = fixture();
    for (argument, provided_released) in [(moved(1), false), (copied(1), true)] {
        let result = guarantee(
            &table,
            &[int, future, int],
            1,
            vec![
                block(
                    vec![],
                    Terminator::Call {
                        func: Operand::Constant(Constant::FunctionRef(DefId::new(9962))),
                        args: vec![argument],
                        destination: Place::from_local(Local(2)),
                        target: BlockId(1),
                        span: Span::DUMMY,
                    },
                ),
                block(
                    if provided_released {
                        vec![drop_owner(1), assign(0, moved(2))]
                    } else {
                        vec![assign(0, moved(2))]
                    },
                    Terminator::Return,
                ),
            ],
        );
        let Guarantee::Returns(certificate) = result else {
            panic!("expected a possible normal return")
        };
        assert_eq!(
            certificate.released_provided.contains(&Local(1)),
            provided_released
        );
        assert!(certificate.retains_no_owner.is_empty());
        assert!(certificate.completed.is_empty());
    }
}

#[test]
fn release_only_certificates_participate_in_the_descending_proof_lattice() {
    let mut proof = Guarantee::Returns(Certificate {
        released_provided: FxHashSet::from_iter([Local(1)]),
        ..Certificate::default()
    });
    assert!(proof.has_effect());
    assert!(proof.intersect(&Guarantee::Returns(Certificate::default())));
    assert!(!proof.has_effect());
    assert!(
        !Guarantee::NoReturn.intersect(&Guarantee::Returns(Certificate {
            released_provided: FxHashSet::from_iter([Local(1)]),
            retains_no_owner: FxHashSet::from_iter([Local(1)]),
            ..Certificate::default()
        }))
    );
}

#[test]
fn copying_a_borrowed_view_does_not_retain_the_provided_owner() {
    let (mut table, int, future, _) = fixture();
    let reference = table.intern(Ty::Ref {
        inner: future,
        mutable: false,
        lifetime: spark_types::LifetimeId::ANONYMOUS,
    });
    let certificate = returns(
        &table,
        &[int, future, reference, reference],
        vec![
            Statement::Assign {
                place: Place::from_local(Local(2)),
                rvalue: Rvalue::Ref {
                    place: Place::from_local(Local(1)),
                    mutable: false,
                },
                span: Span::DUMMY,
            },
            assign(3, copied(2)),
            drop_owner(1),
            assign(0, Operand::Constant(Constant::Int(0))),
        ],
    );
    assert_eq!(
        certificate.released_provided,
        FxHashSet::from_iter([Local(1)])
    );
    assert_eq!(
        certificate.retains_no_owner,
        FxHashSet::from_iter([Local(1)])
    );
}

#[test]
fn opaque_access_preserves_owner_escape_through_address_dependencies() {
    use crate::provenance::{
        address::{Dependency, Endpoint, FrameLifetime},
        Origin, Value,
    };
    let (mut table, int, future, _) = fixture();
    let reference = table.intern(Ty::Ref {
        inner: future,
        mutable: false,
        lifetime: spark_types::LifetimeId::ANONYMOUS,
    });
    let function = MirFunction {
        def_id: DefId::new(9963),
        param_count: 1,
        return_type: int,
        locals: [int, future, reference]
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
        blocks: vec![],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let flow = Flow::new(&function, &table, None);
    let mut state = flow.initial_state();
    let point = crate::facts::Point::new(BlockId::ENTRY, 0);
    let mut view = Value::reference(Origin::Loan(point));
    for leaf in view.leaves.values_mut() {
        leaf.dependencies
            .insert(Dependency::Address(Endpoint::Frame {
                lifetime: FrameLifetime::Local,
                function: function.def_id,
                root: point,
                local: Local(1),
                root_ty: future,
                pointee: future,
                location: vec![],
            }));
    }
    state.set(Local(2), view);
    flow.escape_input_owner_operand(&copied(2), &mut state);
    state.input_owners.drop_local(Local(1));
    assert!(
        !state.input_owners.retains_no_owner(Local(1)),
        "opaque access through a sampled address can retain another owner"
    );
}

#[test]
fn opaque_access_through_nested_borrows_does_not_prove_no_retained_owner() {
    let (mut table, int, future, _) = fixture();
    let reference = table.intern(Ty::Ref {
        inner: future,
        mutable: false,
        lifetime: spark_types::LifetimeId::ANONYMOUS,
    });
    let nested = table.intern(Ty::Ref {
        inner: reference,
        mutable: false,
        lifetime: spark_types::LifetimeId::ANONYMOUS,
    });
    let borrow = |destination, source| Statement::Assign {
        place: Place::from_local(Local(destination)),
        rvalue: Rvalue::Ref {
            place: Place::from_local(Local(source)),
            mutable: false,
        },
        span: Span::DUMMY,
    };
    let result = guarantee(
        &table,
        &[int, future, reference, nested, int],
        1,
        vec![
            block(
                vec![borrow(2, 1), borrow(3, 2)],
                Terminator::Call {
                    func: Operand::Constant(Constant::FunctionRef(DefId::new(9964))),
                    args: vec![copied(3)],
                    destination: Place::from_local(Local(4)),
                    target: BlockId(1),
                    span: Span::DUMMY,
                },
            ),
            block(vec![drop_owner(1), assign(0, moved(4))], Terminator::Return),
        ],
    );
    let Guarantee::Returns(certificate) = result else {
        panic!("expected a possible normal return")
    };
    assert!(
        certificate.retains_no_owner.is_empty(),
        "an opaque reader can retain the Future through nested borrowed views"
    );
}
