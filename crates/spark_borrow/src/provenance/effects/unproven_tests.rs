// TAG [SPARK-114]

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
    incoming.unproven_stores.insert(int, value.clone());
    assert!(summary.join(&incoming));
    assert!(!summary.join(&incoming));
    value = value.with_metadata(&Leaf {
        endpoints: FxHashSet::from_iter([endpoint(DefId::new(9850), point, int)]),
        ..Default::default()
    });
    incoming.unproven_stores.insert(int, value.clone());
    assert!(
        summary.join(&incoming),
        "new endpoint must wake dependent callers"
    );
    assert_eq!(summary.unproven_stores[&int], value);
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
    flow.record_unproven_store(point, int, &value);
    assert_eq!(flow.transfers[&point].seeds, value.all_sources());
    assert!(!flow.transfers[&point].seeds.is_empty());
    assert!(flow.transfers[&point].definitions.is_empty());
    assert_eq!(flow.unproven_stores[&(point, int)], value);
    flow.record_unproven_store(point, TypeId::ERROR, &Value::function(DefId::new(9852)));
    assert_eq!(
        flow.unproven_stores.len(),
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
