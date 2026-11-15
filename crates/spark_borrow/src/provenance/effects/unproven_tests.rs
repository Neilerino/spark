// TAG [SPARK-114]

use super::super::obligations::Boundary;
use super::super::{demand::Storage, value::Leaf};
use super::*;
use spark_span::Span;
use spark_types::{IntWidth, Ty};

fn endpoint(function: DefId, point: Point, ty: TypeId) -> Endpoint {
    Endpoint::Frame {
        function,
        root: point,
        local: Local(1),
        root_ty: ty,
        pointee: ty,
        location: Vec::new(),
    }
}

#[test]
fn prefix_summary_growth_includes_metadata_without_a_normal_return() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let point = Point::new(BlockId::ENTRY, 0);
    let mut value = Value::reference(Origin::Loan(point));
    let mut summary = Summary::default();
    let mut incoming = Summary::default();
    incoming
        .obligations
        .insert(Boundary::Store(int), value.clone());
    assert!(summary.join(&incoming));
    assert!(!summary.join(&incoming));
    value = value.with_metadata(&Leaf {
        endpoints: FxHashSet::from_iter([endpoint(DefId::new(9850), point, int)]),
        ..Default::default()
    });
    incoming
        .obligations
        .insert(Boundary::Store(int), value.clone());
    assert!(
        summary.join(&incoming),
        "new endpoint must wake dependent callers"
    );
    assert_eq!(summary.obligations[&Boundary::Store(int)], value);
    assert!(!summary.normal_return);
    assert!(!summary.join(&incoming));
}

#[test]
fn prefix_obligations_demand_sources_without_defining_a_destination() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let function = MirFunction {
        def_id: DefId::new(9851),
        param_count: 0,
        return_type: int,
        locals: vec![],
        blocks: vec![],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let point = Point::new(BlockId::ENTRY, 0);
    let value = Value::reference(Origin::Loan(point)).sourced(&Storage::Logical(Local(1)));
    let mut flow = Flow::new(&function, &table, None);
    flow.record_obligation(point, Boundary::Store(int), &value);
    assert_eq!(flow.transfers[&point].seeds, value.all_sources());
    assert!(!flow.transfers[&point].seeds.is_empty());
    assert!(flow.transfers[&point].definitions.is_empty());
    assert_eq!(flow.obligations[&(point, Boundary::Store(int))], value);
    flow.record_obligation(
        point,
        Boundary::Store(TypeId::ERROR),
        &Value::function(DefId::new(9852)),
    );
    assert_eq!(
        flow.obligations.len(),
        1,
        "owned functions do not borrow storage"
    );
}

#[test]
fn captured_address_dependencies_retain_only_the_current_frame_sources() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let function = DefId::new(9853);
    let point = Point::new(BlockId::ENTRY, 0);
    let other = Point::new(BlockId::ENTRY, 1);
    let value = Value::reference(Origin::Loan(point)).with_metadata(&Leaf {
        dependencies: FxHashSet::from_iter([
            Dependency::Address(endpoint(function, point, int)),
            Dependency::Address(endpoint(DefId::new(9854), other, int)),
        ]),
        ..Default::default()
    });
    assert_eq!(
        value.frame_sources(function).collect::<Vec<_>>(),
        vec![point]
    );
}

