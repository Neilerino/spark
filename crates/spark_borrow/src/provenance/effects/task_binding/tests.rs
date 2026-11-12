// TAG [SPARK-114]

use super::*;
use spark_mir::{BlockId, LocalDecl};
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::{IntWidth, TypeTable};

#[test]
fn widened_future_input_selections_keep_type_and_capture_roles_separate() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let string = table.intern(Ty::String);
    let first = table.intern(Ty::Future { inner: int });
    let other = table.intern(Ty::Future { inner: string });
    let callable = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let function = MirFunction {
        def_id: DefId::new(9920),
        param_count: 1,
        return_type: int,
        locals: [int, callable]
            .into_iter()
            .enumerate()
            .map(|(index, ty)| LocalDecl {
                local: super::super::Local(index as u32),
                ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            })
            .collect(),
        blocks: Vec::new(),
        captures: Vec::new(),
        is_async: false,
        span: Span::DUMMY,
    };
    let missing_ty = table.intern(Ty::Future { inner: callable });
    let flow = Flow::new(&function, &table, None);
    let source = super::super::super::tasks::Capture {
        owner: DefId::new(9921),
        index: 0,
        ty: first,
    };
    let mut argument = Value::default();
    for (index, ty, capture) in [
        (1, first, Some(source)),
        (
            2,
            first,
            Some(super::super::super::tasks::Capture { index: 1, ..source }),
        ),
        (3, other, None),
    ] {
        let (_, mut value) = super::super::super::tasks::birth(
            function.def_id,
            Point::new(BlockId::ENTRY, index),
            ty,
        );
        for handles in value.futures.values_mut() {
            *handles = handles
                .iter()
                .map(|h| Handle {
                    capture,
                    ..h.clone()
                })
                .collect();
        }
        argument.join(&value.prefixed(Slot::Tail, callable, &table));
    }
    let arguments = [argument];
    let state = super::super::super::State::default();
    let callee = Value::default();
    let point = Point::new(BlockId::ENTRY, 9);
    let actuals = CallActuals {
        callee_place: None,
        callee: &callee,
        arguments: &arguments,
        ty: first,
        point,
        read: crate::provenance::observations::ReadView::current(&state),
        instantiation_site: None,
        frame_caller: None,
        aliases: None,
    };
    let target = Target::Input {
        input: super::super::Local(1),
        path: vec![Slot::Tail],
        ty: first,
        capture: Some(source),
        queue: None,
    };
    let selected = flow.bind_task_target(&target, &function, actuals);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected.iter().next().unwrap().target.ty(), first);
    assert_eq!(selected.iter().next().unwrap().capture, Some(source));
    let missing = Target::Input {
        input: super::super::Local(1),
        path: vec![Slot::Tail],
        ty: missing_ty,
        capture: Some(source),
        queue: None,
    };
    let selected = flow.bind_task_target(&missing, &function, actuals);
    assert!(
        matches!(&selected.iter().next().unwrap().target,Target::Unknown{site,..} if site.function==function.def_id && site.point==point)
    );
}

mod resident_sources {
    // TAG [SPARK-114]

    use super::*;
    use crate::provenance::{
        demand::{Atom, Coordinate, Demand, Source, Storage},
        effects::{Context, Summary},
        tasks::{birth, views::View},
        Origin, State,
    };
    use spark_mir::{BlockId, Local, LocalDecl, Mir, Place};
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::{IntWidth, LifetimeId, TypeTable};

    fn function(def: DefId, locals: &[TypeId], returned: TypeId) -> MirFunction {
        MirFunction {
            def_id: def,
            param_count: 0,
            return_type: returned,
            locals: locals
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
            blocks: Vec::new(),
            captures: Vec::new(),
            is_async: false,
            span: Span::DUMMY,
        }
    }

    #[derive(Clone, Copy)]
    enum Root {
        Resident,
        Catalog,
        Residual,
    }

    struct Program {
        table: TypeTable,
        caller: MirFunction,
        mir: Mir,
        summaries: crate::provenance::effects::Summaries,
        ports: crate::provenance::effects::Ports,
        reference: TypeId,
        future: TypeId,
        callback_ty: TypeId,
        callback_def: DefId,
    }

    impl Program {
        fn new() -> Self {
            let mut table = TypeTable::new();
            let int = table.intern(Ty::Int(IntWidth::I64));
            let reference = table.intern(Ty::Ref {
                inner: int,
                mutable: false,
                lifetime: LifetimeId::ANONYMOUS,
            });
            let future = table.intern(Ty::Future { inner: reference });
            let callback_ty = table.intern(Ty::Function {
                params: Vec::new(),
                ret: reference,
                is_async: false,
            });
            let caller = function(DefId::new(9940), &[int, future, reference], int);
            let callback_def = DefId::new(9941);
            let mut callback = function(callback_def, &[reference, reference], reference);
            callback.captures.push((DefId::new(9942), reference));
            let mir = Mir {
                functions: std::collections::HashMap::from_iter([(callback_def, callback)]),
            };
            let summaries = FxHashMap::from_iter([(
                Key {
                    callables: crate::provenance::callbacks::Inputs::invocation(),
                    ..Key::distinct(callback_def)
                },
                Summary {
                    normal_return: true,
                    returned: crate::provenance::value::inputs(
                        reference,
                        Local(1),
                        reference,
                        &[],
                        &table,
                    ),
                    ..Summary::default()
                },
            )]);
            let ports = FxHashMap::from_iter([(callback_def, Vec::new())]);
            Self {
                table,
                caller,
                mir,
                summaries,
                ports,
                reference,
                future,
                callback_ty,
                callback_def,
            }
        }

