// TAG [SPARK-114]

use super::*;
use spark_types::{IntWidth, TypeTable};

#[test]
fn known_and_unresolved_alternatives_survive_collection_projection() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let function = table.intern(Ty::Function {
        params: vec![],
        ret: int,
        is_async: false,
    });
    let list = table.intern(Ty::List(function));
    let mut value = value::tainted(
        function,
        &std::iter::once(super::super::Origin::Unproven(crate::facts::Point::new(
            spark_mir::BlockId::ENTRY,
            0,
        )))
        .collect(),
        &table,
    );
    assert!(value.join(&Value::function(DefId::new(9860))));
    let selected = value
        .prefixed(Slot::Element, list, &table)
        .project(&Slot::Element);
    let targets = &selected.callees[&Vec::new()];
    assert!(targets.unresolved);
    assert_eq!(targets.known, FxHashSet::from_iter([DefId::new(9860)]));
    assert!(!value.join(&selected));
}

#[test]
fn callback_context_identity_is_independent_of_discovery_order() {
    let first = (
        (Local(1), vec![Slot::Tuple(0)], TypeId::ERROR, None),
        Targets::function(DefId::new(9861)),
    );
    let second = (
        (Local(2), vec![], TypeId::ERROR, None),
        Targets {
            unresolved: true,
            ..Targets::default()
        },
    );
    let a = Inputs {
        slots: FxHashMap::from_iter([first.clone(), second.clone()]),
        ..Inputs::default()
    };
    let b = Inputs {
        slots: FxHashMap::from_iter([second, first]),
        ..Inputs::default()
    };
    let cache = FxHashMap::from_iter([(a.clone(), 7)]);
    assert_eq!(a, b);
    assert_eq!(cache.get(&b), Some(&7));
}

#[test]
fn target_only_growth_and_uncertainty_wake_reaching_state() {
    let mut first = super::super::State::default();
    first.set(Local(1), Value::function(DefId::new(9862)));
    let mut second = super::super::State::default();
    second.set(Local(1), Value::function(DefId::new(9863)));
    assert!(first.join(&second));
    assert!(!first.join(&second));
    let mut unknown = Value::default();
    unknown.callees.insert(
        vec![],
        Targets {
            unresolved: true,
            ..Targets::default()
        },
    );
    second.set(Local(1), unknown);
    assert!(first.join(&second));
    assert!(!first.join(&second));
    assert!(first.value(Local(1)).leaves.is_empty());
    assert_eq!(first.value(Local(1)).callees[&Vec::new()].known.len(), 2);
    assert!(first.value(Local(1)).callees[&Vec::new()].unresolved);
}

#[test]
fn universal_and_invocation_defaults_are_distinct_contexts() {
    let universal = Inputs::default();
    let invocation = Inputs::invocation();
    assert_ne!(universal, invocation);
    let cache = FxHashMap::from_iter([(universal, 1), (invocation, 2)]);
    assert_eq!(cache.len(), 2);
    let mut open = Targets {
        open: true,
        ..Targets::default()
    };
    assert!(open.join(&Targets {
        unresolved: true,
        ..Targets::default()
    }));
    assert!(open.open && open.unresolved);
}

#[test]
fn tail_target_queries_are_typed_and_exact_roots_take_precedence() {
    use spark_mir::MirFunction;
    use spark_span::Span;
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let string = table.intern(Ty::String);
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: int,
        is_async: false,
    });
    let different_arity = table.intern(Ty::Function {
        params: vec![int],
        ret: int,
        is_async: false,
    });
    let different_return = table.intern(Ty::Function {
        params: vec![],
        ret: string,
        is_async: false,
    });
    let matching = DefId::new(9890);
    let arity = DefId::new(9891);
    let returned = DefId::new(9892);
    for (def, ty) in [
        (matching, callable),
        (arity, different_arity),
        (returned, different_return),
    ] {
        table.set_def_type(def, ty);
    }
    let function = MirFunction {
        def_id: DefId::new(9893),
        param_count: 0,
        return_type: int,
        locals: vec![],
        blocks: vec![],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let flow = Flow::new(&function, &table, None);
    let mut value = Value::default();
    value.callees.insert(
        vec![Slot::Tail],
        Targets {
            known: [matching, arity, returned].into_iter().collect(),
            open: true,
            unresolved: false,
        },
    );
    let extracted = value.capture(DefId::new(9894), 0);
    let targets = flow.callee_targets(&extracted, callable);
    assert_eq!(targets.known, FxHashSet::from_iter([matching]));
    assert!(targets.open && !targets.unresolved);
    value.callees.insert(vec![], Targets::function(arity));
    assert_eq!(
        flow.callee_targets(&value, different_arity).known,
        FxHashSet::from_iter([arity]),
        "contained functions are not alternatives for an exact root"
    );
    let mut uncertain = extracted;
    uncertain
        .callees
        .get_mut(&vec![Slot::Tail])
        .unwrap()
        .known
        .insert(DefId::new(9895));
    assert!(flow.callee_targets(&uncertain, callable).unresolved);
    let mismatched = Value::function(arity).prefixed(Slot::Tail, callable, &table);
    assert!(flow.callee_targets(&mismatched, callable).unresolved);
}

