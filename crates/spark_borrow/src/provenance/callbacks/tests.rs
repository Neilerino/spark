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
        (Local(1), vec![Slot::Tuple(0)], TypeId::ERROR),
        Targets::function(DefId::new(9861)),
    );
    let second = (
        (Local(2), vec![], TypeId::ERROR),
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
