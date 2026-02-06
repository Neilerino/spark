//! Statement lowering.
//!
//! Converts AST [`Stmt`] nodes into HIR [`HirStmt`] nodes.
//! `CompoundAssign` is desugared into `Assign` + `BinaryOp`.

use spark_ast::{BinaryOp, CompoundOp, Stmt, StmtKind};
use spark_diagnostics::Diagnostics;
use spark_resolve::{DefinitionTable, ResolutionMap};
use spark_types::{TypeId, TypeTable};

use crate::hir::{HirExpr, HirExprKind, HirStmt};
use crate::lower_expr::lower_expr;

/// Lowers a statement from the AST to HIR.
#[allow(clippy::too_many_arguments)]
pub fn lower_stmt(
    stmt: &Stmt,
    table: &TypeTable,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut Diagnostics,
    current_fn_ret: Option<TypeId>,
) -> HirStmt {
    match &stmt.kind {
        StmtKind::Let { name, ty: _, mutable, value } => {
            let def_id = resolutions
                .and_then(|res| res.get_binding(name.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);

            HirStmt::Let {
                def_id,
                mutable: *mutable,
                value: lower_expr(value, table, definitions, resolutions, diagnostics, current_fn_ret),
            }
        }

        StmtKind::Assign { target, value } => HirStmt::Assign {
            target: lower_expr(target, table, definitions, resolutions, diagnostics, current_fn_ret),
            value: lower_expr(value, table, definitions, resolutions, diagnostics, current_fn_ret),
        },

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
