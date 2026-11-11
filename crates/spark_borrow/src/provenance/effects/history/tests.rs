// TAG [SPARK-114]

use super::*;
use crate::provenance::{
    address::OpaqueSite, aliases::Port, observations::ReadView, value::Leaf, Origin, Point, State,
    Value,
};
use spark_mir::{BlockId, Local, LocalDecl};
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::{IntWidth, LifetimeId, Ty, TypeId, TypeTable};

fn function(id: u32, ty: TypeId) -> MirFunction {
    MirFunction {
        def_id: DefId::new(id),
        param_count: 1,
        return_type: ty,
        locals: [ty, ty]
            .into_iter()
            .enumerate()
            .map(|(index, ty)| LocalDecl {
                local: Local(index as u32),
                ty,
                name: None,
                is_user_variable: false,
                mutable: true,
                span: Span::DUMMY,
            })
            .collect(),
        blocks: Vec::new(),
        captures: Vec::new(),
        is_async: false,
        span: Span::DUMMY,
    }
}

#[test]
fn binding_known_and_opaque_support_keeps_both_alternatives() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let caller = function(9993, int);
    let callee = function(9994, reference);
    let point = Point::new(BlockId::ENTRY, 0);
    let pointer = Value::reference(Origin::Loan(point)).with_metadata(&Leaf {
        endpoints: FxHashSet::from_iter([Endpoint::Frame {
            lifetime: FrameLifetime::Local,
            function: caller.def_id,
            root: point,
            local: Local(1),
            root_ty: int,
            pointee: int,
            location: Vec::new(),
        }]),
        opaque: FxHashSet::from_iter([OpaqueSite {
            function: caller.def_id,
            point: Point::new(BlockId::ENTRY, 1),
        }]),
        ..Leaf::default()
    });
    let flow = Flow::new(&caller, &table, None);
    let state = State::default();
    let callee_value = Value::default();
    let samples = flow.bind_observation_sample(
        &Sample {
            memory: Memory::External(Port::root(Local(1), int)),
            path: Vec::new(),
            ty: int,
        },
        &callee,
        CallActuals {
            callee: &callee_value,
            arguments: &[pointer],
            ty: int,
            point,
            instantiation_site: None,
            frame_caller: Some(caller.def_id),
            read: ReadView::current(&state),
            aliases: None,
        },
    );
    assert!(samples
        .iter()
        .any(|sample| matches!(sample.memory, Memory::Frame {
        scope: FrameScope::Current, function, ..
    } if function == caller.def_id)));
    assert!(
        samples
            .iter()
            .any(|sample| matches!(sample.memory, Memory::Unavailable(_))),
        "a concrete endpoint cannot suppress the opaque support alternative"
    );
}

#[test]
fn imported_frame_support_does_not_alias_a_recursive_callers_current_frame() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let reference = table.intern(Ty::Ref {
        inner: int,
        mutable: false,
        lifetime: LifetimeId::ANONYMOUS,
    });
    let caller = function(9995, reference);
    let point = Point::new(BlockId::ENTRY, 0);
    let mut state = State::default();
    state.set(Local(1), Value::reference(Origin::Loan(point)));
    let flow = Flow::new(&caller, &table, None);
    let sample = Sample {
        memory: Memory::Frame {
            scope: FrameScope::Current,
            function: caller.def_id,
            local: Local(1),
            ty: reference,
        },
        path: Vec::new(),
        ty: reference,
    };
    assert!(!flow
        .read_memory(sample.clone(), ReadView::current(&state))
        .is_empty());
    let callee_value = Value::default();
    let bound = flow.bind_observation_sample(
        &sample,
        &caller,
        CallActuals {
            callee: &callee_value,
            arguments: &[],
            ty: reference,
            point,
            instantiation_site: None,
            frame_caller: Some(caller.def_id),
            read: ReadView::current(&state),
            aliases: None,
        },
    );
    assert_eq!(bound.len(), 1);
    let imported = bound.into_iter().next().unwrap();
    assert!(matches!(
        imported.memory,
        Memory::Frame {
            scope: FrameScope::Foreign,
            ..
        }
    ));
    assert!(flow
        .read_memory(imported, ReadView::current(&state))
        .is_empty());
}

#[test]
fn binding_known_and_unknown_queue_support_keeps_the_unknown_alternative() {
    use crate::provenance::channels::{Handle, Target};

    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let channel = table.intern(Ty::Channel { inner: int });
    let caller = function(9996, channel);
    let callee = function(9997, channel);
    let point = Point::new(BlockId::ENTRY, 0);
    let site = OpaqueSite {
        function: caller.def_id,
        point,
    };
    let mut argument = Value::default();
    argument.queues.insert(
        Vec::new(),
        FxHashSet::from_iter([
            Handle::new(Target::Allocation {
                ty: channel,
                function: caller.def_id,
                point,
                constructor: None,
            }),
            Handle::new(Target::Unknown {
                site,
                ty: channel,
                constructor: None,
            }),
        ]),
    );
    let flow = Flow::new(&caller, &table, None);
    let state = State::default();
    let callee_value = Value::default();
    let samples = flow.bind_observation_sample(
        &Sample {
            memory: Memory::Queue(
                Target::Input {
                    capture: None,
                    queue: None,
                    local: Local(1),
                    path: Vec::new(),
                    ty: channel,
                },
                int,
            ),
            path: Vec::new(),
            ty: int,
        },
        &callee,
        CallActuals {
            callee: &callee_value,
            arguments: &[argument],
            ty: int,
            point,
            instantiation_site: None,
            frame_caller: Some(caller.def_id),
            read: ReadView::current(&state),
            aliases: None,
        },
    );
    assert!(samples
        .iter()
        .any(|sample| matches!(sample.memory, Memory::Queue(Target::Allocation { .. }, _))));
    assert!(samples
        .iter()
        .any(|sample| matches!(sample.memory, Memory::Unavailable(_))));
}