#[test]
fn invocation_obligations_exclude_only_proven_targets_capture_environments() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: spark_types::LifetimeId::ANONYMOUS,
    });
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: int,
        is_async: false,
    });
    let caller = MirFunction {
        def_id: DefId::new(9870),
        param_count: 0,
        return_type: int,
        locals: vec![],
        blocks: vec![],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let target = MirFunction {
        def_id: DefId::new(9871),
        captures: vec![(DefId::new(9872), reference)],
        ..caller.clone()
    };
    let call = Point::new(BlockId::ENTRY, 0);
    let known_source = Point::new(BlockId::ENTRY, 1);
    let unknown_source = Point::new(BlockId::ENTRY, 2);
    let captured = |point| {
        Value::reference(Origin::Loan(point)).with_metadata(&Leaf {
            endpoints: FxHashSet::from_iter([endpoint(caller.def_id, point, int)]),
            ..Leaf::default()
        })
    };
    let mut mixed = Value::function(target.def_id);
    mixed.callees.get_mut(&Vec::new()).unwrap().unresolved = true;
    mixed.join(&captured(known_source).prefixed(
        Slot::Capture(target.def_id, 0, reference),
        callable,
        &table,
    ));
    mixed.join(&captured(unknown_source).prefixed(Slot::Tail, callable, &table));
    for known_body in [false, true] {
        let mir = Mir {
            functions: if known_body {
                std::collections::HashMap::from([(target.def_id, target.clone())])
            } else {
                std::collections::HashMap::new()
            },
        };
        let summaries = Summaries::default();
        let ports = Ports::default();
        let mut flow = Flow::new(
            &caller,
            &table,
            Some(Context {
                proofs: None,
                definitions: None,
                mir: &mir,
                summaries: &summaries,
                ports: &ports,
            }),
        );
        flow.record_obligation(
            call,
            Boundary::invoke(
                callable,
                if known_body {
                    vec![target.def_id]
                } else {
                    vec![]
                },
            ),
            &mixed,
        );
        let remaining: FxHashSet<_> = flow.obligations[&(
            call,
            Boundary::invoke(
                callable,
                if known_body {
                    vec![target.def_id]
                } else {
                    vec![]
                },
            ),
        )]
            .frame_sources(caller.def_id)
            .collect();
        assert!(remaining.contains(&unknown_source));
        assert_eq!(
            remaining.contains(&known_source),
            !known_body,
            "only a body checked through the regular call path discharges its capture effects"
        );
        let undispatched = Boundary::invoke(callable, []);
        flow.record_obligation(call, undispatched.clone(), &mixed);
        assert!(
            flow.obligations[&(call, undispatched)]
                .frame_sources(caller.def_id)
                .any(|point| point == known_source),
            "binding to a known target does not prove original dispatch"
        );
        flow.record_obligation(call, Boundary::InvokeArgument(callable), &mixed);
        assert!(
            flow.obligations[&(call, Boundary::InvokeArgument(callable))]
                .frame_sources(caller.def_id)
                .any(|point| point == known_source),
            "an unknown outer callback may retain a known inner callback's borrowed captures"
        );
        flow.record_obligation(call, Boundary::Store(callable), &mixed);
        assert!(flow.obligations[&(call, Boundary::Store(callable))]
            .frame_sources(caller.def_id)
            .any(|point| point == known_source));
    }
}

#[test]
fn imported_invocation_keeps_its_original_dispatch_witness() {
    use spark_mir::LocalDecl;
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: spark_types::LifetimeId::ANONYMOUS,
    });
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: int,
        is_async: false,
    });
    let caller = MirFunction {
        def_id: DefId::new(9880),
        param_count: 0,
        return_type: int,
        locals: vec![],
        blocks: vec![],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let wrapper = MirFunction {
        def_id: DefId::new(9881),
        param_count: 1,
        return_type: callable,
        locals: (0..2)
            .map(|index| LocalDecl {
                local: Local(index),
                ty: callable,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            })
            .collect(),
        ..caller.clone()
    };
    let target = DefId::new(9882);
    let point = Point::new(BlockId::ENTRY, 0);
    let actual = Value::reference(Origin::Loan(point))
        .with_metadata(&Leaf {
            endpoints: FxHashSet::from_iter([endpoint(caller.def_id, point, int)]),
            ..Leaf::default()
        })
        .prefixed(Slot::Capture(target, 0, reference), callable, &table);
    let mut actual = actual;
    actual.join(&Value::function(target));
    let arguments = [actual];
    let callee = Value::default();
    let state = State::default();
    let mut flow = Flow::new(&caller, &table, None);
    let symbolic = value::inputs(callable, Local(1), callable, &[], &table);
    let bound = flow.bind(
        &symbolic,
        &wrapper,
        CallActuals {
            callee: &callee,
            arguments: &arguments,
            ty: callable,
            point,
            state: &state,
            aliases: None,
        },
    );
    let empty = Boundary::invoke(callable, []);
    flow.record_obligation(point, empty.clone(), &bound);
    assert!(flow.obligations[&(point, empty)]
        .frame_sources(caller.def_id)
        .any(|root| root == point));
    let checked = Boundary::invoke(callable, [target]);
    flow.record_obligation(point, checked.clone(), &bound);
    assert!(!flow.obligations.contains_key(&(point, checked)));
    let mut joined = Summary::default();
    for witness in [vec![], vec![target]] {
        let incoming = Summary {
            obligations: FxHashMap::from_iter([(
                Boundary::invoke(callable, witness),
                symbolic.clone(),
            )]),
            ..Summary::default()
        };
        assert!(joined.join(&incoming));
    }
    assert_eq!(
        joined.obligations.len(),
        2,
        "different sites cannot union their discharge evidence"
    );
    assert_eq!(
        Boundary::invoke(callable, [target, DefId::new(9883), target]),
        Boundary::invoke(callable, [DefId::new(9883), target])
    );
}