fn source_function(ty: TypeId) -> spark_mir::MirFunction {
    spark_mir::MirFunction {
        def_id: DefId::new(9930),
        param_count: 1,
        return_type: ty,
        locals: [ty, ty]
            .into_iter()
            .enumerate()
            .map(|(index, ty)| spark_mir::LocalDecl {
                local: Local(index as u32),
                ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: spark_span::Span::DUMMY,
            })
            .collect(),
        blocks: vec![],
        captures: vec![],
        is_async: false,
        span: spark_span::Span::DUMMY,
    }
}

#[test]
fn callable_facts_only_materialize_owned_function_locations() {
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
    let pointer = table.intern(Ty::Ref {
        inner: callable,
        mutable: false,
        lifetime: spark_types::LifetimeId::ANONYMOUS,
    });
    let data = table.intern(Ty::Map(int, reference));
    let functions = table.intern(Ty::Map(int, callable));
    let target = DefId::new(9931);
    for (ty, path, owned) in [
        (data, vec![], false),
        (pointer, vec![Slot::Deref], false),
        (functions, vec![Slot::Value], true),
    ] {
        let function = source_function(ty);
        let mut flow = Flow::new(&function, &table, None);
        flow.callables = Inputs::invocation();
        flow.callables.slots.insert(
            (Local(1), path.clone(), callable, None),
            Targets::function(target),
        );
        let value = flow.symbolic_input(ty, Local(1), ty, &[]);
        assert_eq!(value.callees.contains_key(&path), owned);
        if !owned {
            assert!(value.callees.is_empty());
        }
        if ty == pointer {
            let address = super::super::address::Selector {
                channel_capture: None,
                queue: None,
                input: Local(1),
                selection: vec![],
                capture: None,
                expected: Some(callable),
                intent: super::super::address::Intent::Address(super::super::address::TypedPath {
                    kind: super::super::address::DerefKind::All,
                    pointee: callable,
                    projection: vec![],
                }),
            };
            let pointed =
                flow.symbolic_pointee_input(callable, pointer, &[Slot::Deref], &address, None);
            assert_eq!(
                pointed.callees[&vec![]].known,
                FxHashSet::from_iter([target])
            );
        }
    }
}

#[test]
fn widened_pointees_do_not_reuse_surrounding_function_value_targets() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: int,
        is_async: false,
    });
    let function = source_function(callable);
    let mut flow = Flow::new(&function, &table, None);
    flow.callables = Inputs::invocation();
    let target = DefId::new(9932);
    flow.callables.slots.insert(
        (Local(1), vec![Slot::Tail], callable, None),
        Targets::function(target),
    );
    let owned = flow.symbolic_input(callable, Local(1), callable, &[Slot::Tail]);
    assert_eq!(owned.callees[&vec![]].known, FxHashSet::from_iter([target]));
    let address = super::super::address::Selector {
        channel_capture: None,
        queue: None,
        input: Local(1),
        selection: vec![Slot::Tail],
        capture: None,
        expected: Some(callable),
        intent: super::super::address::Intent::Address(super::super::address::TypedPath {
            kind: super::super::address::DerefKind::All,
            pointee: callable,
            projection: vec![],
        }),
    };
    let pointed = flow.symbolic_pointee_input(callable, callable, &[Slot::Tail], &address, None);
    let targets = &pointed.callees[&vec![]];
    assert!(targets.known.is_empty());
    assert!(targets.unresolved && !targets.open);
}

#[test]
fn captured_pointee_facts_do_not_collide_with_surrounding_function_facts() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: int,
        is_async: false,
    });
    let pointer = table.intern(Ty::Ref {
        inner: callable,
        mutable: false,
        lifetime: spark_types::LifetimeId::ANONYMOUS,
    });
    let capture = value::ReferenceCapture {
        owner: DefId::new(9933),
        index: 0,
        ty: pointer,
    };
    let outer = DefId::new(9934);
    let inner = DefId::new(9935);
    let function = source_function(callable);
    let mut flow = Flow::new(&function, &table, None);
    flow.callables = Inputs::invocation();
    flow.callables
        .slots
        .insert((Local(1), vec![], callable, None), Targets::function(outer));
    flow.callables.slots.insert(
        (Local(1), vec![], callable, Some(capture)),
        Targets {
            open: true,
            ..Targets::function(inner)
        },
    );
    let surrounding = flow.symbolic_input(callable, Local(1), callable, &[]);
    assert_eq!(
        surrounding.callees[&vec![]].known,
        FxHashSet::from_iter([outer])
    );
    assert!(!surrounding.callees[&vec![]].open);
    let address = super::super::address::Selector {
        channel_capture: None,
        queue: None,
        input: Local(1),
        selection: vec![Slot::Tail],
        capture: Some(capture),
        expected: Some(callable),
        intent: super::super::address::Intent::Address(super::super::address::TypedPath {
            kind: super::super::address::DerefKind::All,
            pointee: callable,
            projection: vec![],
        }),
    };
    let pointed = flow.symbolic_pointee_input(callable, callable, &[Slot::Tail], &address, None);
    let targets = &pointed.callees[&vec![]];
    assert_eq!(targets.known, FxHashSet::from_iter([inner]));
    assert!(targets.open && !targets.unresolved);
}
