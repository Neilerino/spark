//! Statement type checking.
//!
//! Checks let bindings, assignments, compound assignments, and
//! expression statements.

use spark_ast::{Expr, ExprKind, Stmt, StmtKind};
use spark_diagnostics::{Diagnostic, Label};
use spark_resolve::DefinitionKind;

use crate::check_expr::{
    check_collection_argument, check_expr, check_expr_expected, collection_value_coercion,
    is_compatible, is_supported_map_key, strip_mutable, strip_ref,
};
use crate::ctx::TypeCheckCtx;
use crate::display::display_type as type_name;
use crate::lower::lower_type_expr;
use crate::ty::{Ty, TypeId};

/// Type-checks a statement.
pub fn check_stmt(stmt: &Stmt, ctx: &mut TypeCheckCtx<'_>, current_fn_ret: Option<TypeId>) {
    match &stmt.kind {
        StmtKind::Let {
            name,
            ty,
            mutable: _,
            value,
        } => {
            // Lower the declared type
            let declared_ty = lower_type_expr(ty, ctx);

            let value_ty = check_binding_initializer(value, declared_ty, ctx, current_fn_ret);

            // Verify compatibility
            if !is_compatible(value_ty, declared_ty, ctx.table) {
                let expected = type_name(declared_ty, ctx.table, ctx.definitions);
                let found = type_name(value_ty, ctx.table, ctx.definitions);
                ctx.diagnostics.push(
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
            if let Some(res) = ctx.resolutions {
                if let Some(def_id) = res.get_binding(name.span) {
                    ctx.table.set_def_type(def_id, local_ty);
                }
            }
        }

        StmtKind::Assign { target, value } => {
            check_assignment(target, value, ctx, current_fn_ret);
        }

        StmtKind::CompoundAssign { target, op, value } => {
            let (root, indices) = target.index_path();
            if !indices.is_empty() {
                check_index_target(root, &indices, ctx, current_fn_ret);
                check_expr(value, ctx, current_fn_ret);
                ctx.diagnostics.push(
                    Diagnostic::error("indexed compound assignment is unsupported")
                        .with_label(Label::primary(target.span, "use plain indexed assignment")),
                );
                return;
            }
            let target_ty = check_expr(target, ctx, current_fn_ret);
            let value_ty = check_expr(value, ctx, current_fn_ret);

            // Target must be mutable
            if !target_ty.is_error() && !matches!(ctx.table.resolve(target_ty), Ty::Mutable(_)) {
                ctx.diagnostics.push(
                    Diagnostic::error(format!(
                        "cannot use `{}` on immutable variable",
                        op.as_str()
                    ))
                    .with_label(Label::primary(target.span, "not mutable")),
                );
            }

            // Strip mutable, then strip Ref for type checking
            let stripped_mut = if let Ty::Mutable(inner) = ctx.table.resolve(target_ty) {
                *inner
            } else {
                target_ty
            };
            let inner_ty = strip_ref(stripped_mut, ctx.table);

            // Both operands must be numeric
            if !inner_ty.is_error() && !ctx.table.resolve(inner_ty).is_numeric() {
                ctx.diagnostics.push(
                    Diagnostic::error(format!(
                        "operator `{}` requires numeric operands",
                        op.as_str()
                    ))
                    .with_label(Label::primary(target.span, "not numeric")),
                );
            }

            // Strip Ref from value type before compatibility check
            let value_base = strip_ref(strip_mutable(value_ty, ctx.table), ctx.table);
            if !is_compatible(value_base, inner_ty, ctx.table) {
                let expected = type_name(inner_ty, ctx.table, ctx.definitions);
                let found = type_name(value_ty, ctx.table, ctx.definitions);
                ctx.diagnostics.push(
                    Diagnostic::error(format!(
                        "type mismatch for `{}`: expected `{expected}`, found `{found}`",
                        op.as_str()
                    ))
                    .with_label(Label::primary(value.span, format!("expected `{expected}`"))),
                );
            }
        }

        StmtKind::Expr(expr) => {
            let _ = check_expr(expr, ctx, current_fn_ret);
        }
    }
}

fn check_assignment(
    target: &Expr,
    value: &Expr,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) {
    let (root, indices) = target.index_path();
    if !indices.is_empty() {
        let slot = check_index_target(root, &indices, ctx, current_fn_ret);
        ctx.table.set_expr_type(target.id, slot);
        let actual = check_expr_expected(value, ctx, current_fn_ret, Some(slot));
        if !actual.is_error()
            && !slot.is_error()
            && !matches!(ctx.table.resolve(actual), Ty::Never)
            && collection_value_coercion(actual, slot, ctx.table).is_none()
        {
            let expected = type_name(slot, ctx.table, ctx.definitions);
            let found = type_name(actual, ctx.table, ctx.definitions);
            ctx.diagnostics.push(
                Diagnostic::error(format!(
                    "assignment type mismatch: expected `{expected}`, found `{found}`"
                ))
                .with_label(Label::primary(value.span, format!("expected `{expected}`"))),
            );
        }
        return;
    }
    let target_ty = check_expr(target, ctx, current_fn_ret);
    let value_ty = check_expr(value, ctx, current_fn_ret);

    // Target must be mutable
    if !target_ty.is_error() && !matches!(ctx.table.resolve(target_ty), Ty::Mutable(_)) {
        ctx.diagnostics.push(
            Diagnostic::error("cannot assign to immutable variable").with_label(Label::primary(
                target.span,
                "not mutable (declare with `Type!` for mutability)",
            )),
        );
    }

    // Check type compatibility (strip mutable wrapper for comparison)
    let inner_ty = if let Ty::Mutable(inner) = ctx.table.resolve(target_ty) {
        *inner
    } else {
        target_ty
    };

    if !is_compatible(value_ty, inner_ty, ctx.table) {
        let expected = type_name(inner_ty, ctx.table, ctx.definitions);
        let found = type_name(value_ty, ctx.table, ctx.definitions);
        ctx.diagnostics.push(
            Diagnostic::error(format!(
                "assignment type mismatch: expected `{expected}`, found `{found}`"
            ))
            .with_label(Label::primary(value.span, format!("expected `{expected}`"))),
        );
    }
}

fn check_index_target(
    root: &Expr,
    indices: &[&Expr],
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    let root_ty = check_expr(root, ctx, current_fn_ret);
    let mut slot = match ctx.table.resolve(root_ty) {
        Ty::Mutable(inner) if is_binding_place(root, ctx) => *inner,
        Ty::Mutable(_) => {
            ctx.diagnostics.push(
                Diagnostic::error("indexed assignment requires a mutable binding").with_label(
                    Label::primary(root.span, "use a local or parameter binding"),
                ),
            );
            TypeId::ERROR
        }
        Ty::Error => TypeId::ERROR,
        _ => {
            ctx.diagnostics.push(
                Diagnostic::error("cannot assign to immutable variable").with_label(
                    Label::primary(root.span, "indexed writes require a mutable place"),
                ),
            );
            TypeId::ERROR
        }
    };

    for (position, index) in indices.iter().enumerate() {
        let index_ty = check_expr(index, ctx, current_fn_ret);
        slot = match ctx.table.resolve(strip_mutable(slot, ctx.table)) {
            Ty::List(elem) => {
                let elem = *elem;
                let resolved = ctx.table.resolve(strip_mutable(index_ty, ctx.table));
                if !resolved.is_integer() && !matches!(resolved, Ty::Error | Ty::Never) {
                    ctx.diagnostics.push(
                        Diagnostic::error("list index must be an integer")
                            .with_label(Label::primary(index.span, "expected an integer")),
                    );
                }
                elem
            }
            Ty::Map(key, value) => {
                let key = *key;
                let value = *value;
                if !matches!(ctx.table.resolve(index_ty), Ty::Never)
                    && !check_collection_argument(
                        index_ty,
                        key,
                        index.span,
                        "map key type mismatch",
                        ctx,
                    )
                {
                    TypeId::ERROR
                } else if position + 1 != indices.len() {
                    ctx.diagnostics.push(
                        Diagnostic::error("map entry must end an indexed assignment path")
                            .with_label(Label::primary(index.span, "cannot traverse a map entry")),
                    );
                    TypeId::ERROR
                } else if !is_supported_map_key(key, ctx.table) {
                    ctx.diagnostics.push(
                        Diagnostic::error("map write keys must be integers or strings")
                            .with_label(Label::primary(index.span, "unsupported map key type")),
                    );
                    TypeId::ERROR
                } else {
                    value
                }
            }
            Ty::Error => TypeId::ERROR,
            _ => {
                ctx.diagnostics.push(
                    Diagnostic::error("type is not indexable")
                        .with_label(Label::primary(index.span, "expected a list or map")),
                );
                TypeId::ERROR
            }
        };
    }
    strip_mutable(slot, ctx.table)
}

fn is_binding_place(expr: &Expr, ctx: &TypeCheckCtx<'_>) -> bool {
    match &expr.kind {
        ExprKind::Ident(ident) => ctx
            .resolutions
            .and_then(|resolutions| resolutions.get_ident(ident.span))
            .and_then(|id| ctx.definitions.get(id))
            .is_some_and(|info| {
                matches!(info.kind, DefinitionKind::Local | DefinitionKind::Parameter)
            }),
        _ => false,
    }
}

fn check_binding_initializer(
    value: &Expr,
    declared_ty: TypeId,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    let ty = strip_mutable(declared_ty, ctx.table);
    if matches!(ctx.table.resolve(ty), Ty::Map(..))
        && type_empty_map_initializer(value, ty, ctx.table)
    {
        ty
    } else {
        check_expr_expected(value, ctx, current_fn_ret, Some(declared_ty))
    }
}

fn type_empty_map_initializer(expr: &Expr, map_ty: TypeId, table: &mut crate::TypeTable) -> bool {
    match &expr.kind {
        ExprKind::Block(block) if block.stmts.is_empty() && block.expr.is_none() => {}
        ExprKind::Paren(inner) if type_empty_map_initializer(inner, map_ty, table) => {}
        _ => return false,
    }
    table.set_expr_type(expr.id, map_ty);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_ast::*;
    use spark_diagnostics::Diagnostics;
    use spark_intern::sym;
    use spark_resolve::DefinitionTable;
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

        let mut ctx = TypeCheckCtx::new(&mut table, &builtins, &defs, None, &mut diags);
        check_stmt(&stmt, &mut ctx, None);
        assert!(!ctx.diagnostics.has_errors());
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

        let mut ctx = TypeCheckCtx::new(&mut table, &builtins, &defs, None, &mut diags);
        check_stmt(&stmt, &mut ctx, None);
        assert!(ctx.diagnostics.has_errors());
    }

    #[test]
    fn check_expr_stmt() {
        let mut table = crate::table::TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        let stmt = Stmt::expr(int_literal());
        let mut ctx = TypeCheckCtx::new(&mut table, &builtins, &defs, None, &mut diags);
        check_stmt(&stmt, &mut ctx, None);
        assert!(!ctx.diagnostics.has_errors());
    }

    /// Helper: builds a compound assign statement with a target ident that
    /// resolves to the given `def_id` and an int-literal value.
    fn compound_assign_stmt(target_span: Span, op: spark_ast::CompoundOp) -> Stmt {
        let target = Expr::new(
            ExprKind::Ident(Ident::new(sym("x"), target_span)),
            target_span,
        );
        Stmt::new(
            StmtKind::CompoundAssign {
                target,
                op,
                value: int_literal(),
            },
            Span::DUMMY,
        )
    }

    #[test]
    fn compound_assign_ref_int_ok() {
        use crate::ty::LifetimeId;
        use spark_resolve::{DefId, ResolutionMap};

        let mut table = crate::table::TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        // Create Mutable(Ref[Int, mutable=true])
        let int_id = builtins.int;
        let ref_ty = table.intern(Ty::Ref {
            inner: int_id,
            mutable: true,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let mut_ref_ty = table.intern(Ty::Mutable(ref_ty));

        // Set up resolution: target ident at span(0,1) -> def_id 99
        let target_span = Span::new(spark_span::FileId::new(0), 0, 1);
        let def_id = DefId::new(99);
        let mut res = ResolutionMap::new();
        res.record_ident(target_span, def_id);
        table.set_def_type(def_id, mut_ref_ty);

        let stmt = compound_assign_stmt(target_span, spark_ast::CompoundOp::Add);
        let mut ctx = TypeCheckCtx::new(&mut table, &builtins, &defs, Some(&res), &mut diags);
        check_stmt(&stmt, &mut ctx, None);
        assert!(
            !ctx.diagnostics.has_errors(),
            "Compound assign on Ref[Int] should work: {:?}",
            ctx.diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn compound_assign_immutable_ref_error() {
        use crate::ty::LifetimeId;
        use spark_resolve::{DefId, ResolutionMap};

        let mut table = crate::table::TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        // Create non-mutable Ref[Int] (no Mutable wrapper)
        let int_id = builtins.int;
        let ref_ty = table.intern(Ty::Ref {
            inner: int_id,
            mutable: true,
            lifetime: LifetimeId::ANONYMOUS,
        });

        let target_span = Span::new(spark_span::FileId::new(0), 10, 11);
        let def_id = DefId::new(100);
        let mut res = ResolutionMap::new();
        res.record_ident(target_span, def_id);
        table.set_def_type(def_id, ref_ty); // NOT Mutable(...)

        let stmt = compound_assign_stmt(target_span, spark_ast::CompoundOp::Add);
        let mut ctx = TypeCheckCtx::new(&mut table, &builtins, &defs, Some(&res), &mut diags);
        check_stmt(&stmt, &mut ctx, None);
        assert!(
            ctx.diagnostics.has_errors(),
            "Compound assign on immutable variable should error"
        );
    }

    #[test]
    fn compound_assign_ref_non_numeric_error() {
        use crate::ty::LifetimeId;
        use spark_resolve::{DefId, ResolutionMap};

        let mut table = crate::table::TypeTable::new();
        let builtins = crate::builtins::register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let mut diags = Diagnostics::new();

        // Create Mutable(Ref[String, mutable=true]) — String is not numeric
        let string_id = builtins.string;
        let ref_ty = table.intern(Ty::Ref {
            inner: string_id,
            mutable: true,
            lifetime: LifetimeId::ANONYMOUS,
        });
        let mut_ref_ty = table.intern(Ty::Mutable(ref_ty));

        let target_span = Span::new(spark_span::FileId::new(0), 20, 21);
        let def_id = DefId::new(101);
        let mut res = ResolutionMap::new();
        res.record_ident(target_span, def_id);
        table.set_def_type(def_id, mut_ref_ty);

        let stmt = compound_assign_stmt(target_span, spark_ast::CompoundOp::Add);
        let mut ctx = TypeCheckCtx::new(&mut table, &builtins, &defs, Some(&res), &mut diags);
        check_stmt(&stmt, &mut ctx, None);
        assert!(
            ctx.diagnostics.has_errors(),
            "Compound assign on Ref[String] should error (not numeric)"
        );
    }
}