#[test]
fn validation_results_survive_binding_dereference_reborrow_and_taint() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: int,
        is_async: false,
    });
    let reference = table.intern(Ty::Ref {
        inner: callable,
        mutable: false,
        lifetime: spark_types::LifetimeId::ANONYMOUS,
    });
    let optional = table.intern(Ty::Optional(reference));
    let function = MirFunction {
        def_id: DefId::new(9884),
        param_count: 0,
        return_type: callable,
        locals: vec![],
        blocks: vec![],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let point = Point::new(BlockId::ENTRY, 0);
    let state = State::default();
    let mut flow = Flow::new(&function, &table, None);
    let hole = value::validation_hole(reference, &table)
        .sourced(&Storage::Logical(Local(1)))
        .prefixed(Slot::Optional, optional, &table)
        .project(&Slot::Optional);
    assert!(hole.all_sources().is_empty());
    let callee = Value::default();
    let bound = flow.bind(
        &hole,
        &function,
        CallActuals {
            callee: &callee,
            arguments: &[],
            ty: reference,
            point,
            state: &state,
            aliases: None,
        },
    );
    assert_eq!(hole, bound);
    let loaded = flow.read_pointee(&bound, callable, &state);
    assert!(loaded.callees[&Vec::new()].open);
    assert!(!loaded.callees[&Vec::new()].unresolved);
    let derived = Value::reference(Origin::Loan(point)).with_metadata(&Leaf {
        parents: std::iter::once(Origin::ValidationHole).collect(),
        ..Leaf::default()
    });
    let exported = export_value(&derived, function.def_id);
    let imported = flow.bind(
        &exported,
        &function,
        CallActuals {
            callee: &callee,
            arguments: &[],
            ty: reference,
            point,
            state: &state,
            aliases: None,
        },
    );
    let through_summary = flow.read_pointee(&imported, callable, &state);
    assert!(
        through_summary
            .callees
            .get(&Vec::new())
            .is_some_and(|targets| targets.open && !targets.unresolved),
        "reborrowed deferred data must survive export and binding"
    );
    let aliases = Aliases::default();
    let residual = flow.residual_reference(
        &derived,
        &Port {
            kind: crate::provenance::aliases::StorageKind::Reference,
            channel_capture: None,
            queue: None,
            capture: None,
            input: Local(1),
            selection: vec![Slot::Element],
            pointee: callable,
        },
        &function,
        CallActuals {
            callee: &callee,
            arguments: &[],
            ty: reference,
            point,
            state: &state,
            aliases: Some(&aliases),
        },
    );
    assert!(
        flow.read_pointee(&residual, callable, &state)
            .callees
            .get(&Vec::new())
            .is_some_and(|targets| targets.open),
        "residual filtering must retain the deferred alternative"
    );
    let real_and_hole = derived.clone().with_metadata(&Leaf {
        endpoints: std::iter::once(endpoint(function.def_id, point, callable)).collect(),
        ..Leaf::default()
    });
    let exported_mixed = export_value(&real_and_hole, function.def_id);
    assert!(exported_mixed
        .leaves
        .keys()
        .any(|(_, origin, _)| *origin == Origin::ValidationHole));
    assert!(exported_mixed
        .frame_sources(function.def_id)
        .any(|root| root == point));
    let cast = value::tainted_from(reference, &derived, &table);
    let loaded = flow.read_pointee(&cast, callable, &state);
    assert!(loaded.callees[&Vec::new()].open);
    assert!(!loaded.callees[&Vec::new()].unresolved);
    let mut mixed = loaded;
    mixed.join(&value::tainted(
        callable,
        &std::iter::once(Origin::Unproven(point)).collect(),
        &table,
    ));
    assert!(mixed.callees[&Vec::new()].open);
    assert!(mixed.callees[&Vec::new()].unresolved);
    mixed.join(&Value::reference(Origin::Loan(point)).with_metadata(&Leaf {
        endpoints: FxHashSet::from_iter([endpoint(function.def_id, point, int)]),
        ..Leaf::default()
    }));
    assert!(mixed
        .frame_sources(function.def_id)
        .any(|root| root == point));
}

