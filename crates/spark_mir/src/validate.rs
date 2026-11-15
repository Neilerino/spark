// TAG [SPARK-154]

//! MIR structural validation.
//!
//! Runs between the optimization pass and codegen to catch invalid MIR
//! before it reaches code generation, where structural errors would
//! manifest as silent runtime corruption or ICE panics.
//!
//! Checks structural invariants and destination layouts required by codegen.
//!
//! All formatting is deferred to error sites so the happy path (valid MIR)
//! performs only integer comparisons with zero heap allocations.

use spark_types::{Ty, TypeTable};

use crate::mir::{
    AggregateKind, BasicBlock, BlockId, Mir, MirFunction, Operand, Place, Projection, Rvalue,
    Statement, Terminator,
};

/// Validates the structural integrity of all functions in the MIR.
///
/// Returns a list of error messages. An empty list means the MIR is valid.
/// The caller can choose to panic, emit diagnostics, or log depending on
/// the build configuration.
#[must_use]
pub fn validate(mir: &Mir, table: &TypeTable) -> Vec<String> {
    let mut errors = Vec::new();
    for func in mir.functions.values() {
        validate_function(func, table, &mut errors);
    }
    errors
}

/// Validates a single MIR function.
fn validate_function(func: &MirFunction, table: &TypeTable, errors: &mut Vec<String>) {
    let func_id = func.def_id.as_u32();

    if func.blocks.is_empty() {
        errors.push(format!("DefId({func_id}): function has no blocks"));
        return;
    }

    for (i, decl) in func.locals.iter().enumerate() {
        if decl.local.index() != i {
            errors.push(format!(
                "DefId({func_id}): local at index {i} has mismatched Local({})",
                decl.local.0
            ));
        }
    }

    for (block_idx, block) in func.blocks.iter().enumerate() {
        let block_id = BlockId(block_idx as u32);
        validate_block(func, block, block_id, table, errors);
    }
}

/// Validates a single basic block.
fn validate_block(
    func: &MirFunction,
    block: &BasicBlock,
    block_id: BlockId,
    table: &TypeTable,
    errors: &mut Vec<String>,
) {
    let func_id = func.def_id.as_u32();
    let num_locals = func.locals.len();
    let num_blocks = func.blocks.len();
    for (stmt_idx, stmt) in block.statements.iter().enumerate() {
        match stmt {
            Statement::Assign { place, rvalue, .. } => {
                check_place(
                    place, num_locals, func_id, block_id, stmt_idx, "dest", errors,
                );
                check_rvalue(rvalue, num_locals, func_id, block_id, stmt_idx, errors);
                if let Rvalue::CollectionGetOptional { collection, .. } = rvalue {
                    if !has_lookup_layout(func, table, place, collection) {
                        errors.push(format!(
                            "DefId({func_id})/{block_id}/stmt[{stmt_idx}]: \
                             checked lookup requires a matching Optional destination local"
                        ));
                    }
                }
            }
            Statement::IndexedAssign {
                root,
                indices,
                value,
                ..
            } => {
                if indices.is_empty() {
                    errors.push(format!(
                        "DefId({func_id})/{block_id}/stmt[{stmt_idx}]: indexed assignment requires an index"
                    ));
                }
                check_place(
                    root,
                    num_locals,
                    func_id,
                    block_id,
                    stmt_idx,
                    "indexed owner",
                    errors,
                );
                for &local in indices.iter().chain(std::iter::once(value)) {
                    check_place(
                        &Place::from_local(local),
                        num_locals,
                        func_id,
                        block_id,
                        stmt_idx,
                        "indexed assignment operand",
                        errors,
                    );
                }
            }
            Statement::Drop { place, .. } => {
                check_place(
                    place, num_locals, func_id, block_id, stmt_idx, "drop", errors,
                );
            }
            Statement::Nop => {}
        }
    }

    check_terminator(
        &block.terminator,
        num_locals,
        num_blocks,
        func_id,
        block_id,
        errors,
    );
}

fn has_lookup_layout(
    func: &MirFunction,
    table: &TypeTable,
    dest: &Place,
    collection: &Place,
) -> bool {
    if !dest.projections.is_empty() {
        return false;
    }
    let (Some(dest), Some(collection)) = (
        func.locals.get(dest.local.index()),
        crate::mir::place_type(&func.locals, collection, table),
    ) else {
        return false;
    };
    let storage_type = |ty| match table.resolve(ty) {
        Ty::Mutable(inner) => *inner,
        _ => ty,
    };
    let (Ty::List(value) | Ty::Map(_, value)) = table.resolve(storage_type(collection)) else {
        return false;
    };
    matches!(table.resolve(dest.ty), Ty::Optional(inner) if storage_type(*inner) == storage_type(*value))
}

