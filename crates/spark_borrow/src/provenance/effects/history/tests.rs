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

#[test]
fn input_residual_binding_keeps_callbacks_with_their_current_actor() {
    use crate::provenance::tasks::{
        self,
        residual::{Activity, Key, Visibility},
        views::{Subject, View},
    };
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: int });
    let callback_ty = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let caller = function(19100, future);
    let callee = function(19101, future);
    let point = Point::new(BlockId::ENTRY, 3);
    let mut state = State::default();
    let mut actual = Value::default();
    for index in 0..2 {
        let issued = Point::new(BlockId::ENTRY, index);
        let (target, mut current) = tasks::birth(caller.def_id, issued, future);
        let callback = Value::reference(Origin::Loan(issued));
        let mut view = View::pending(callback.clone(), callback_ty);
        view.memory.may_write = true;
        current.task_views.entry(Vec::new()).or_default().insert(
            Subject {
                target: target.clone(),
                capture: None,
            },
            view,
        );
        actual.join(&current);
        state.tasks.insert(
            target,
            tasks::Record {
                callback,
                callback_ty,
            },
        );
    }
    let mut symbolic = Value::default();
    tasks::seed_inputs(&mut symbolic, future, Local(1), future, &[], &table);
    let handle = symbolic.futures[&Vec::new()].iter().next().unwrap();
    let key = std::sync::Arc::new(Key {
        site: OpaqueSite {
            function: callee.def_id,
            point,
        },
        role: Vec::new(),
        ty: future,
        actor: Subject::from(handle),
    });
    let mut flow = Flow::new(&caller, &table, None);
    let callee_value = Value::default();
    let bound = flow.bind_residual_activity(
        &key,
        &Activity {
            value: symbolic,
            ..Activity::default()
        },
        &callee,
        CallActuals {
            callee: &callee_value,
            arguments: &[actual],
            ty: future,
            point,
            read: ReadView::current(&state),
            instantiation_site: None,
            frame_caller: Some(caller.def_id),
            aliases: None,
        },
        &Visibility::Continuation(tasks::proofs::EventFence::default()),
    );
    assert_eq!(bound.len(), 2);
    for residual in bound {
        assert_eq!(
            residual.callbacks.len(),
            1,
            "a contribution must not import another actor's callback"
        );
        assert_eq!(
            residual.callbacks[0].1.origins(),
            state.tasks[&residual.key.actor.target].callback.origins()
        );
    }
}
