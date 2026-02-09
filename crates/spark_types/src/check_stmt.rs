//! Statement type checking.
//!
//! Checks let bindings, assignments, compound assignments, and
//! expression statements.

use spark_ast::{Stmt, StmtKind};
use spark_diagnostics::{Diagnostic, Label};
use spark_resolve::{DefinitionTable, ResolutionMap};

use crate::builtins::BuiltinTypes;
use crate::check_expr::{check_expr, check_expr_expected, is_compatible, type_name};
use crate::lower::lower_type_expr;
use crate::table::TypeTable;
use crate::ty::{Ty, TypeId};

/// Type-checks a statement.
#[allow(clippy::too_many_arguments)]
pub fn check_stmt(
    stmt: &Stmt,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
    current_fn_ret: Option<TypeId>,
) {
    match &stmt.kind {
        StmtKind::Let { name, ty, mutable: _, value } => {
            // Lower the declared type
            let declared_ty = lower_type_expr(ty, table, builtins, definitions, resolutions, diagnostics);

            // Check the value expression, passing the declared type as context
            // so that propagation (!) can use it to split union types.
            let value_ty = check_expr_expected(value, table, builtins, definitions, resolutions, diagnostics, current_fn_ret, Some(declared_ty));

            // Verify compatibility
            if !is_compatible(value_ty, declared_ty, table) {
                let expected = type_name(declared_ty, table, definitions);
                let found = type_name(value_ty, table, definitions);
                diagnostics.push(
                    Diagnostic::error(format!(
                        "type mismatch in `{}`: expected `{expected}`, found `{found}`",
                        name.name
                    ))
                    .with_label(Label::primary(value.span, format!("expected `{expected}`"))),
                );
            }

            // Register the local's type.
            // The declared type is the source of truth: `Int!` already lowers
            // to `Mutable(Int)`, so we must NOT wrap again based on the
            // `mutable` flag (which is derived from the same type annotation).
            let local_ty = declared_ty;

            // Try to find the DefId for this local
            if let Some(res) = resolutions {
                if let Some(def_id) = res.get_binding(name.span) {
                    table.set_def_type(def_id, local_ty);
                }
            }
        }

        StmtKind::Assign { target, value } => {
            let target_ty = check_expr(target, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let value_ty = check_expr(value, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);

            // Target must be mutable
            if !target_ty.is_error() && !matches!(table.resolve(target_ty), Ty::Mutable(_)) {
                diagnostics.push(
                    Diagnostic::error("cannot assign to immutable variable")
                        .with_label(Label::primary(target.span, "not mutable (declare with `Type!` for mutability)")),
                );
            }

            // Check type compatibility (strip mutable wrapper for comparison)
            let inner_ty = if let Ty::Mutable(inner) = table.resolve(target_ty) {
                *inner
            } else {
                target_ty
            };

            if !is_compatible(value_ty, inner_ty, table) {
                let expected = type_name(inner_ty, table, definitions);
                let found = type_name(value_ty, table, definitions);
                diagnostics.push(
                    Diagnostic::error(format!("assignment type mismatch: expected `{expected}`, found `{found}`"))
                        .with_label(Label::primary(value.span, format!("expected `{expected}`"))),
                );
            }
        }

        StmtKind::CompoundAssign { target, op, value } => {
            let target_ty = check_expr(target, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let value_ty = check_expr(value, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);

            // Target must be mutable
            if !target_ty.is_error() && !matches!(table.resolve(target_ty), Ty::Mutable(_)) {
                diagnostics.push(
                    Diagnostic::error(format!("cannot use `{}` on immutable variable", op.as_str()))
                        .with_label(Label::primary(target.span, "not mutable")),
                );
            }

            // Strip mutable for type checking
            let inner_ty = if let Ty::Mutable(inner) = table.resolve(target_ty) {
                *inner
            } else {
                target_ty
            };

            // Both operands must be numeric
            if !inner_ty.is_error() && !table.resolve(inner_ty).is_numeric() {
                diagnostics.push(
                    Diagnostic::error(format!("operator `{}` requires numeric operands", op.as_str()))
                        .with_label(Label::primary(target.span, "not numeric")),
                );
            }

            if !is_compatible(value_ty, inner_ty, table) {
                let expected = type_name(inner_ty, table, definitions);
                let found = type_name(value_ty, table, definitions);
                diagnostics.push(
                    Diagnostic::error(format!("type mismatch for `{}`: expected `{expected}`, found `{found}`", op.as_str()))
                        .with_label(Label::primary(value.span, format!("expected `{expected}`"))),
                );
            }
        }

        StmtKind::Expr(expr) => {
            let _ = check_expr(expr, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_ast::*;
    use spark_diagnostics::Diagnostics;
    use spark_intern::sym;
    use spark_span::Span;

    fn int_type() -> TypeExpr {
        TypeExpr::new(
            TypeExprKind::Named {
                path: Path::from_ident(Ident::dummy(sym("Int"))),
                type_args: vec![],
            },
            Span::DUMMY,
        )
    }

    fn int_literal() -> Expr {
        Expr::new(
            ExprKind::Literal(Literal::int(42, Span::DUMMY)),
            Span::DUMMY,
        )
    }

    #[test]
    fn check_let_matching_type() {
        let mut table = crate::table::TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let stmt = Stmt::let_stmt(
            Ident::dummy(sym("x")),
            int_type(),
            false,
            int_literal(),
            Span::DUMMY,
        );

        check_stmt(&stmt, &mut table, &builtins, &defs, None, &mut diags, None);
        assert!(!diags.has_errors());
    }

    #[test]
    fn check_let_type_mismatch() {
        let mut table = crate::table::TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let string_type = TypeExpr::new(
            TypeExprKind::Named {
                path: Path::from_ident(Ident::dummy(sym("String"))),
                type_args: vec![],
            },
            Span::DUMMY,
        );

        let stmt = Stmt::let_stmt(
            Ident::dummy(sym("x")),
            string_type,
            false,
            int_literal(),
            Span::DUMMY,
        );

        check_stmt(&stmt, &mut table, &builtins, &defs, None, &mut diags, None);
        assert!(diags.has_errors());
    }

    #[test]
    fn check_expr_stmt() {
        let mut table = crate::table::TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let stmt = Stmt::expr(int_literal());
        check_stmt(&stmt, &mut table, &builtins, &defs, None, &mut diags, None);
        assert!(!diags.has_errors());
    }
}
