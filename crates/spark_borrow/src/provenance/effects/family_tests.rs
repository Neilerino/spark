// TAG [SPARK-114]

use super::*;
use spark_mir::LocalDecl;
use spark_span::Span;
use spark_types::{IntWidth, LifetimeId, Ty};

#[test]
fn residual_binding_filters_endpoints_without_erasing_opaque_or_ambiguous_alternatives() {
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
        mutable: true,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let list = table.intern(Ty::List(map_ref));
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
                span: Span::DUMMY,
            })
            .collect()
    };
    let caller = MirFunction {
        def_id: DefId::new(9842),
        param_count: 0,
        return_type: map,
        locals: locals(&[map, map, map, map_ref]),
        blocks: vec![],
        captures: vec![],
        is_async: false,
        span: Span::DUMMY,
    };
    let callee = MirFunction {
        def_id: DefId::new(9843),
        param_count: 2,
        locals: locals(&[map, list, map_ref]),
        ..caller.clone()
    };
    let point = Point::new(BlockId::ENTRY, 0);
    let endpoint = |local| Endpoint::Frame {
        lifetime: crate::provenance::address::FrameLifetime::Local,
        function: caller.def_id,
        root: point,
        local: Local(local),
        root_ty: map,
        pointee: map,
        location: Vec::new(),
    };
    let first = endpoint(1);
    let second = endpoint(2);
    let pointer = |endpoints| {
        Value::reference(Origin::Loan(point)).with_metadata(&super::super::value::Leaf {
            endpoints,
            ..Default::default()
        })
    };
    let family = Port {
        kind: crate::provenance::aliases::StorageKind::Reference,
        channel_capture: None,
        queue: None,
        capture: None,
        input: Local(1),
        selection: vec![Slot::Element],
        pointee: map,
    };
    let root = Port::root(Local(2), map);
    let aliases = Aliases::default()
        .body_variants(&callee, &[family.clone(), root], &table)
        .remove(0);
    let state = State::default();
    let flow = Flow::new(&caller, &table, None);
    for opaque in [false, true] {
        let mut selected = pointer(FxHashSet::from_iter([first.clone(), second.clone()]));
        if opaque {
            for leaf in selected.leaves.values_mut() {
                leaf.opaque.insert(super::super::address::OpaqueSite {
                    function: caller.def_id,
                    point,
                });
            }
        }
        selected = selected.sourced(&super::super::demand::Storage::Logical(Local(3)));
        let arguments = [
            selected.prefixed(Slot::Element, list, &table),
            pointer(FxHashSet::from_iter([first.clone()])),
        ];
        let target = Value::default();
        let actuals = CallActuals {
            callee: &target,
            arguments: &arguments,
            ty: map_ref,
            point,
            read: crate::provenance::observations::ReadView::current(&state),
            instantiation_site: None,
            frame_caller: None,
            aliases: Some(&aliases),
        };
        let filtered = flow.residual_reference(&selected, &family, &callee, actuals);
        assert_eq!(filtered.leaves.len(), 1);
        let leaf = filtered.leaves.values().next().unwrap();
        assert_eq!(leaf.endpoints, FxHashSet::from_iter([second.clone()]));
        assert_eq!(!leaf.opaque.is_empty(), opaque);
        assert_eq!(
            filtered.sources, selected.sources,
            "remaining alternatives retain their source support"
        );
    }
    let selected = pointer(FxHashSet::from_iter([first.clone(), second.clone()]));
    let arguments = [
        selected.prefixed(Slot::Element, list, &table),
        selected.clone(),
    ];
    let target = Value::default();
    let actuals = CallActuals {
        callee: &target,
        arguments: &arguments,
        ty: map_ref,
        point,
        read: crate::provenance::observations::ReadView::current(&state),
        instantiation_site: None,
        frame_caller: None,
        aliases: Some(&aliases),
    };
    let filtered = flow.residual_reference(&selected, &family, &callee, actuals);
    assert_eq!(
        filtered.leaves.values().next().unwrap().endpoints.len(),
        2,
        "an alternative endpoint set does not prove an excluded singleton"
    );
}