#[test]
fn opaque_runtime_results_do_not_inherit_open_from_arguments() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: int,
        is_async: false,
    });
    let reference = table.intern(Ty::Ref {
        inner: callable,
        mutable: false,
        lifetime: spark_types::LifetimeId::ANONYMOUS,
    });
    let function = MirFunction {
        def_id: DefId::new(9885),
        param_count: 1,
        return_type: reference,
        locals: (0..2)
            .map(|index| spark_mir::LocalDecl {
                local: Local(index),
                ty: reference,
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
    let point = Point::new(BlockId::ENTRY, 0);
    let state = State::default();
    let mut flow = Flow::new(&function, &table, None);
    let callee = Value::function(DefId::new(9886));
    let arguments = [value::validation_hole(reference, &table)];
    let result = flow.opaque_result(
        &[Operand::Constant(spark_mir::Constant::Void)],
        CallActuals {
            callee: &callee,
            arguments: &arguments,
            ty: reference,
            point,
            state: &state,
            aliases: None,
        },
    );
    let loaded = flow.read_pointee(&result, callable, &state);
    let targets = &loaded.callees[&Vec::new()];
    assert!(targets.unresolved);
    assert!(
        !targets.open,
        "a foreign call's result is a real runtime alternative"
    );
    let symbolic = [value::inputs(reference, Local(1), reference, &[], &table)];
    let result = flow.opaque_result(
        &[Operand::Constant(spark_mir::Constant::Void)],
        CallActuals {
            callee: &callee,
            arguments: &symbolic,
            ty: reference,
            point,
            state: &state,
            aliases: None,
        },
    );
    let summary = export_value(&result, function.def_id);
    let bound = flow.bind(
        &summary,
        &function,
        CallActuals {
            callee: &callee,
            arguments: &arguments,
            ty: reference,
            point,
            state: &state,
            aliases: None,
        },
    );
    let loaded = flow.read_pointee(&bound, callable, &state);
    let targets = &loaded.callees[&Vec::new()];
    assert!(
        targets.unresolved && !targets.open,
        "binding a hole argument cannot turn a foreign result into validation data"
    );
}

#[test]
fn known_function_placeholders_do_not_erase_widened_reference_captures() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let string = table.intern(Ty::String);
    let reference = table.intern(Ty::Ref {
        inner: string,
        mutable: false,
        lifetime: spark_types::LifetimeId::ANONYMOUS,
    });
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: int,
        is_async: false,
    });
    let outer = DefId::new(9896);
    let inner = DefId::new(9897);
    let schema = |def, capture_ty| MirFunction {
        def_id: def,
        param_count: 0,
        return_type: int,
        locals: vec![],
        blocks: vec![],
        captures: vec![(DefId::new(9898), capture_ty)],
        is_async: false,
        span: Span::DUMMY,
    };
    let caller = MirFunction {
        def_id: DefId::new(9899),
        captures: vec![],
        ..schema(DefId::new(9899), callable)
    };
    for captured in [string, reference] {
        let mir = Mir {
            functions: std::collections::HashMap::from([
                (outer, schema(outer, callable)),
                (inner, schema(inner, captured)),
            ]),
        };
        let summaries = Summaries::default();
        let ports = Ports::default();
        let mut flow = Flow::new(
            &caller,
            &table,
            Some(Context {
                proofs: None,
                definitions: None,
                mir: &mir,
                summaries: &summaries,
                ports: &ports,
            }),
        );
        flow.callables = super::super::callbacks::Inputs::invocation();
        flow.callables.slots.insert(
            (Local(1), vec![], callable, None),
            super::super::callbacks::Targets::function(outer),
        );
        flow.callables.slots.insert(
            (
                Local(1),
                vec![Slot::Capture(outer, 0, callable)],
                callable,
                None,
            ),
            super::super::callbacks::Targets::function(inner),
        );
        let value = flow.symbolic_input(callable, Local(1), callable, &[]);
        assert_eq!(!value.leaves.is_empty(), captured == reference, "known owned captures need no placeholder, while real references survive the same normalized path");
        if captured == reference {
            assert!(value
                .leaves
                .keys()
                .any(|(path, _, _)| path == &[Slot::Capture(outer, 0, callable), Slot::Tail]));
        }
    }
}