/// Checks that all locals referenced in a place are in bounds.
fn check_place(
    place: &Place,
    num_locals: usize,
    func_id: u32,
    block_id: BlockId,
    stmt_idx: usize,
    tag: &str,
    errors: &mut Vec<String>,
) {
    if place.local.index() >= num_locals {
        errors.push(format!(
            "DefId({func_id})/{block_id}/stmt[{stmt_idx}] {tag}: \
             references Local({}) but function only has {num_locals} locals",
            place.local.0
        ));
    }

    for proj in &place.projections {
        if let Projection::Index(idx_local) = proj {
            if idx_local.index() >= num_locals {
                errors.push(format!(
                    "DefId({func_id})/{block_id}/stmt[{stmt_idx}] {tag}: \
                     index projection references Local({}) but function only has \
                     {num_locals} locals",
                    idx_local.0
                ));
            }
        }
    }
}

/// Checks an operand's locals are in bounds.
fn check_operand(
    operand: &Operand,
    num_locals: usize,
    func_id: u32,
    block_id: BlockId,
    stmt_idx: usize,
    tag: &str,
    errors: &mut Vec<String>,
) {
    match operand {
        Operand::Copy(place) | Operand::Move(place) => {
            check_place(place, num_locals, func_id, block_id, stmt_idx, tag, errors);
        }
        Operand::Constant(_) => {}
    }
}

/// Validates an rvalue's structural integrity.
fn check_rvalue(
    rvalue: &Rvalue,
    num_locals: usize,
    func_id: u32,
    block_id: BlockId,
    stmt_idx: usize,
    errors: &mut Vec<String>,
) {
    match rvalue {
        Rvalue::ReadFutureResult { pointer } => check_place(
            &Place::from_local(*pointer),
            num_locals,
            func_id,
            block_id,
            stmt_idx,
            "future result pointer",
            errors,
        ),
        Rvalue::Use(op) | Rvalue::UnaryOp(_, op) | Rvalue::Cast { operand: op, .. } => {
            check_operand(
                op, num_locals, func_id, block_id, stmt_idx, "rvalue", errors,
            );
        }
        Rvalue::BinaryOp(_, lhs, rhs) => {
            check_operand(
                lhs,
                num_locals,
                func_id,
                block_id,
                stmt_idx,
                "binop.lhs",
                errors,
            );
            check_operand(
                rhs,
                num_locals,
                func_id,
                block_id,
                stmt_idx,
                "binop.rhs",
                errors,
            );
        }
        Rvalue::Ref { place, .. } => {
            check_place(
                place, num_locals, func_id, block_id, stmt_idx, "ref", errors,
            );
        }
        Rvalue::Aggregate(kind, operands) => {
            for (i, op) in operands.iter().enumerate() {
                check_operand(op, num_locals, func_id, block_id, stmt_idx, "agg", errors);
                let _ = i; // index available for future per-operand diagnostics
            }
            check_aggregate_kind(kind, operands.len(), func_id, block_id, stmt_idx, errors);
        }
        Rvalue::Discriminant(place) => {
            check_place(
                place, num_locals, func_id, block_id, stmt_idx, "discrim", errors,
            );
        }
        Rvalue::StringInterp(operands) => {
            for op in operands {
                check_operand(
                    op, num_locals, func_id, block_id, stmt_idx, "interp", errors,
                );
            }
        }
        Rvalue::ListRange {
            start, end, step, ..
        }
        | Rvalue::RangeLen {
            start, end, step, ..
        } => {
            for (operand, role) in [
                (start, "range.start"),
                (end, "range.end"),
                (step, "range.step"),
            ] {
                check_operand(
                    operand, num_locals, func_id, block_id, stmt_idx, role, errors,
                );
            }
        }
        Rvalue::CollectionGetOptional { collection, index } => {
            for (place, role) in [(collection, "lookup.collection"), (index, "lookup.index")] {
                check_place(place, num_locals, func_id, block_id, stmt_idx, role, errors);
            }
        }
    }
}

