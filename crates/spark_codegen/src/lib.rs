//! # `spark_codegen`
//!
//! Native code generation for the Spark compiler (Phase 9).
//!
//! This crate compiles MIR (mid-level intermediate representation) to native
//! machine code using a pluggable backend architecture. The default backend
//! uses Cranelift for AOT compilation to object files.
//!
//! ## Quick Start
//!
//! ```ignore
//! use spark_codegen::{generate, CodegenInput, TargetSpec};
//!
//! let input = CodegenInput {
//!     mir: borrow_output.mir,
//!     table: borrow_output.table,
//!     definitions: resolver_definitions,
//!     target: TargetSpec::host(),
//! };
//!
//! let output = generate(input);
//!
//! if output.diagnostics.has_errors() {
//!     // Handle code generation errors
//! } else {
//!     std::fs::write("output.o", &output.object_bytes).unwrap();
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::implicit_hasher)]

pub mod abi;
pub mod backend;
pub mod cranelift;
pub mod layout;

use spark_diagnostics::Diagnostics;
use spark_mir::Mir;
use spark_resolve::DefinitionTable;
use spark_types::TypeTable;

use crate::backend::CodegenBackend;
use crate::cranelift::CraneliftBackend;

/// Target specification for code generation.
#[derive(Debug, Clone)]
pub struct TargetSpec {
    /// The target triple (e.g. `x86_64-unknown-linux-gnu`).
    pub triple: target_lexicon::Triple,
}

impl TargetSpec {
    /// Returns a target specification for the host machine.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn host() -> Self {
        Self {
            triple: target_lexicon::Triple::host(),
        }
    }
}

impl Default for TargetSpec {
    fn default() -> Self {
        Self::host()
    }
}

/// Input to the code generation pass.
#[derive(Debug)]
pub struct CodegenInput {
    /// The MIR to compile.
    pub mir: Mir,
    /// The type table (read-only).
    pub table: TypeTable,
    /// Definition table from the resolver.
    pub definitions: DefinitionTable,
    /// Target specification (defaults to host).
    pub target: TargetSpec,
}

/// Output from the code generation pass.
#[derive(Debug)]
pub struct CodegenOutput {
    /// The compiled native object file bytes (.o / .obj).
    pub object_bytes: Vec<u8>,
    /// Diagnostics accumulated during code generation.
    pub diagnostics: Diagnostics,
}

