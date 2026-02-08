//! # `spark_borrow`
//!
//! Borrow checking for the Spark compiler (Phase 8).
//!
//! This crate validates ownership, move semantics, and reference safety over
//! the MIR control-flow graph. It implements an NLL-style (Non-Lexical Lifetimes)
//! analysis:
//!
//! 1. **Fact extraction**: Walk the MIR to find loans, uses, moves, and CFG edges.
//! 2. **Liveness analysis**: Backward dataflow to determine which locals are live
//!    at each program point.
//! 3. **Region inference**: Compute the set of program points where each loan
//!    (borrow) must be active.
//! 4. **Conflict detection**: Check for overlapping mutable borrows, use after move,
//!    mutation through immutable references, etc.
//!
//! ## Quick Start
//!
//! ```ignore
//! use spark_borrow::{check, BorrowInput};
//!
//! let input = BorrowInput {
//!     mir: mir_output.mir,
//!     table: mir_output.table,
//!     definitions: resolver_output.definitions,
//! };
//!
//! let output = check(input);
//!
//! if output.diagnostics.has_errors() {
//!     // Handle borrow check errors
//! } else {
//!     // Use output.mir for subsequent phases
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::cast_possible_truncation)]

pub mod check;
pub mod errors;
pub mod facts;
pub mod liveness;
pub mod regions;

use spark_diagnostics::Diagnostics;
use spark_mir::Mir;
use spark_resolve::DefinitionTable;
use spark_types::{Ty, TypeTable};

pub use facts::{BorrowFacts, Loan, LoanId, Point};
pub use liveness::LivenessResult;
pub use regions::Region;

/// Input to the borrow checker.
#[derive(Debug)]
pub struct BorrowInput {
    /// The MIR to check.
    pub mir: Mir,
    /// The type table (read-only).
    pub table: TypeTable,
    /// Definition table from the resolver.
    pub definitions: DefinitionTable,
}

/// Output from the borrow checker.
#[derive(Debug)]
pub struct BorrowOutput {
    /// The MIR (passed through unchanged).
    pub mir: Mir,
    /// The type table (passed through unchanged).
    pub table: TypeTable,
    /// Diagnostics accumulated during borrow checking.
    pub diagnostics: Diagnostics,
}

/// Runs borrow checking on all functions in the MIR.
///
/// Functions whose signatures do not involve `Ref` types are skipped entirely,
/// as they use default copy semantics and don't need borrow checking.
///
/// # Example
///
/// ```ignore
/// let output = spark_borrow::check(BorrowInput {
///     mir: mir_output.mir,
///     table: mir_output.table,
///     definitions: resolver_output.definitions,
/// });
/// assert!(!output.diagnostics.has_errors());
/// ```
#[must_use]
pub fn check(input: BorrowInput) -> BorrowOutput {
    let BorrowInput {
        mir,
        table,
        definitions,
    } = input;

    let mut diagnostics = Diagnostics::new();

    for func in mir.functions.values() {
        // Skip functions that don't use Ref types (optimization).
        if !function_uses_refs(func, &table) {
            continue;
        }

        let borrow_facts = facts::extract_facts(func);
        let live = liveness::compute_liveness(func, &borrow_facts);
        let regs = regions::compute_regions(&borrow_facts, &live);
        check::check_borrows(func, &borrow_facts, &regs, &mut diagnostics);
    }

    let _ = &definitions;

    BorrowOutput {
        mir,
        table,
        diagnostics,
    }
}

/// Returns true if a function's parameter or return types involve `Ty::Ref`.
fn function_uses_refs(func: &spark_mir::MirFunction, table: &TypeTable) -> bool {
    for local_decl in &func.locals {
        if type_involves_ref(local_decl.ty, table) {
            return true;
        }
    }
    false
}