/// Validates an aggregate construction.
fn check_aggregate_kind(
    kind: &AggregateKind,
    operand_count: usize,
    func_id: u32,
    block_id: BlockId,
    stmt_idx: usize,
    errors: &mut Vec<String>,
) {
    match kind {
        AggregateKind::Struct { fields, .. } => {
            if fields.len() != operand_count {
                errors.push(format!(
                    "DefId({func_id})/{block_id}/stmt[{stmt_idx}]: \
                     struct aggregate has {} fields but {operand_count} operands",
                    fields.len()
                ));
            }
        }
        AggregateKind::Union { .. } => {
            if operand_count != 1 {
                errors.push(format!(
                    "DefId({func_id})/{block_id}/stmt[{stmt_idx}]: \
                     union aggregate should have exactly 1 operand, got {operand_count}"
                ));
            }
        }
        AggregateKind::Map => {
            if !operand_count.is_multiple_of(2) {
                errors.push(format!(
                    "DefId({func_id})/{block_id}/stmt[{stmt_idx}]: \
                     map aggregate has odd operand count ({operand_count})"
                ));
            }
        }
        AggregateKind::Channel => {
            if operand_count != 0 {
                errors.push(format!(
                    "DefId({func_id})/{block_id}/stmt[{stmt_idx}]: \
                     channel aggregate should have 0 operands, got {operand_count}"
                ));
            }
        }
        AggregateKind::Tuple
        | AggregateKind::List
        | AggregateKind::Set
        | AggregateKind::EnumVariant { .. }
        | AggregateKind::Closure { .. } => {}
    }
}

/// Validates a block terminator.
fn check_terminator(
    term: &Terminator,
    num_locals: usize,
    num_blocks: usize,
    func_id: u32,
    block_id: BlockId,
    errors: &mut Vec<String>,
) {
    // Use a sentinel stmt_idx for terminator context.
    let stmt_idx = usize::MAX;

    match term {
        Terminator::Goto { target } => {
            check_block_ref(*target, num_blocks, func_id, block_id, errors);
        }
        Terminator::Return | Terminator::Unreachable => {}
        Terminator::SwitchInt {
            discriminant,
            targets,
            otherwise,
        } => {
            check_operand(
                discriminant,
                num_locals,
                func_id,
                block_id,
                stmt_idx,
                "switch",
                errors,
            );
            for (_, target) in targets {
                check_block_ref(*target, num_blocks, func_id, block_id, errors);
            }
            check_block_ref(*otherwise, num_blocks, func_id, block_id, errors);
        }
        Terminator::Call {
            func,
            args,
            destination,
            target,
            ..
        } => {
            check_operand(
                func,
                num_locals,
                func_id,
                block_id,
                stmt_idx,
                "call.func",
                errors,
            );
            for arg in args {
                check_operand(
                    arg, num_locals, func_id, block_id, stmt_idx, "call.arg", errors,
                );
            }
            check_place(
                destination,
                num_locals,
                func_id,
                block_id,
                stmt_idx,
                "call.dest",
                errors,
            );
            check_block_ref(*target, num_blocks, func_id, block_id, errors);
        }
    }
}

