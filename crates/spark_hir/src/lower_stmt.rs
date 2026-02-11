//! Statement lowering.
//!
//! Converts AST [`Stmt`] nodes into HIR [`HirStmt`] nodes.
//! `CompoundAssign` is desugared into `Assign` + `BinaryOp`.

use spark_ast::{BinaryOp, CompoundOp, Stmt, StmtKind};
use spark_diagnostics::Diagnostics;
use spark_resolve::{DefinitionTable, ResolutionMap};
use spark_types::ty::Ty;
use spark_types::{TypeId, TypeTable};

use crate::hir::{HirExpr, HirExprKind, HirStmt};
use crate::lower_expr::lower_expr;

/// Lowers a statement from the AST to HIR.
#[allow(clippy::too_many_arguments)]
pub fn lower_stmt(
    stmt: &Stmt,
    table: &TypeTable,
    definitions: &mut DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut Diagnostics,
    current_fn_ret: Option<TypeId>,
) -> HirStmt {
    match &stmt.kind {
        StmtKind::Let { name, ty: _, mutable, value } => {
            let def_id = resolutions
                .and_then(|res| res.get_binding(name.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);

            let mut lowered_value = lower_expr(value, table, definitions, resolutions, diagnostics, current_fn_ret);

            // Auto-borrow: if declared type is Ref[T] and value is T, wrap in Borrow
            if let Some(declared_ty) = table.get_def_type(def_id) {
                let stripped_declared = strip_mutable(declared_ty, table);
                if let Ty::Ref { mutable: ref_mut, .. } = table.resolve(stripped_declared) {
                    let value_stripped = strip_mutable(lowered_value.ty, table);
                    if !matches!(table.resolve(value_stripped), Ty::Ref { .. })
                        && !lowered_value.ty.is_error()
                    {
                        let ref_mut = *ref_mut;
                        lowered_value = HirExpr::new(
                            HirExprKind::Borrow { operand: Box::new(lowered_value), mutable: ref_mut },
                            declared_ty, value.span,
                        );
                    }
                }
            }

            // Auto-deref: if declared type is T and value is Ref[T], wrap in Deref
            let value_stripped = strip_mutable(lowered_value.ty, table);
            if let Ty::Ref { inner, .. } = table.resolve(value_stripped) {
                let inner_ty = *inner;
                if let Some(declared_ty) = table.get_def_type(def_id) {
                    let decl_stripped = strip_mutable(declared_ty, table);
                    if !matches!(table.resolve(decl_stripped), Ty::Ref { .. }) {
                        lowered_value = HirExpr::new(
                            HirExprKind::Deref(Box::new(lowered_value)),
                            inner_ty, value.span,
                        );
                    }
                }
            }

            HirStmt::Let {
                def_id,
                mutable: *mutable,
                value: lowered_value,
            }
        }

        StmtKind::Assign { target, value } => {
            let mut target_hir = lower_expr(target, table, definitions, resolutions, diagnostics, current_fn_ret);
            let mut value_hir = lower_expr(value, table, definitions, resolutions, diagnostics, current_fn_ret);

            let target_inner = strip_mutable(target_hir.ty, table);
            let value_ty = value_hir.ty;

            // Deref-store: target is Ref[T], value is T → write through the ref
            if let Ty::Ref { inner, .. } = table.resolve(target_inner) {
                if !matches!(table.resolve(value_ty), Ty::Ref { .. }) && !value_ty.is_error() {
                    let inner_ty = *inner;
                    target_hir = HirExpr::new(
                        HirExprKind::Deref(Box::new(target_hir)),
                        inner_ty, target.span,
                    );
                }
            }

            // Auto-deref value: target is T, value is Ref[T]
            let target_stripped = strip_mutable(target_hir.ty, table);
            if !matches!(table.resolve(target_stripped), Ty::Ref { .. }) {
                if let Ty::Ref { inner, .. } = table.resolve(value_hir.ty) {
                    let inner_ty = *inner;
                    value_hir = HirExpr::new(
                        HirExprKind::Deref(Box::new(value_hir)),
                        inner_ty, value.span,
                    );
                }
            }

            HirStmt::Assign {
                target: target_hir,
                value: value_hir,
            }
        }

        StmtKind::CompoundAssign { target, op, value } => {
            // Desugar: `x += y` -> `x = x + y`
            let target_hir = lower_expr(target, table, definitions, resolutions, diagnostics, current_fn_ret);
            let value_hir = lower_expr(value, table, definitions, resolutions, diagnostics, current_fn_ret);

            // Re-lower target for the RHS (we need two copies)
            let target_for_rhs = lower_expr(target, table, definitions, resolutions, diagnostics, current_fn_ret);

            let binary_op = compound_to_binary(*op);
            let rhs = HirExpr::new(
                HirExprKind::BinaryOp {
                    op: binary_op,
                    lhs: Box::new(target_for_rhs),
                    rhs: Box::new(value_hir),
                },
                target_hir.ty,
                stmt.span,
            );

            HirStmt::Assign {
                target: target_hir,
                value: rhs,
            }
        }

        StmtKind::Expr(expr) => {
            HirStmt::Expr(lower_expr(expr, table, definitions, resolutions, diagnostics, current_fn_ret))
        }
    }
}

/// Strips the `Mutable` wrapper from a type if present.
fn strip_mutable(ty: TypeId, table: &TypeTable) -> TypeId {
    if let Ty::Mutable(inner) = table.resolve(ty) {
        *inner
    } else {
        ty
    }
}

/// Converts a `CompoundOp` to its corresponding `BinaryOp`.
const fn compound_to_binary(op: CompoundOp) -> BinaryOp {
    match op {
        CompoundOp::Add => BinaryOp::Add,
        CompoundOp::Sub => BinaryOp::Sub,
        CompoundOp::Mul => BinaryOp::Mul,
        CompoundOp::Div => BinaryOp::Div,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compound_to_binary_all() {
        assert!(matches!(compound_to_binary(CompoundOp::Add), BinaryOp::Add));
        assert!(matches!(compound_to_binary(CompoundOp::Sub), BinaryOp::Sub));
        assert!(matches!(compound_to_binary(CompoundOp::Mul), BinaryOp::Mul));
        assert!(matches!(compound_to_binary(CompoundOp::Div), BinaryOp::Div));
    }
}