/// Compiles MIR to native object code.
///
/// This is the main entry point for code generation. It uses the Cranelift
/// backend by default.
///
/// # Example
///
/// ```ignore
/// let output = spark_codegen::generate(CodegenInput {
///     mir: borrow_output.mir,
///     table: borrow_output.table,
///     definitions: resolver_definitions,
///     target: TargetSpec::host(),
/// });
/// assert!(!output.diagnostics.has_errors());
/// ```
#[must_use]
pub fn generate(input: CodegenInput) -> CodegenOutput {
    let CodegenInput {
        mir,
        table,
        definitions,
        target,
    } = input;

    let mut diagnostics = Diagnostics::new();

    let backend = match CraneliftBackend::new(&target) {
        Ok(b) => b,
        Err(msg) => {
            diagnostics.error(msg);
            return CodegenOutput {
                object_bytes: Vec::new(),
                diagnostics,
            };
        }
    };

    let object_bytes = backend.compile(&mir, &table, &definitions, &mut diagnostics);

    CodegenOutput {
        object_bytes,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_ast::BinaryOp;
    use spark_mir::{
        BasicBlock, BlockId, Constant, Local, LocalDecl, MirFunction, Operand, Place, Rvalue,
        Statement, Terminator,
    };
    use spark_modules::ModulePath;
    use spark_resolve::{DefId, DefinitionInfo, DefinitionKind, Visibility};
    use spark_span::Span;
    use spark_types::{IntWidth, Ty, TypeId};
    use std::collections::HashMap;

    /// Helper: builds a `CodegenInput` from a list of functions.
    fn make_input(
        functions: Vec<(DefId, MirFunction)>,
        table: TypeTable,
        definitions: DefinitionTable,
    ) -> CodegenInput {
        let mut map = HashMap::new();
        for (id, f) in functions {
            map.insert(id, f);
        }
        CodegenInput {
            mir: Mir { functions: map },
            table,
            definitions,
            target: TargetSpec::host(),
        }
    }

    /// Helper: registers a function definition.
    fn register_fn(
        definitions: &mut DefinitionTable,
        name: &str,
    ) -> DefId {
        definitions.insert(DefinitionInfo::new(
            DefinitionKind::Function,
            spark_intern::sym(name),
            Span::DUMMY,
            Visibility::Public,
            ModulePath::root(),
        ))
    }

    #[test]
    fn empty_program_produces_valid_object() {
        let input = CodegenInput {
            mir: Mir {
                functions: HashMap::new(),
            },
            table: TypeTable::new(),
            definitions: DefinitionTable::new(),
            target: TargetSpec::host(),
        };

        let output = generate(input);
        assert!(
            !output.diagnostics.has_errors(),
            "empty program should not produce errors"
        );
        // Even an empty program produces object file headers.
        assert!(
            !output.object_bytes.is_empty(),
            "should produce non-empty object bytes"
        );
    }

    #[test]
    fn void_function_compiles() {
        // fn noop(): Void {}
        let mut table = TypeTable::new();
        let void_ty = table.intern(Ty::Void);
        let mut definitions = DefinitionTable::new();
        let def_id = register_fn(&mut definitions, "noop");

        let func = MirFunction {
            def_id,
            param_count: 0,
            return_type: void_ty,
            locals: vec![LocalDecl {
                local: Local(0),
                ty: void_ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            }],
            blocks: vec![BasicBlock {
                statements: vec![],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            span: Span::DUMMY,
        };

        let input = make_input(vec![(def_id, func)], table, definitions);
        let output = generate(input);

        assert!(
            !output.diagnostics.has_errors(),
            "void function should compile: {:?}",
            output.diagnostics
        );
        assert!(!output.object_bytes.is_empty());
    }

    #[test]
    fn int_constant_return() {
        // fn answer(): Int { 42 }
        let mut table = TypeTable::new();
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));
        let mut definitions = DefinitionTable::new();
        let def_id = register_fn(&mut definitions, "answer");

        let func = MirFunction {
            def_id,
            param_count: 0,
            return_type: i64_ty,
            locals: vec![LocalDecl {
                local: Local(0),
                ty: i64_ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            }],
            blocks: vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            span: Span::DUMMY,
        };

        let input = make_input(vec![(def_id, func)], table, definitions);
        let output = generate(input);

        assert!(
            !output.diagnostics.has_errors(),
            "constant return should compile: {:?}",
            output.diagnostics
        );
        assert!(!output.object_bytes.is_empty());
    }

    #[test]
    fn identity_function() {
        // fn id(x: Int): Int { x }
        let mut table = TypeTable::new();
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));
        let mut definitions = DefinitionTable::new();
        let def_id = register_fn(&mut definitions, "id");

        let func = MirFunction {
            def_id,
            param_count: 1,
            return_type: i64_ty,
            locals: vec![
                LocalDecl {
                    local: Local(0),
                    ty: i64_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(1),
                    ty: i64_ty,
                    name: Some(spark_intern::sym("x")),
                    is_user_variable: true,
                    mutable: false,
                    span: Span::DUMMY,
                },
            ],
            blocks: vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(1)))),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            span: Span::DUMMY,
        };

        let input = make_input(vec![(def_id, func)], table, definitions);
        let output = generate(input);

        assert!(
            !output.diagnostics.has_errors(),
            "identity function should compile: {:?}",
            output.diagnostics
        );
        assert!(!output.object_bytes.is_empty());
    }

    #[test]
    fn binary_add() {
        // fn add(a: Int, b: Int): Int { a + b }
        let mut table = TypeTable::new();
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));
        let mut definitions = DefinitionTable::new();
        let def_id = register_fn(&mut definitions, "add");

        let func = MirFunction {
            def_id,
            param_count: 2,
            return_type: i64_ty,
            locals: vec![
                LocalDecl {
                    local: Local(0),
                    ty: i64_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(1),
                    ty: i64_ty,
                    name: Some(spark_intern::sym("a")),
                    is_user_variable: true,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(2),
                    ty: i64_ty,
                    name: Some(spark_intern::sym("b")),
                    is_user_variable: true,
                    mutable: false,
                    span: Span::DUMMY,
                },
            ],
            blocks: vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::BinaryOp(
                        BinaryOp::Add,
                        Operand::Copy(Place::from_local(Local(1))),
                        Operand::Copy(Place::from_local(Local(2))),
                    ),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            span: Span::DUMMY,
        };

        let input = make_input(vec![(def_id, func)], table, definitions);
        let output = generate(input);

        assert!(
            !output.diagnostics.has_errors(),
            "add function should compile: {:?}",
            output.diagnostics
        );
        assert!(!output.object_bytes.is_empty());
    }

    #[test]
    fn multi_function_with_call() {
        // fn double(x: Int): Int { x + x }
        // fn main(): Int { double(21) }
        let mut table = TypeTable::new();
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));
        let mut definitions = DefinitionTable::new();
        let double_id = register_fn(&mut definitions, "double");
        let main_id = register_fn(&mut definitions, "main");

        let double_func = MirFunction {
            def_id: double_id,
            param_count: 1,
            return_type: i64_ty,
            locals: vec![
                LocalDecl {
                    local: Local(0),
                    ty: i64_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                LocalDecl {
                    local: Local(1),
                    ty: i64_ty,
                    name: Some(spark_intern::sym("x")),
                    is_user_variable: true,
                    mutable: false,
                    span: Span::DUMMY,
                },
            ],
            blocks: vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::BinaryOp(
                        BinaryOp::Add,
                        Operand::Copy(Place::from_local(Local(1))),
                        Operand::Copy(Place::from_local(Local(1))),
                    ),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            span: Span::DUMMY,
        };

        let main_func = MirFunction {
            def_id: main_id,
            param_count: 0,
            return_type: i64_ty,
            locals: vec![
                LocalDecl {
                    local: Local(0),
                    ty: i64_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
                // Temp for call argument
                LocalDecl {
                    local: Local(1),
                    ty: i64_ty,
                    name: None,
                    is_user_variable: false,
                    mutable: false,
                    span: Span::DUMMY,
                },
            ],
            blocks: vec![
                // Block 0: call double(21), continue to block 1
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(21))),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::Call {
                        func: Operand::Constant(Constant::FunctionRef(double_id)),
                        args: vec![Operand::Copy(Place::from_local(Local(1)))],
                        destination: Place::from_local(Local(0)),
                        target: BlockId(1),
                        span: Span::DUMMY,
                    },
                    is_cleanup: false,
                },
                // Block 1: return
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
            captures: vec![],
            span: Span::DUMMY,
        };

        let input = make_input(
            vec![(double_id, double_func), (main_id, main_func)],
            table,
            definitions,
        );
        let output = generate(input);

        assert!(
            !output.diagnostics.has_errors(),
            "multi-function with call should compile: {:?}",
            output.diagnostics
        );
        assert!(!output.object_bytes.is_empty());
    }

    #[test]
    fn control_flow_if_else() {
        // fn abs(x: Int): Int { if x < 0 { -x } else { x } }
        // MIR:
        //   _0: Int (return)
        //   _1: Int (param x)
        //   _2: Bool (comparison result)
        //   bb0: _2 = _1 < 0; switchint _2 [1 -> bb1] otherwise bb2
        //   bb1: _0 = -_1; goto bb3
        //   bb2: _0 = _1; goto bb3
        //   bb3: return
        let mut table = TypeTable::new();
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));
        let bool_ty = table.intern(Ty::Bool);
        let mut definitions = DefinitionTable::new();
        let def_id = register_fn(&mut definitions, "abs");

        let func = MirFunction {
            def_id,
            param_count: 1,
            return_type: i64_ty,
            locals: vec![
                LocalDecl { local: Local(0), ty: i64_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(1), ty: i64_ty, name: Some(spark_intern::sym("x")), is_user_variable: true, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(2), ty: bool_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
            ],
            blocks: vec![
                // bb0: compare and branch
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::BinaryOp(
                            BinaryOp::Lt,
                            Operand::Copy(Place::from_local(Local(1))),
                            Operand::Constant(Constant::Int(0)),
                        ),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::SwitchInt {
                        discriminant: Operand::Copy(Place::from_local(Local(2))),
                        targets: vec![(1, BlockId(1))],
                        otherwise: BlockId(2),
                    },
                    is_cleanup: false,
                },
                // bb1: negate
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::UnaryOp(
                            spark_ast::UnaryOp::Neg,
                            Operand::Copy(Place::from_local(Local(1))),
                        ),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::Goto { target: BlockId(3) },
                    is_cleanup: false,
                },
                // bb2: identity
                BasicBlock {
                    statements: vec![Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(1)))),
                        span: Span::DUMMY,
                    }],
                    terminator: Terminator::Goto { target: BlockId(3) },
                    is_cleanup: false,
                },
                // bb3: return
                BasicBlock {
                    statements: vec![],
                    terminator: Terminator::Return,
                    is_cleanup: false,
                },
            ],
            captures: vec![],
            span: Span::DUMMY,
        };

        let input = make_input(vec![(def_id, func)], table, definitions);
        let output = generate(input);

        assert!(
            !output.diagnostics.has_errors(),
            "control flow function should compile: {:?}",
            output.diagnostics
        );
        assert!(!output.object_bytes.is_empty());
    }

    #[test]
    fn bool_constant_return() {
        // fn yes(): Bool { true }
        let mut table = TypeTable::new();
        let bool_ty = table.intern(Ty::Bool);
        let mut definitions = DefinitionTable::new();
        let def_id = register_fn(&mut definitions, "yes");

        let func = MirFunction {
            def_id,
            param_count: 0,
            return_type: bool_ty,
            locals: vec![LocalDecl {
                local: Local(0),
                ty: bool_ty,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            }],
            blocks: vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::Use(Operand::Constant(Constant::Bool(true))),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            span: Span::DUMMY,
        };

        let input = make_input(vec![(def_id, func)], table, definitions);
        let output = generate(input);

        assert!(
            !output.diagnostics.has_errors(),
            "bool return should compile: {:?}",
            output.diagnostics
        );
        assert!(!output.object_bytes.is_empty());
    }

    #[test]
    fn float_operations() {
        // fn fadd(a: Float64, b: Float64): Float64 { a + b }
        let mut table = TypeTable::new();
        let f64_ty = table.intern(Ty::Float(spark_types::FloatWidth::F64));
        let mut definitions = DefinitionTable::new();
        let def_id = register_fn(&mut definitions, "fadd");

        let func = MirFunction {
            def_id,
            param_count: 2,
            return_type: f64_ty,
            locals: vec![
                LocalDecl { local: Local(0), ty: f64_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(1), ty: f64_ty, name: Some(spark_intern::sym("a")), is_user_variable: true, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(2), ty: f64_ty, name: Some(spark_intern::sym("b")), is_user_variable: true, mutable: false, span: Span::DUMMY },
            ],
            blocks: vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::BinaryOp(
                        BinaryOp::Add,
                        Operand::Copy(Place::from_local(Local(1))),
                        Operand::Copy(Place::from_local(Local(2))),
                    ),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            span: Span::DUMMY,
        };

        let input = make_input(vec![(def_id, func)], table, definitions);
        let output = generate(input);

        assert!(
            !output.diagnostics.has_errors(),
            "float add should compile: {:?}",
            output.diagnostics
        );
        assert!(!output.object_bytes.is_empty());
    }

    #[test]
    fn multiple_locals_and_assignments() {
        // fn multi(): Int {
        //     a: Int = 10
        //     b: Int = 20
        //     c: Int = a + b
        //     c
        // }
        let mut table = TypeTable::new();
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));
        let mut definitions = DefinitionTable::new();
        let def_id = register_fn(&mut definitions, "multi");

        let func = MirFunction {
            def_id,
            param_count: 0,
            return_type: i64_ty,
            locals: vec![
                LocalDecl { local: Local(0), ty: i64_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(1), ty: i64_ty, name: Some(spark_intern::sym("a")), is_user_variable: true, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(2), ty: i64_ty, name: Some(spark_intern::sym("b")), is_user_variable: true, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(3), ty: i64_ty, name: Some(spark_intern::sym("c")), is_user_variable: true, mutable: false, span: Span::DUMMY },
            ],
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(10))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(20))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(3)),
                        rvalue: Rvalue::BinaryOp(
                            BinaryOp::Add,
                            Operand::Copy(Place::from_local(Local(1))),
                            Operand::Copy(Place::from_local(Local(2))),
                        ),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(3)))),
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            span: Span::DUMMY,
        };

        let input = make_input(vec![(def_id, func)], table, definitions);
        let output = generate(input);

        assert!(
            !output.diagnostics.has_errors(),
            "multiple locals should compile: {:?}",
            output.diagnostics
        );
        assert!(!output.object_bytes.is_empty());
    }

    #[test]
    fn nop_and_drop_statements() {
        let mut table = TypeTable::new();
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));
        let mut definitions = DefinitionTable::new();
        let def_id = register_fn(&mut definitions, "nop_test");

        let func = MirFunction {
            def_id,
            param_count: 0,
            return_type: i64_ty,
            locals: vec![
                LocalDecl { local: Local(0), ty: i64_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(1), ty: i64_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
            ],
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(99))),
                        span: Span::DUMMY,
                    },
                    Statement::Nop,
                    Statement::Drop {
                        place: Place::from_local(Local(1)),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Copy(Place::from_local(Local(1)))),
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            captures: vec![],
            span: Span::DUMMY,
        };

        let input = make_input(vec![(def_id, func)], table, definitions);
        let output = generate(input);

        assert!(
            !output.diagnostics.has_errors(),
            "nop/drop should compile: {:?}",
            output.diagnostics
        );
    }
}
