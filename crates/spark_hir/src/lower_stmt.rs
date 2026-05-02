//! Statement lowering.
//!
//! Converts AST [`Stmt`] nodes into HIR [`HirStmt`] nodes.
//! `CompoundAssign` is desugared into `Assign` + `BinaryOp`.

use spark_ast::{BinaryOp, CompoundOp, Stmt, StmtKind};
use spark_types::ty::Ty;
use spark_types::{TypeId, TypeTable};

use crate::hir::{HirExpr, HirExprKind, HirStmt};
use crate::lower_expr::{auto_deref_ref, coerce_ref, lower_expr, LowerCtx};

/// Lowers a statement from the AST to HIR.
#[allow(clippy::too_many_lines)]
pub(crate) fn lower_stmt(stmt: &Stmt, ctx: &mut LowerCtx<'_>) -> HirStmt {
    let table = ctx.table;
    match &stmt.kind {
        StmtKind::Let {
            name,
            ty: _,
            mutable,
            value,
        } => {
            let def_id = ctx
                .resolutions
                .and_then(|res| res.get_binding(name.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);

            let mut lowered_value = lower_expr(value, ctx);

            let declared_info = table
                .get_def_type(def_id)
                .map(|ty| (ty, strip_mutable(ty, table)));

            // Collection element coercion: if declared type is List[Ref[T]] and
            // elements are T, wrap each element with Borrow/Deref as needed.
            if let Some((_, stripped)) = declared_info {
                match (table.resolve(stripped), &mut lowered_value.kind) {
                    (Ty::List(expected_elem), HirExprKind::ListLit(elems))
                    | (Ty::Set(expected_elem), HirExprKind::SetLit(elems)) => {
                        coerce_collection_elems(elems, *expected_elem, value.span, table);
                    }
                    (Ty::Map(expected_key, expected_val), HirExprKind::MapLit(entries)) => {
                        let expected_key = *expected_key;
                        let expected_val = *expected_val;
                        *entries = std::mem::take(entries)
                            .into_iter()
                            .map(|(k, v)| {
                                (
                                    coerce_ref(k, expected_key, value.span, table),
                                    coerce_ref(v, expected_val, value.span, table),
                                )
                            })
                            .collect();
                    }
                    _ => {}
                }
            }

            // Auto-borrow: if declared type is Ref[T] and value is T, wrap in Borrow
            if let Some((declared_ty, stripped_declared)) = declared_info {
                if let Ty::Ref {
                    mutable: ref_mut, ..
                } = table.resolve(stripped_declared)
                {
                    let value_stripped = strip_mutable(lowered_value.ty, table);
                    if !matches!(table.resolve(value_stripped), Ty::Ref { .. })
                        && !lowered_value.ty.is_error()
                    {
                        // Ref[T]! (Mutable(Ref)) means a mutable reference — the
                        // `!` suffix on Ref controls borrow mutability.
                        let is_outer_mutable = matches!(table.resolve(declared_ty), Ty::Mutable(_));
                        let borrow_mutable = *ref_mut || is_outer_mutable;
                        lowered_value = HirExpr::new(
                            HirExprKind::Borrow {
                                operand: Box::new(lowered_value),
                                mutable: borrow_mutable,
                            },
                            declared_ty,
                            value.span,
                        );
                    }
                }
            }

            // Auto-deref: if declared type is T and value is Ref[T], wrap in Deref
            let value_stripped = strip_mutable(lowered_value.ty, table);
            if let Ty::Ref { inner, .. } = table.resolve(value_stripped) {
                let inner_ty = *inner;
                if let Some((_, decl_stripped)) = declared_info {
                    if !matches!(table.resolve(decl_stripped), Ty::Ref { .. }) {
                        lowered_value = HirExpr::new(
                            HirExprKind::Deref(Box::new(lowered_value)),
                            inner_ty,
                            value.span,
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
            let mut target_hir = lower_expr(target, ctx);
            let mut value_hir = lower_expr(value, ctx);

            let target_inner = strip_mutable(target_hir.ty, table);
            let value_ty = value_hir.ty;

            // Deref-store: target is Ref[T], value is T → write through the ref
            if let Ty::Ref { inner, .. } = table.resolve(target_inner) {
                if !matches!(table.resolve(value_ty), Ty::Ref { .. }) && !value_ty.is_error() {
                    let inner_ty = *inner;
                    target_hir = HirExpr::new(
                        HirExprKind::Deref(Box::new(target_hir)),
                        inner_ty,
                        target.span,
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
                        inner_ty,
                        value.span,
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
            let target_hir = lower_expr(target, ctx);
            let value_hir = lower_expr(value, ctx);

            // Re-lower target for the RHS (we need two copies)
            let target_for_rhs = lower_expr(target, ctx);

            // Auto-deref operands for the binary op (so Ref[Int] + Int works)
            let target_rhs_deref = auto_deref_ref(target_for_rhs, table);
            let value_deref = auto_deref_ref(value_hir, table);

            // Result type is the deref'd type (e.g. Int, not Ref[Int])
            let result_ty = strip_ref(strip_mutable(target_hir.ty, table), table);

            let binary_op = compound_to_binary(*op);
            let rhs = HirExpr::new(
                HirExprKind::BinaryOp {
                    op: binary_op,
                    lhs: Box::new(target_rhs_deref),
                    rhs: Box::new(value_deref),
                },
                result_ty,
                stmt.span,
            );

            // If target is Ref[T], deref for the store (write through the reference)
            let target_inner = strip_mutable(target_hir.ty, table);
            let store_target = if let Ty::Ref { inner, .. } = table.resolve(target_inner) {
                let inner_ty = *inner;
                HirExpr::new(
                    HirExprKind::Deref(Box::new(target_hir)),
                    inner_ty,
                    stmt.span,
                )
            } else {
                target_hir
            };

            HirStmt::Assign {
                target: store_target,
                value: rhs,
            }
        }

        StmtKind::Expr(expr) => HirStmt::Expr(lower_expr(expr, ctx)),
    }
}

/// Coerces each element in a collection literal to the expected element type.
fn coerce_collection_elems(
    elems: &mut Vec<HirExpr>,
    expected_elem: TypeId,
    span: spark_span::Span,
    table: &TypeTable,
) {
    *elems = std::mem::take(elems)
        .into_iter()
        .map(|e| coerce_ref(e, expected_elem, span, table))
        .collect();
}

/// Strips the `Mutable` wrapper from a type if present.
fn strip_mutable(ty: TypeId, table: &TypeTable) -> TypeId {
    if let Ty::Mutable(inner) = table.resolve(ty) {
        *inner
    } else {
        ty
    }
}

/// Strips the `Ref` wrapper from a type if present.
fn strip_ref(ty: TypeId, table: &TypeTable) -> TypeId {
    if let Ty::Ref { inner, .. } = table.resolve(ty) {
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
    use spark_ast::{Expr, ExprKind, Ident, Literal};
    use spark_diagnostics::Diagnostics;
    use spark_intern::sym;
    use spark_resolve::{DefId, ResolutionMap};
    use spark_span::Span;
    use spark_types::ty::{LifetimeId, Ty};

    #[test]
    fn compound_to_binary_all() {
        assert!(matches!(compound_to_binary(CompoundOp::Add), BinaryOp::Add));
        assert!(matches!(compound_to_binary(CompoundOp::Sub), BinaryOp::Sub));
        assert!(matches!(compound_to_binary(CompoundOp::Mul), BinaryOp::Mul));
        assert!(matches!(compound_to_binary(CompoundOp::Div), BinaryOp::Div));
    }

    #[test]
    fn compound_assign_ref_desugars_with_deref() {
        let mut table = spark_types::TypeTable::new();
        let builtins = spark_types::builtins::register_builtins(&mut table);
        let mut defs = spark_resolve::DefinitionTable::new();
        let mut diags = Diagnostics::new();

        // Create types: Mutable(Ref[Int, mutable=true])
        let int_id = builtins.int;
        let ref_ty = table.intern(Ty::Ref {
            inner: int_id,
            mutable: true,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let mut_ref_ty = table.intern(Ty::Mutable(ref_ty));

        // Set up def for the target variable
        let def_id = DefId::new(50);
        table.set_def_type(def_id, mut_ref_ty);

        // Create target ident expression
        let target_span = Span::new(spark_span::FileId::new(0), 0, 1);
        let target = Expr::new(
            ExprKind::Ident(Ident::new(sym("x"), target_span)),
            target_span,
        );
        // Set expression type in the table (as the type checker would)
        table.set_expr_type(target.id, mut_ref_ty);

        // Create value literal
        let value = Expr::new(ExprKind::Literal(Literal::int(1, Span::DUMMY)), Span::DUMMY);
        table.set_expr_type(value.id, int_id);

        let mut res = ResolutionMap::new();
        res.record_ident(target_span, def_id);

        let stmt = Stmt::new(
            StmtKind::CompoundAssign {
                target,
                op: CompoundOp::Add,
                value,
            },
            Span::DUMMY,
        );

        let mut ctx = LowerCtx {
            table: &table,
            definitions: &mut defs,
            resolutions: Some(&res),
            builtins: &builtins,
            diagnostics: &mut diags,
            current_fn_ret: None,
        };
        let hir = lower_stmt(&stmt, &mut ctx);

        // The result should be an Assign with a Deref target (write through ref)
        match &hir {
            HirStmt::Assign { target, value } => {
                // Target should be Deref (writing through the reference)
                assert!(
                    matches!(target.kind, HirExprKind::Deref(_)),
                    "Expected Deref target for Ref[T], got {:?}",
                    target.kind
                );
                // Value should be a BinaryOp
                assert!(
                    matches!(value.kind, HirExprKind::BinaryOp { .. }),
                    "Expected BinaryOp value, got {:?}",
                    value.kind
                );
            }
            other => panic!("Expected Assign, got {other:?}"),
        }
    }

    #[test]
    fn compound_assign_bare_value_unchanged() {
        let mut table = spark_types::TypeTable::new();
        let builtins = spark_types::builtins::register_builtins(&mut table);
        let mut defs = spark_resolve::DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let int_id = builtins.int;
        let mut_int = table.intern(Ty::Mutable(int_id));

        let def_id = DefId::new(51);
        table.set_def_type(def_id, mut_int);

        let target_span = Span::new(spark_span::FileId::new(0), 5, 6);
        let target = Expr::new(
            ExprKind::Ident(Ident::new(sym("y"), target_span)),
            target_span,
        );
        table.set_expr_type(target.id, mut_int);

        let value = Expr::new(ExprKind::Literal(Literal::int(2, Span::DUMMY)), Span::DUMMY);
        table.set_expr_type(value.id, int_id);

        let mut res = ResolutionMap::new();
        res.record_ident(target_span, def_id);

        let stmt = Stmt::new(
            StmtKind::CompoundAssign {
                target,
                op: CompoundOp::Add,
                value,
            },
            Span::DUMMY,
        );

        let mut ctx = LowerCtx {
            table: &table,
            definitions: &mut defs,
            resolutions: Some(&res),
            builtins: &builtins,
            diagnostics: &mut diags,
            current_fn_ret: None,
        };
        let hir = lower_stmt(&stmt, &mut ctx);

        // Target should NOT be wrapped in Deref (it's a plain Mutable(Int))
        match &hir {
            HirStmt::Assign { target, .. } => {
                assert!(
                    !matches!(target.kind, HirExprKind::Deref(_)),
                    "Plain Int target should not be wrapped in Deref"
                );
            }
            other => panic!("Expected Assign, got {other:?}"),
        }
    }
}
