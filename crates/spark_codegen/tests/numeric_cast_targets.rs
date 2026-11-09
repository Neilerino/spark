//! Checks scalar cast emission for ARM64 and x86-64 native objects.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use object::Object;
use spark_codegen::{generate, CodegenInput, TargetSpec};
use spark_mir::{
    BasicBlock, Local, LocalDecl, Mir, MirFunction, Operand, Place, Rvalue, Statement, Terminator,
};
use spark_modules::ModulePath;
use spark_resolve::{DefinitionInfo, DefinitionKind, DefinitionTable, Visibility};
use spark_span::Span;
use spark_types::{FloatWidth, IntWidth, Ty, TypeTable};

#[test]
fn scalar_casts_emit_for_arm64_and_x86_64() {
    for (triple, architecture) in [
        ("aarch64-unknown-linux-gnu", object::Architecture::Aarch64),
        ("x86_64-unknown-linux-gnu", object::Architecture::X86_64),
    ] {
        let mut table = TypeTable::new();
        let mut definitions = DefinitionTable::new();
        let mut functions = HashMap::new();
        let integers = [
            IntWidth::I8,
            IntWidth::I16,
            IntWidth::I32,
            IntWidth::I64,
            IntWidth::U8,
            IntWidth::U16,
            IntWidth::U32,
            IntWidth::U64,
        ];
        let numeric: Vec<Ty> = integers
            .into_iter()
            .map(Ty::Int)
            .chain([Ty::Float(FloatWidth::F32), Ty::Float(FloatWidth::F64)])
            .collect();
        let mut pairs: Vec<(Ty, Ty)> = numeric
            .iter()
            .flat_map(|source| {
                numeric
                    .iter()
                    .map(move |target| (source.clone(), target.clone()))
            })
            .collect();
        pairs.extend(integers.into_iter().map(|width| (Ty::Bool, Ty::Int(width))));
        pairs.extend([
            (Ty::Byte, Ty::Int(IntWidth::U8)),
            (Ty::Int(IntWidth::U8), Ty::Byte),
        ]);
        for (index, (source, target)) in pairs.into_iter().enumerate() {
            let source = table.intern(source);
            let target = table.intern(target);
            let def = definitions.insert(DefinitionInfo::new(
                DefinitionKind::Function,
                spark_intern::sym(&format!("cast_{index}")),
                Span::DUMMY,
                Visibility::Public,
                ModulePath::root(),
            ));
            let locals = [target, source]
                .into_iter()
                .enumerate()
                .map(|(index, ty)| LocalDecl {
                    local: Local(u32::try_from(index).unwrap()),
                    ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                })
                .collect();
            functions.insert(
                def,
                MirFunction {
                    def_id: def,
                    param_count: 1,
                    return_type: target,
                    locals,
                    blocks: vec![BasicBlock {
                        statements: vec![Statement::Assign {
                            place: Place::from_local(Local(0)),
                            rvalue: Rvalue::Cast {
                                operand: Operand::Copy(Place::from_local(Local(1))),
                                target_ty: target,
                            },
                            span: Span::DUMMY,
                        }],
                        terminator: Terminator::Return,
                        is_cleanup: false,
                    }],
                    captures: vec![],
                    is_async: false,
                    span: Span::DUMMY,
                },
            );
        }
        let output = generate(CodegenInput {
            mir: Mir { functions },
            table,
            definitions,
            target: TargetSpec {
                triple: triple.parse().unwrap(),
            },
            poll_fn_map: rustc_hash::FxHashMap::default(),
        });
        assert!(
            !output.diagnostics.has_errors(),
            "{triple}: {:?}",
            output.diagnostics
        );
        let object = object::File::parse(output.object_bytes.as_slice()).unwrap();
        assert_eq!(object.architecture(), architecture);
    }
}