        fn context(&self) -> Context<'_> {
            Context {
                mir: &self.mir,
                definitions: None,
                summaries: &self.summaries,
                proofs: None,
                ports: &self.ports,
            }
        }

        fn callback(&self, origin: Origin) -> Value {
            let mut callback = Value::function(self.callback_def);
            callback.join(&Value::reference(origin).prefixed(
                Slot::Capture(self.callback_def, 0, self.reference),
                self.callback_ty,
                &self.table,
            ));
            callback
        }

        fn state(&self, root: Root) -> (State, Storage, Target) {
            use crate::provenance::{
                address::OpaqueSite,
                tasks::{residual, views::Subject},
            };
            let (target, mut pending) = birth(
                self.caller.def_id,
                Point::new(BlockId::ENTRY, 1),
                self.future,
            );
            let callback = self.callback(Origin::Loan(Point::new(BlockId::ENTRY, 0)));
            let mut state = State::default();
            if matches!(root, Root::Catalog) {
                state.tasks.insert(
                    target.clone(),
                    Record {
                        callback,
                        callback_ty: self.callback_ty,
                    },
                );
            } else {
                pending.task_views.entry(Vec::new()).or_default().insert(
                    Subject {
                        target: target.clone(),
                        capture: None,
                    },
                    View::pending(callback, self.callback_ty),
                );
            }
            let storage = if matches!(root, Root::Residual) {
                let key = std::sync::Arc::new(residual::Key {
                    site: OpaqueSite {
                        function: self.caller.def_id,
                        point: Point::new(BlockId::ENTRY, 1),
                    },
                    role: Vec::new(),
                    ty: self.future,
                    actor: Subject {
                        target: target.clone(),
                        capture: None,
                    },
                });
                state.residual_callbacks.insert(
                    (key.clone(), self.callback_ty),
                    self.callback(Origin::Loan(Point::new(BlockId::ENTRY, 3))),
                );
                state.residuals.insert(key.clone(), pending);
                Storage::Residual(key)
            } else {
                state.set(Local(1), pending);
                Storage::Logical(Local(1))
            };
            (state, storage, target)
        }
    }

    fn check_task_query_sources(root: Root) {
        let program = Program::new();
        let (mut state, root_storage, target) = program.state(root);
        let point = Point::new(BlockId::ENTRY, 2);
        let origin = Origin::Loan(Point::new(BlockId::ENTRY, 0));
        let mut flow = Flow::new(&program.caller, &program.table, Some(program.context()));
        flow.ensure_transfer(point, &state);
        let correct_source = |storage: &Storage| {
            if matches!(root, Root::Catalog) {
                *storage == Storage::Task(target.clone(), program.callback_ty)
            } else {
                matches!(storage, Storage::CallbackNode(location) if location.root() == &root_storage)
            }
        };
        if !matches!(root, Root::Residual) {
            let TaskDispatch::Returns(outcomes) = flow
                .await_task(
                    &[Operand::Copy(Place::from_local(Local(1)))],
                    program.reference,
                    point,
                    &state,
                )
                .unwrap()
            else {
                panic!("the callback has a normal return");
            };
            let returned = flow.freeze_sources(point, outcomes[0].returned.clone());
            assert!(returned.origins().contains(&origin));
            assert!(
                returned.sources.values().flatten().any(|source| {
                    matches!(source, Source::Entry(atom) if correct_source(&atom.coordinate.storage))
                }),
                "await must retain the selected callback source"
            );
        }
        flow.settle_pending_tasks(point, &mut state);
        assert!(state
            .stored_value(&root_storage)
            .unwrap()
            .future_result()
            .origins()
            .contains(&origin));
        let prior = flow.transfers[&point].before(&Demand::from_iter([Atom {
            coordinate: Coordinate {
                storage: root_storage.clone(),
                path: vec![Slot::Element],
            },
            origin,
            capture: None,
        }]));
        assert!(
            prior
                .iter()
                .any(|atom| correct_source(&atom.coordinate.storage)),
            "a settled result must demand its selected callback source"
        );
        if let Storage::Residual(key) = &root_storage {
            let bound_origin = Origin::Loan(Point::new(BlockId::ENTRY, 3));
            assert!(state
                .stored_value(&root_storage)
                .unwrap()
                .future_result()
                .origins()
                .contains(&bound_origin));
            let prior = flow.transfers[&point].before(&Demand::from_iter([Atom {
                coordinate: Coordinate {
                    storage: root_storage.clone(),
                    path: vec![Slot::Element],
                },
                origin: bound_origin,
                capture: None,
            }]));
            assert!(
                prior.iter().any(|atom| atom.coordinate.storage == Storage::ResidualCallback(key.clone(), program.callback_ty)),
                "the bound callback must retain its independent source alongside the resident recipe"
            );
        }
    }

    #[test]
    fn resident_task_queries_keep_the_actual_callback_node_source_without_catalog_backing() {
        check_task_query_sources(Root::Resident);
    }

    #[test]
    fn residual_task_queries_keep_both_resident_and_bound_callback_sources() {
        check_task_query_sources(Root::Residual);
    }

    #[test]
    fn fallback_task_queries_keep_the_catalog_source() {
        check_task_query_sources(Root::Catalog);
    }
}