/// Checks that a block ID is within the function's block list.
fn check_block_ref(
    id: BlockId,
    num_blocks: usize,
    func_id: u32,
    block_id: BlockId,
    errors: &mut Vec<String>,
) {
    if id.index() >= num_blocks {
        errors.push(format!(
            "DefId({func_id})/{block_id}: references {id} but function only has {num_blocks} blocks"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::{Local, LocalDecl, MirFunction};
    use spark_resolve::DefId;
    use spark_span::Span;
    use spark_types::TypeId;

    fn dummy_func(num_locals: usize, blocks: Vec<BasicBlock>) -> MirFunction {
        let locals = (0..num_locals)
            .map(|i| LocalDecl {
                local: Local(i as u32),
                ty: TypeId::ERROR,
                name: None,
                is_user_variable: false,
                mutable: false,
                span: Span::DUMMY,
            })
            .collect();

        MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals,
            blocks,
            captures: Vec::new(),
            is_async: false,
            span: Span::DUMMY,
        }
    }

    #[test]
    fn valid_minimal_function() {
        let func = dummy_func(
            1,
            vec![BasicBlock {
                statements: vec![],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
        );
        let mir = Mir {
            functions: std::iter::once((DefId::new(0), func)).collect(),
        };
        let errors = validate(&mir, &TypeTable::new());
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    #[test]
    fn checked_lookup_requires_optional_of_the_collection_element() {
        let mut table = TypeTable::new();
        let int = table.intern(Ty::Int(spark_types::IntWidth::I64));
        let bool_ty = table.intern(Ty::Bool);
        let list = table.intern(Ty::List(int));
        let optional_bool = table.intern(Ty::Optional(bool_ty));
        let optional_int = table.intern(Ty::Optional(int));
        let mut func = dummy_func(
            3,
            vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::CollectionGetOptional {
                        collection: Place::from_local(Local(1)),
                        index: Place::from_local(Local(2)),
                    },
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
        );
        func.locals[1].ty = list;
        func.locals[2].ty = int;

        let field = spark_intern::sym("items");
        let state = table.intern(Ty::AsyncState {
            func_def_id: DefId::new(0),
            fields: vec![(field, list)],
        });
        let reference = table.intern(Ty::Ref {
            inner: state,
            mutable: true,
            lifetime: spark_types::ty::LifetimeId::ANONYMOUS,
        });
        for projected in [false, true] {
            func.locals[1].ty = if projected { reference } else { list };
            let Statement::Assign {
                rvalue: Rvalue::CollectionGetOptional { collection, .. },
                ..
            } = &mut func.blocks[0].statements[0]
            else {
                unreachable!()
            };
            collection.projections = if projected {
                vec![Projection::Deref, Projection::Field(field)]
            } else {
                vec![]
            };
            for (dest_ty, is_valid) in [(optional_bool, false), (optional_int, true)] {
                func.locals[0].ty = dest_ty;
                let mut errors = Vec::new();
                validate_function(&func, &table, &mut errors);
                assert_eq!(
                    errors.is_empty(),
                    is_valid,
                    "projected={projected}: {errors:?}"
                );
            }
        }
    }

    #[test]
    fn detects_out_of_bounds_local() {
        let func = dummy_func(
            1,
            vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(5)),
                    rvalue: Rvalue::Use(Operand::Constant(crate::mir::Constant::Int(0))),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
        );
        let mir = Mir {
            functions: std::iter::once((DefId::new(0), func)).collect(),
        };
        let errors = validate(&mir, &TypeTable::new());
        assert!(!errors.is_empty(), "should detect out-of-bounds local");
        assert!(errors[0].contains("Local(5)"));
    }

    #[test]
    fn detects_out_of_bounds_block_target() {
        let func = dummy_func(
            1,
            vec![BasicBlock {
                statements: vec![],
                terminator: Terminator::Goto {
                    target: BlockId(99),
                },
                is_cleanup: false,
            }],
        );
        let mir = Mir {
            functions: std::iter::once((DefId::new(0), func)).collect(),
        };
        let errors = validate(&mir, &TypeTable::new());
        assert!(!errors.is_empty(), "should detect out-of-bounds block");
        assert!(errors[0].contains("bb99"));
    }

    #[test]
    fn detects_struct_field_operand_mismatch() {
        use spark_intern::kw;
        let func = dummy_func(
            2,
            vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::Aggregate(
                        AggregateKind::Struct {
                            def_id: DefId::new(1),
                            fields: vec![kw::FN, kw::STRUCT],
                        },
                        vec![Operand::Constant(crate::mir::Constant::Int(1))],
                    ),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
        );
        let mir = Mir {
            functions: std::iter::once((DefId::new(0), func)).collect(),
        };
        let errors = validate(&mir, &TypeTable::new());
        assert!(!errors.is_empty(), "should detect field/operand mismatch");
        assert!(errors[0].contains("2 fields but 1 operands"));
    }

    #[test]
    fn detects_empty_function() {
        let func = dummy_func(1, vec![]);
        let mir = Mir {
            functions: std::iter::once((DefId::new(0), func)).collect(),
        };
        let errors = validate(&mir, &TypeTable::new());
        assert!(!errors.is_empty(), "should detect empty function");
        assert!(errors[0].contains("no blocks"));
    }

    #[test]
    fn detects_union_aggregate_wrong_operand_count() {
        let func = dummy_func(
            2,
            vec![BasicBlock {
                statements: vec![Statement::Assign {
                    place: Place::from_local(Local(0)),
                    rvalue: Rvalue::Aggregate(
                        AggregateKind::Union {
                            union_ty: TypeId::ERROR,
                            member_ty: TypeId::ERROR,
                        },
                        vec![
                            Operand::Constant(crate::mir::Constant::Int(1)),
                            Operand::Constant(crate::mir::Constant::Int(2)),
                        ],
                    ),
                    span: Span::DUMMY,
                }],
                terminator: Terminator::Return,
                is_cleanup: false,
            }],
        );
        let mir = Mir {
            functions: std::iter::once((DefId::new(0), func)).collect(),
        };
        let errors = validate(&mir, &TypeTable::new());
        assert!(
            !errors.is_empty(),
            "should detect wrong union operand count"
        );
        assert!(errors[0].contains("exactly 1 operand"));
    }
}
