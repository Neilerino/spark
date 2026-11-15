// TAG [SPARK-154]
//! Drive actual generated poll code through repeated Pending and wakeup.

use std::collections::HashMap;
use std::process::Command;

use spark_ast::BinaryOp;
use spark_codegen::{layout::LayoutCache, CodegenInput, TargetSpec};
use spark_intern::{kw, sym};
use spark_mir::async_transform::{transform, AsyncTransformInput};
use spark_mir::{
    AggregateKind, BasicBlock, BlockId, Constant, Local, LocalDecl, Mir, MirFunction, Operand,
    Place, Projection, Rvalue, Statement, Terminator,
};
use spark_modules::ModulePath;
use spark_opt::OptLevel;
use spark_resolve::{DefinitionInfo, DefinitionKind, DefinitionTable, Visibility};
use spark_span::Span;
use spark_types::{IntWidth, Ty, TypeTable};

#[derive(Clone, Copy, Debug)]
enum Referent {
    Scalar,
    Tuple,
}

fn poll_object(
    opt_level: OptLevel,
    reference_parameter: bool,
    referent: Referent,
) -> (Vec<u8>, u32, u32, u32) {
    let mut table = TypeTable::new();
    let integer = table.intern(Ty::Int(IntWidth::I64));
    let future = table.intern(Ty::Future { inner: integer });
    let source_type = match referent {
        Referent::Scalar => integer,
        Referent::Tuple => table.intern(Ty::Tuple(vec![integer, integer])),
    };
    let reference = table.intern(Ty::Ref {
        inner: source_type,
        mutable: false,
        lifetime: spark_types::ty::LifetimeId::ANONYMOUS,
    });
    let future_reference = table.intern(Ty::Ref {
        inner: future,
        mutable: false,
        lifetime: spark_types::ty::LifetimeId::ANONYMOUS,
    });
    let mut definitions = DefinitionTable::new();
    let mut define = |kind, name| {
        definitions.insert(DefinitionInfo::new(
            kind,
            name,
            Span::DUMMY,
            Visibility::Private,
            ModulePath::root(),
        ))
    };
    let await_def = define(DefinitionKind::BuiltinFunction, kw::SPARK_AWAIT);
    let entered = define(DefinitionKind::ExternFunction, sym("record_entered"));
    let relay = define(DefinitionKind::Function, sym("relay"));
    let entered_ty = table.intern(Ty::Function {
        params: vec![],
        ret: integer,
        is_async: false,
    });
    table.set_def_type(entered, entered_ty);
    let assign = |local, rvalue| Statement::Assign {
        place: Place::from_local(Local(local)),
        rvalue,
        span: Span::DUMMY,
    };
    let mut function = MirFunction {
        def_id: relay,
        param_count: 1,
        return_type: integer,
        locals: [
            integer,
            future,
            source_type,
            reference,
            integer,
            integer,
            future_reference,
            future,
        ]
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
        blocks: vec![
            BasicBlock {
                statements: vec![
                    assign(
                        2,
                        match referent {
                            Referent::Scalar => Rvalue::Use(Operand::Constant(Constant::Int(10))),
                            Referent::Tuple => Rvalue::Aggregate(
                                AggregateKind::Tuple,
                                vec![
                                    Operand::Constant(Constant::Int(10)),
                                    Operand::Constant(Constant::Int(20)),
                                ],
                            ),
                        },
                    ),
                    assign(
                        3,
                        Rvalue::Ref {
                            place: Place::from_local(Local(2)),
                            mutable: false,
                        },
                    ),
                ],
                terminator: Terminator::Call {
                    func: Operand::Constant(Constant::FunctionRef(entered)),
                    args: vec![],
                    destination: Place::from_local(Local(4)),
                    target: BlockId(1),
                    span: Span::DUMMY,
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![],
                terminator: Terminator::Call {
                    func: Operand::Constant(Constant::FunctionRef(await_def)),
                    args: vec![Operand::Copy(Place::from_local(Local(1)))],
                    destination: Place::from_local(Local(5)),
                    target: BlockId(2),
                    span: Span::DUMMY,
                },
                is_cleanup: false,
            },
            BasicBlock {
                statements: vec![
                    assign(
                        0,
                        Rvalue::BinaryOp(
                            BinaryOp::Add,
                            Operand::Copy(Place {
                                local: Local(3),
                                projections: match referent {
                                    Referent::Scalar => vec![Projection::Deref],
                                    Referent::Tuple => {
                                        vec![Projection::Deref, Projection::Field(sym("0"))]
                                    }
                                },
                            }),
                            Operand::Copy(Place::from_local(Local(5))),
                        ),
                    ),
                    Statement::Drop {
                        place: Place::from_local(Local(1)),
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            },
        ],
        captures: vec![],
        is_async: true,
        span: Span::DUMMY,
    };
    assert_eq!(function.locals[2].ty, source_type);
    assert!(
        matches!(table.resolve(function.locals[3].ty), Ty::Ref { inner, .. }
        if *inner == function.locals[2].ty)
    );
    if reference_parameter {
        function.blocks[0].statements.push(assign(
            6,
            Rvalue::Ref {
                place: Place::from_local(Local(1)),
                mutable: false,
            },
        ));
        function.blocks[2].statements.splice(
            1..1,
            [
                assign(
                    7,
                    Rvalue::Use(Operand::Copy(Place {
                        local: Local(6),
                        projections: vec![Projection::Deref],
                    })),
                ),
                Statement::Drop {
                    place: Place::from_local(Local(7)),
                    span: Span::DUMMY,
                },
            ],
        );
    }
    let mut output = transform(AsyncTransformInput {
        mir: Mir {
            functions: HashMap::from([(relay, function)]),
        },
        table,
        definitions,
    });
    assert!(!output.diagnostics.has_errors());
    let info = &output.poll_fn_map[&relay];
    let poll = &output.mir.functions[&info.poll_def_id];
    let referents: Vec<_> = poll
        .blocks
        .iter()
        .flat_map(|block| &block.statements)
        .filter_map(|statement| match statement {
            Statement::Assign {
                rvalue: Rvalue::Ref { place, .. },
                ..
            } => Some(place),
            _ => None,
        })
        .collect();
    assert!(!referents.is_empty());
    assert!(
        referents.iter().all(|place| place.local == Local(1)
            && matches!(
                place.projections.as_slice(),
                [Projection::Deref, Projection::Field(_)]
            )),
        "native suspension is safe only after stable backing is established: {referents:?}"
    );
    let mut layouts = LayoutCache::new(u32::try_from(std::mem::size_of::<usize>()).unwrap());
    let layout = layouts.async_state_layout_of(&info.state_fields, &output.table);
    let offset = |name| {
        layout.field_offsets[info
            .state_fields
            .iter()
            .position(|(field, _)| *field == sym(name))
            .expect("state field")]
    };
    let parameter_offset = offset("__local_1");
    let result_offset = offset("__result");
    // The test object exports just the real poll via the backend's existing main seam.
    output.definitions.get_mut(info.poll_def_id).unwrap().name = sym("main");
    output
        .mir
        .functions
        .retain(|def, _| *def == info.poll_def_id);
    let optimized = spark_opt::optimize(spark_opt::OptInput {
        mir: output.mir,
        table: output.table,
        definitions: output.definitions.clone(),
        opt_level,
    });
    assert!(!optimized.diagnostics.has_errors());
    let errors = spark_mir::validate::validate(&optimized.mir, &optimized.table);
    assert!(errors.is_empty(), "{errors:?}");
    let object = spark_codegen::generate(CodegenInput {
        mir: optimized.mir,
        table: optimized.table,
        definitions: output.definitions,
        target: TargetSpec::host(),
        poll_fn_map: output.poll_fn_map,
    });
    assert!(!object.diagnostics.has_errors(), "{:?}", object.diagnostics);
    (
        object.object_bytes,
        layout.layout.size,
        parameter_offset,
        result_offset,
    )
}

#[test]
fn generated_poll_preserves_references_through_repeated_pending_and_wakeup() {
    assert_suspension(false);
}

#[test]
fn generated_poll_releases_its_initialized_frame_owners() {
    assert_suspension(true);
}

fn assert_suspension(check_owners: bool) {
    for (opt_level, reference_parameter, referent) in [
        (OptLevel::None, false, Referent::Scalar),
        (OptLevel::Basic, false, Referent::Scalar),
        (OptLevel::None, true, Referent::Scalar),
        (OptLevel::Basic, true, Referent::Scalar),
        (OptLevel::None, false, Referent::Tuple),
        (OptLevel::Basic, false, Referent::Tuple),
        (OptLevel::None, true, Referent::Tuple),
        (OptLevel::Basic, true, Referent::Tuple),
    ] {
        let (object, size, parameter, result) =
            poll_object(opt_level, reference_parameter, referent);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("poll.o"), object).unwrap();
        let harness = include_str!("suspension.c")
            .replace("CHECK_OWNERS", if check_owners { "1" } else { "0" })
            .replace("STATE_SIZE", &size.to_string())
            .replace("PARAMETER_OFFSET", &parameter.to_string())
            .replace("RESULT_OFFSET", &result.to_string());
        std::fs::write(dir.path().join("harness.c"), harness).unwrap();
        let runtime = std::path::Path::new(env!("CARGO_BIN_EXE_spark"))
            .parent()
            .unwrap()
            .join("libspark_runtime.a");
        let build = Command::new("cc")
            .current_dir(dir.path())
            .args(["-std=c11", "harness.c", "poll.o"])
            .arg(runtime)
            .args(["-lm", "-lpthread", "-ldl", "-o", "program"])
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
        let run = Command::new(dir.path().join("program")).output().unwrap();
        assert!(
            run.status.success(),
            "{opt_level:?} {referent:?} parameter_ref={reference_parameter}: {:?}: {} stdout={}",
            run.status,
            String::from_utf8_lossy(&run.stderr),
            String::from_utf8_lossy(&run.stdout)
        );
        assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "17 2 1 1");
    }
}