/// Returns true if a type (or any type reachable from it) involves `Ty::Ref`.
fn type_involves_ref(type_id: spark_types::TypeId, table: &TypeTable) -> bool {
    match table.resolve(type_id) {
        Ty::Ref { .. } => true,
        Ty::Optional(inner) | Ty::Mutable(inner) | Ty::List(inner) | Ty::Set(inner) => {
            type_involves_ref(*inner, table)
        }
        Ty::Map(k, v) => type_involves_ref(*k, table) || type_involves_ref(*v, table),
        Ty::Tuple(elems) => elems.iter().any(|e| type_involves_ref(*e, table)),
        Ty::Union(members) => members.iter().any(|m| type_involves_ref(*m, table)),
        Ty::Function { params, ret, .. } => {
            params.iter().any(|p| type_involves_ref(*p, table))
                || type_involves_ref(*ret, table)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_mir::{
        BasicBlock, Constant, Local, LocalDecl, Mir, MirFunction, Operand, Place, Projection,
        Rvalue, Statement, Terminator,
    };
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::{LifetimeId, TypeId};
    use std::collections::HashMap;

    /// Creates an empty BorrowInput with no functions.
    fn empty_input() -> BorrowInput {
        BorrowInput {
            mir: Mir {
                functions: HashMap::new(),
            },
            table: TypeTable::new(),
            definitions: DefinitionTable::new(),
        }
    }

    #[test]
    fn empty_input_passes() {
        let output = check(empty_input());
        assert!(!output.diagnostics.has_errors());
    }

    #[test]
    fn function_without_refs_skipped() {
        // A function that uses only Int — should be skipped entirely.
        let mut table = TypeTable::new();
        let int_ty = table.intern(Ty::Int(spark_types::IntWidth::I64));

        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: int_ty,
            locals: vec![LocalDecl {
                local: Local(0),
                ty: int_ty,
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
            span: Span::DUMMY,
        };

        let mut functions = HashMap::new();
        functions.insert(DefId::new(0), func);

        let output = check(BorrowInput {
            mir: Mir { functions },
            table,
            definitions: DefinitionTable::new(),
        });

        assert!(!output.diagnostics.has_errors());
    }

    #[test]
    fn function_with_ref_type_checked() {
        // Create a Ref type in the table.
        let mut table = TypeTable::new();
        let int_ty = table.intern(Ty::Int(spark_types::IntWidth::I64));
        let ref_ty = table.intern(Ty::Ref {
            inner: int_ty,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });

        // fn f(x: Int): Void { r: Ref[Int] = &x; }
        // This creates a loan. Since the ref is immutable and only one borrow,
        // it should pass.
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 1,
            return_type: TypeId::ERROR,
            locals: vec![
                LocalDecl { local: Local(0), ty: TypeId::ERROR, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(1), ty: int_ty, name: Some(spark_intern::sym("x")), is_user_variable: true, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(2), ty: ref_ty, name: Some(spark_intern::sym("r")), is_user_variable: true, mutable: false, span: Span::DUMMY },
            ],
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Ref { place: Place::from_local(Local(1)), mutable: false },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Void)),
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            span: Span::DUMMY,
        };

        let mut functions = HashMap::new();
        functions.insert(DefId::new(0), func);

        let output = check(BorrowInput {
            mir: Mir { functions },
            table,
            definitions: DefinitionTable::new(),
        });

        assert!(
            !output.diagnostics.has_errors(),
            "Single immutable borrow should pass: {:?}",
            output.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn conflicting_mutable_borrows_detected() {
        let mut table = TypeTable::new();
        let int_ty = table.intern(Ty::Int(spark_types::IntWidth::I64));
        let ref_ty = table.intern(Ty::Ref {
            inner: int_ty,
            mutable: true,
            lifetime: LifetimeId::ANONYMOUS,
        });

        // _1 = const 42
        // _2 = Ref(_1) mut
        // _3 = Ref(_1) mut
        // _4 = copy _2.*
        // _5 = copy _3.*
        // _0 = void
        // return
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: vec![
                LocalDecl { local: Local(0), ty: TypeId::ERROR, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(1), ty: int_ty, name: Some(spark_intern::sym("x")), is_user_variable: true, mutable: true, span: Span::DUMMY },
                LocalDecl { local: Local(2), ty: ref_ty, name: Some(spark_intern::sym("r1")), is_user_variable: true, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(3), ty: ref_ty, name: Some(spark_intern::sym("r2")), is_user_variable: true, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(4), ty: int_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
                LocalDecl { local: Local(5), ty: int_ty, name: None, is_user_variable: false, mutable: false, span: Span::DUMMY },
            ],
            blocks: vec![BasicBlock {
                statements: vec![
                    Statement::Assign {
                        place: Place::from_local(Local(1)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Int(42))),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(2)),
                        rvalue: Rvalue::Ref { place: Place::from_local(Local(1)), mutable: true },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(3)),
                        rvalue: Rvalue::Ref { place: Place::from_local(Local(1)), mutable: true },
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(4)),
                        rvalue: Rvalue::Use(Operand::Copy({
                            let mut p = Place::from_local(Local(2));
                            p.projections.push(Projection::Deref);
                            p
                        })),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(5)),
                        rvalue: Rvalue::Use(Operand::Copy({
                            let mut p = Place::from_local(Local(3));
                            p.projections.push(Projection::Deref);
                            p
                        })),
                        span: Span::DUMMY,
                    },
                    Statement::Assign {
                        place: Place::from_local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Constant(Constant::Void)),
                        span: Span::DUMMY,
                    },
                ],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
            span: Span::DUMMY,
        };

        let mut functions = HashMap::new();
        functions.insert(DefId::new(0), func);

        let output = check(BorrowInput {
            mir: Mir { functions },
            table,
            definitions: DefinitionTable::new(),
        });

        assert!(
            output.diagnostics.has_errors(),
            "Two mutable borrows of the same place should produce an error"
        );
    }

    #[test]
    fn type_involves_ref_detection() {
        let mut table = TypeTable::new();
        let int_ty = table.intern(Ty::Int(spark_types::IntWidth::I64));
        let ref_ty = table.intern(Ty::Ref {
            inner: int_ty,
            mutable: false,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let optional_ref = table.intern(Ty::Optional(ref_ty));

        assert!(!type_involves_ref(int_ty, &table));
        assert!(type_involves_ref(ref_ty, &table));
        assert!(type_involves_ref(optional_ref, &table));
    }

    #[test]
    fn mir_passed_through_unchanged() {
        let output = check(empty_input());
        assert!(output.mir.functions.is_empty());
    }
}
