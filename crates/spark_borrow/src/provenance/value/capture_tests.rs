// TAG [SPARK-114]

use super::*;
use crate::facts::Point;
use spark_mir::{BlockId, Local};
use spark_types::{IntWidth, LifetimeId};

fn reference(local: u32, int: TypeId) -> Value {
    Value::reference(Origin::Loan(Point::new(BlockId::ENTRY, 0))).with_metadata(&Leaf {
        endpoints: std::iter::once(Endpoint::Frame {
            function: DefId::new(9900),
            root: Point::new(BlockId::ENTRY, 0),
            local: Local(local),
            root_ty: int,
            pointee: int,
            location: vec![],
        })
        .collect(),
        ..Leaf::default()
    })
}

fn fixture() -> (TypeTable, TypeId, TypeId, ReferenceCapture) {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let ty = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let callable = table.intern(Ty::Function {
        params: vec![],
        ret: int,
        is_async: false,
    });
    (
        table,
        int,
        callable,
        ReferenceCapture {
            owner: DefId::new(9901),
            index: 0,
            ty,
        },
    )
}

fn targets(value: &Value) -> FxHashSet<Local> {
    value
        .leaves
        .values()
        .flat_map(|leaf| leaf.endpoints.iter())
        .filter_map(|endpoint| match endpoint {
            Endpoint::Frame { local, .. } => Some(*local),
            Endpoint::Input(_) => None,
        })
        .collect()
}

#[test]
fn widened_captures_keep_endpoint_and_demand_contributions_separate() {
    let (table, int, callable, first) = fixture();
    let second = ReferenceCapture { index: 1, ..first };
    let wrap = |mut value: Value| {
        for _ in 0..32 {
            value = value.prefixed(
                Slot::Capture(DefId::new(9902), 0, callable),
                callable,
                &table,
            );
        }
        value
    };
    let mut joined = wrap(reference(1, int).with_reference_capture(first));
    assert!(joined.join(&wrap(reference(2, int).with_reference_capture(second))));
    assert!(!joined.join(&joined.clone()));
    assert!(joined.leaves.keys().all(|(path, _, _)| path.len() <= 2));
    let joined = joined.sourced(&Storage::Logical(Local(3)));
    for (capture, expected) in [(first, Local(1)), (second, Local(2))] {
        let selected = joined.select_reference_capture(capture);
        assert_eq!(targets(&selected), FxHashSet::from_iter([expected]));
        assert_eq!(selected.all_sources().len(), 1);
        assert!(selected
            .all_sources()
            .iter()
            .all(|source| matches!(source, super::super::demand::Source::Current(atom) | super::super::demand::Source::Entry(atom) if atom.capture == Some(capture))));
    }
    let tainted = tainted_from(first.ty, &joined, &table);
    assert_eq!(
        targets(&tainted.select_reference_capture(first)),
        FxHashSet::from_iter([Local(1)])
    );
    assert_eq!(
        targets(&tainted.select_reference_capture(second)),
        FxHashSet::from_iter([Local(2)])
    );
}

#[test]
fn repeated_capture_instances_and_unknown_alternatives_are_not_filtered_away() {
    let (_, int, _, capture) = fixture();
    let mut joined = reference(1, int).with_reference_capture(capture);
    joined.join(&reference(2, int).with_reference_capture(capture));
    joined.join(&reference(3, int));
    assert_eq!(
        targets(&joined.select_reference_capture(capture)),
        FxHashSet::from_iter([Local(1), Local(2), Local(3)])
    );
}

#[test]
fn selecting_a_pointer_does_not_copy_callable_targets_from_its_enclosing_environment() {
    let (_, int, _, capture) = fixture();
    let mut environment = reference(1, int).with_reference_capture(capture);
    environment.callees.insert(
        vec![Slot::Tail],
        super::super::callbacks::Targets::function(DefId::new(9903)),
    );
    let pointer = environment.select_reference_capture(capture);
    assert_eq!(targets(&pointer), FxHashSet::from_iter([Local(1)]));
    assert!(pointer.callees.is_empty());
    assert!(!environment.callees.is_empty());
}
