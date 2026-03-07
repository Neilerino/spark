//! Expression type checking (Pass 2).
//!
//! Walks expression trees and returns the type of each expression,
//! recording the result in the type table's `expr_types` map.

use spark_ast::{
    BinaryOp, Block, Expr, ExprKind, LiteralKind, PipelineStageKind, StringPart, UnaryOp,
};
use spark_diagnostics::{Diagnostic, Label};
use spark_intern::{kw, Symbol};
use spark_resolve::{DefId, DefinitionKind, DefinitionTable, ResolutionMap};
use spark_span::Span;

use crate::builtins::BuiltinTypes;
use crate::check_stmt::check_stmt;
use crate::ctx::TypeCheckCtx;
use crate::diagnostics;
use crate::display::display_type as type_name;
use crate::lower::lower_type_expr;
use crate::table::TypeTable;
use crate::ty::{Ty, TypeId};

/// Type-checks an expression and returns its type.
///
/// Records the resulting `TypeId` in `ctx.table.expr_types` keyed by the
/// expression's span.
pub fn check_expr(
    expr: &Expr,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    check_expr_expected(expr, ctx, current_fn_ret, None)
}

/// Type-checks an expression with an expected type hint from context.
///
/// The `expected_ty` is used by propagation (`!`) to determine which union
/// members are the "success" type vs error types to propagate.
pub fn check_expr_expected(
    expr: &Expr,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
    expected_ty: Option<TypeId>,
) -> TypeId {
    let ty = check_expr_inner(expr, ctx, current_fn_ret, expected_ty);
    ctx.table.set_expr_type(expr.id, ty);
    ty
}

/// Inner expression type checker — dispatches on `ExprKind`.
#[allow(clippy::too_many_lines)]
fn check_expr_inner(
    expr: &Expr,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
    expected_ty: Option<TypeId>,
) -> TypeId {
    match &expr.kind {
        // === Literals ===
        ExprKind::Literal(lit) => match &lit.kind {
            LiteralKind::Int(_) => ctx.builtins.int,
            LiteralKind::Float(_) => ctx.builtins.float,
            LiteralKind::String(s) => {
                // Type-check each interpolated sub-expression.
                for part in &s.parts {
                    if let StringPart::Interpolation(sub_expr) = part {
                        let sub_ty = check_expr(sub_expr, ctx, current_fn_ret);
                        // Verify the type supports string conversion.
                        let resolved = ctx.table.resolve(sub_ty);
                        let ok = matches!(
                            resolved,
                            Ty::String
                                | Ty::Int(_)
                                | Ty::Float(_)
                                | Ty::Bool
                                | Ty::Char
                                | Ty::Error
                        );
                        if !ok {
                            let name = type_name(sub_ty, ctx.table, ctx.definitions);
                            ctx.diagnostics.push(
                                Diagnostic::error("type cannot be interpolated into a string")
                                    .with_label(Label::primary(
                                        sub_expr.span,
                                        format!("type `{name}` does not support string interpolation"),
                                    ))
                                    .with_note("only String, Int, Float, Bool, and Char can be interpolated"),
                            );
                        }
                    }
                }
                ctx.builtins.string
            }
            LiteralKind::Char(_) => ctx.builtins.char_ty,
            LiteralKind::Bool(_) => ctx.builtins.bool_ty,
            LiteralKind::Null => ctx.builtins.null,
        },

        // === Identifiers ===
        ExprKind::Ident(ident) => {
            if let Some(res) = ctx.resolutions {
                if let Some(def_id) = res.get_ident(ident.span) {
                    return ctx.table.get_def_type(def_id).unwrap_or(TypeId::ERROR);
                }
            }
            // Unresolved identifier — the resolver should have caught this,
            // so just return error silently.
            TypeId::ERROR
        }

        ExprKind::Path(path) => {
            if let Some(res) = ctx.resolutions {
                if let Some(def_id) = res.get_path(path.span) {
                    return ctx.table.get_def_type(def_id).unwrap_or(TypeId::ERROR);
                }
            }
            TypeId::ERROR
        }

        // === Binary operators ===
        ExprKind::Binary { op, lhs, rhs } => {
            let lhs_ty = check_expr(lhs, ctx, current_fn_ret);
            let rhs_ty = check_expr(rhs, ctx, current_fn_ret);

            check_binary_op(*op, lhs_ty, rhs_ty, expr.span, lhs.span, rhs.span, ctx)
        }

        // === Unary operators ===
        ExprKind::Unary { op, operand } => {
            let operand_ty = check_expr(operand, ctx, current_fn_ret);
            check_unary_op(
                *op,
                operand_ty,
                expr.span,
                operand.span,
                ctx.table,
                ctx.builtins,
                ctx.diagnostics,
            )
        }

        // === Function call ===
        ExprKind::Call { callee, args } => {
            let callee_ty = check_expr(callee, ctx, current_fn_ret);
            check_call(callee_ty, args, expr.span, callee.span, ctx, current_fn_ret)
        }

        // === Field access ===
        ExprKind::Field { base, field } => {
            let base_ty = check_expr(base, ctx, current_fn_ret);
            check_field_access(
                base_ty,
                field.name,
                field.span,
                ctx.table,
                ctx.definitions,
                ctx.diagnostics,
            )
        }

        // === Optional field access ===
        ExprKind::OptionalField { base, field } => {
            let base_ty = check_expr(base, ctx, current_fn_ret);
            let inner_ty = unwrap_optional(base_ty, ctx.table);
            let field_ty = check_field_access(
                inner_ty,
                field.name,
                field.span,
                ctx.table,
                ctx.definitions,
                ctx.diagnostics,
            );
            if field_ty.is_error() {
                return TypeId::ERROR;
            }
            ctx.table.intern(Ty::Optional(field_ty))
        }

        // === Index access ===
        ExprKind::Index { base, index } => {
            let base_ty = check_expr(base, ctx, current_fn_ret);
            let _index_ty = check_expr(index, ctx, current_fn_ret);

            match ctx.table.resolve(strip_mutable(base_ty, ctx.table)).clone() {
                Ty::List(elem) => ctx.table.intern(Ty::Optional(elem)),
                Ty::Map(_, value) => ctx.table.intern(Ty::Optional(value)),
                Ty::Tuple(elems) => {
                    if let ExprKind::Literal(spark_ast::Literal {
                        kind: LiteralKind::Int(val),
                        ..
                    }) = &index.kind
                    {
                        let idx = *val as usize;
                        if idx >= elems.len() {
                            ctx.diagnostics.push(diagnostics::tuple_oob(
                                idx,
                                elems.len(),
                                index.span,
                            ));
                            TypeId::ERROR
                        } else {
                            elems[idx]
                        }
                    } else {
                        ctx.diagnostics.push(
                            Diagnostic::error("tuple index must be an integer literal").with_label(
                                Label::primary(index.span, "expected an integer literal"),
                            ),
                        );
                        TypeId::ERROR
                    }
                }
                Ty::Error => TypeId::ERROR,
                _ => {
                    ctx.diagnostics.push(
                        Diagnostic::error("type is not indexable")
                            .with_label(Label::primary(base.span, "cannot index this type")),
                    );
                    TypeId::ERROR
                }
            }
        }

        // === Struct initialization ===
        ExprKind::StructInit {
            path,
            type_args,
            fields,
            spread,
        } => {
            // Intercept Channel[T]() constructor
            if path.is_simple() && path.segments[0].name == kw::CHANNEL {
                return check_channel_constructor(type_args, fields, expr.span, ctx);
            }
            check_struct_init(
                path,
                type_args,
                fields,
                spread.as_deref(),
                expr.span,
                ctx,
                current_fn_ret,
            )
        }

        // === Enum variant ===
        ExprKind::EnumVariant {
            path,
            variant,
            fields,
        } => check_enum_variant(path, variant, fields, expr.span, ctx, current_fn_ret),

        // === Block ===
        ExprKind::Block(block) => check_block(block, ctx, current_fn_ret),

        // === If expression ===
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let cond_ty = check_expr(condition, ctx, current_fn_ret);

            if !is_compatible(cond_ty, ctx.builtins.bool_ty, ctx.table) {
                ctx.diagnostics.push(
                    Diagnostic::error("if condition must be Bool")
                        .with_label(Label::primary(condition.span, "expected Bool")),
                );
            }

            let then_ty = check_expr(then_branch, ctx, current_fn_ret);

            else_branch
                .as_ref()
                .map_or(ctx.builtins.void, |else_branch| {
                    let else_ty = check_expr(else_branch, ctx, current_fn_ret);
                    unify_branches(then_ty, else_ty, ctx.table, ctx.builtins)
                })
        }

        // === Match expression ===
        ExprKind::Match { scrutinee, arms } => {
            let scrutinee_ty = check_expr(scrutinee, ctx, current_fn_ret);

            let mut arm_types = Vec::with_capacity(arms.len());
            for arm in arms {
                // Pattern checking is simplified — we just check the body
                let body_ty = check_expr(&arm.body, ctx, current_fn_ret);
                arm_types.push(body_ty);
            }

            // Check exhaustiveness
            crate::exhaustive::check_exhaustiveness(scrutinee_ty, arms, expr.span, ctx);

            if arm_types.is_empty() {
                ctx.builtins.never
            } else {
                let mut result = arm_types[0];
                for &ty in &arm_types[1..] {
                    result = unify_branches(result, ty, ctx.table, ctx.builtins);
                }
                result
            }
        }

        // === Loop ===
        ExprKind::Loop { body } => {
            let _ = check_expr(body, ctx, current_fn_ret);
            // Loop's type comes from break expressions — for now, Void
            ctx.builtins.void
        }

        // === Break ===
        ExprKind::Break(value) => {
            if let Some(val) = value {
                let _ = check_expr(val, ctx, current_fn_ret);
            }
            ctx.builtins.never
        }

        // === Continue ===
        ExprKind::Continue => ctx.builtins.never,

        // === Return ===
        ExprKind::Return(value) => {
            let val_ty = value.as_ref().map_or(ctx.builtins.void, |val| {
                check_expr(val, ctx, current_fn_ret)
            });

            if let Some(expected) = current_fn_ret {
                if !is_compatible(val_ty, expected, ctx.table) {
                    let expected_name = type_name(expected, ctx.table, ctx.definitions);
                    let found_name = type_name(val_ty, ctx.table, ctx.definitions);
                    ctx.diagnostics.push(
                        Diagnostic::error(format!("return type mismatch: expected `{expected_name}`, found `{found_name}`"))
                            .with_label(Label::primary(expr.span, format!("expected `{expected_name}`"))),
                    );
                }
            }

            ctx.builtins.never
        }

        // === Lambda ===
        ExprKind::Lambda {
            params,
            ret_type,
            body,
        } => {
            let param_types: Vec<TypeId> =
                params.iter().map(|p| lower_type_expr(&p.ty, ctx)).collect();

            // Register param types
            for (i, param) in params.iter().enumerate() {
                if let Some(res) = ctx.resolutions {
                    if let Some(def_id) = res.get_binding(param.name.span) {
                        ctx.table.set_def_type(def_id, param_types[i]);
                    }
                }
            }

            let declared_ret = ret_type.as_ref().map(|rt| lower_type_expr(rt, ctx));

            let body_ty = check_expr(body, ctx, declared_ret);
            let ret = declared_ret.unwrap_or(body_ty);

            ctx.table.intern(Ty::Function {
                params: param_types,
                ret,
                is_async: false,
            })
        }

        // === Coalesce (??) ===
        ExprKind::Coalesce { lhs, rhs } => {
            let lhs_ty = check_expr(lhs, ctx, current_fn_ret);
            let rhs_ty = check_expr(rhs, ctx, current_fn_ret);

            let inner = unwrap_optional(lhs_ty, ctx.table);
            if inner == lhs_ty && !lhs_ty.is_error() {
                ctx.diagnostics.push(
                    Diagnostic::error("left side of `??` must be an optional type")
                        .with_label(Label::primary(lhs.span, "not optional")),
                );
            }

            // The result is the non-optional inner type unified with rhs
            if inner.is_error() {
                rhs_ty
            } else {
                unify_branches(inner, rhs_ty, ctx.table, ctx.builtins)
            }
        }

        // === Propagate (!) ===
        ExprKind::Propagate(inner_expr) => {
            let inner_ty = check_expr(inner_expr, ctx, current_fn_ret);

            // For Optional(T) → success type is T
            if let Ty::Optional(success) = ctx.table.resolve(inner_ty) {
                let success = *success;
                // Validate that Null is compatible with the enclosing function's return type
                if let Some(ret) = current_fn_ret {
                    if !is_compatible(ctx.builtins.null, ret, ctx.table) {
                        let ret_name = type_name(ret, ctx.table, ctx.definitions);
                        ctx.diagnostics.push(
                            Diagnostic::error(format!(
                                "propagation operator `!` may return `Null`, but function return type `{ret_name}` does not include `Null`"
                            ))
                            .with_label(Label::primary(expr.span, "propagates `Null` here")),
                        );
                    }
                } else {
                    ctx.diagnostics
                        .push(diagnostics::propagation_outside_fn(expr.span));
                }
                return success;
            }

            // For Union types → use split_union to determine success type
            if matches!(ctx.table.resolve(inner_ty), Ty::Union(_)) {
                // Try to split using the expected type (from context) or fn return type
                if let Some((success_ty, errors)) =
                    crate::union::split_union(inner_ty, expected_ty, current_fn_ret, ctx.table)
                {
                    // Validate that each error type is compatible with the enclosing function's return type
                    if let Some(ret) = current_fn_ret {
                        for &error_ty in &errors {
                            if !is_compatible(error_ty, ret, ctx.table) {
                                let error_name = type_name(error_ty, ctx.table, ctx.definitions);
                                let ret_name = type_name(ret, ctx.table, ctx.definitions);
                                ctx.diagnostics.push(
                                    Diagnostic::error(format!(
                                        "propagation operator `!` may return `{error_name}`, but function return type `{ret_name}` does not include it"
                                    ))
                                    .with_label(Label::primary(expr.span, format!("propagates `{error_name}` here"))),
                                );
                            }
                        }
                    } else {
                        ctx.diagnostics
                            .push(diagnostics::propagation_outside_fn(expr.span));
                    }
                    return success_ty;
                }
            }

            // Fallback: unwrap nullable
            let result = unwrap_optional(inner_ty, ctx.table);
            // If unwrap_optional actually unwrapped something, Null was removed — validate it
            if result != inner_ty {
                if let Some(ret) = current_fn_ret {
                    if !is_compatible(ctx.builtins.null, ret, ctx.table) {
                        let ret_name = type_name(ret, ctx.table, ctx.definitions);
                        ctx.diagnostics.push(
                            Diagnostic::error(format!(
                                "propagation operator `!` may return `Null`, but function return type `{ret_name}` does not include `Null`"
                            ))
                            .with_label(Label::primary(expr.span, "propagates `Null` here")),
                        );
                    }
                } else {
                    ctx.diagnostics
                        .push(diagnostics::propagation_outside_fn(expr.span));
                }
            }
            result
        }

        // === Is ===
        ExprKind::Is {
            expr: inner_expr,
            ty,
        } => {
            let _ = check_expr(inner_expr, ctx, current_fn_ret);
            let _ = lower_type_expr(ty, ctx);
            ctx.builtins.bool_ty
        }

        // === As ===
        ExprKind::As {
            expr: inner_expr,
            ty,
        } => {
            let _ = check_expr(inner_expr, ctx, current_fn_ret);
            lower_type_expr(ty, ctx)
        }

        // === Collections ===
        ExprKind::List(elems) => {
            if elems.is_empty() {
                // Empty list — element type unknown, use Error for now
                ctx.table.intern(Ty::List(TypeId::ERROR))
            } else {
                let first_ty = check_expr(&elems[0], ctx, current_fn_ret);
                for elem in &elems[1..] {
                    let _ = check_expr(elem, ctx, current_fn_ret);
                }
                ctx.table.intern(Ty::List(first_ty))
            }
        }

        ExprKind::Map(entries) => {
            if entries.is_empty() {
                ctx.table.intern(Ty::Map(TypeId::ERROR, TypeId::ERROR))
            } else {
                let key_ty = check_expr(&entries[0].key, ctx, current_fn_ret);
                let val_ty = check_expr(&entries[0].value, ctx, current_fn_ret);
                for entry in &entries[1..] {
                    let _ = check_expr(&entry.key, ctx, current_fn_ret);
                    let _ = check_expr(&entry.value, ctx, current_fn_ret);
                }
                ctx.table.intern(Ty::Map(key_ty, val_ty))
            }
        }

        ExprKind::Set(elems) => {
            if elems.is_empty() {
                ctx.table.intern(Ty::Set(TypeId::ERROR))
            } else {
                let first_ty = check_expr(&elems[0], ctx, current_fn_ret);
                for elem in &elems[1..] {
                    let _ = check_expr(elem, ctx, current_fn_ret);
                }
                ctx.table.intern(Ty::Set(first_ty))
            }
        }

        ExprKind::Tuple(elems) => {
            let elem_types: Vec<TypeId> = elems
                .iter()
                .map(|e| check_expr(e, ctx, current_fn_ret))
                .collect();
            ctx.table.intern(Ty::Tuple(elem_types))
        }

        // === Paren ===
        ExprKind::Paren(inner) => check_expr(inner, ctx, current_fn_ret),

        // === Await ===
        ExprKind::Await(inner) | ExprKind::BlockAwait(inner) => {
            let inner_ty = check_expr(inner, ctx, current_fn_ret);
            match ctx.table.resolve(inner_ty) {
                Ty::Future { inner } => *inner,
                _ if inner_ty.is_error() => TypeId::ERROR,
                _ => {
                    ctx.diagnostics.push(
                        Diagnostic::error("cannot `await` a non-future type")
                            .with_label(Label::primary(expr.span, "not a future")),
                    );
                    TypeId::ERROR
                }
            }
        }

        // === Spawn ===
        ExprKind::Spawn(inner) => {
            let inner_ty = check_expr(inner, ctx, current_fn_ret);
            match ctx.table.resolve(inner_ty) {
                Ty::Future { .. } => inner_ty,
                _ if inner_ty.is_error() => TypeId::ERROR,
                _ => {
                    ctx.diagnostics.push(
                        Diagnostic::error("cannot `spawn` a non-async expression")
                            .with_label(Label::primary(inner.span, "not a future")),
                    );
                    TypeId::ERROR
                }
            }
        }

        // === Method calls on collections and strings ===
        ExprKind::MethodCall {
            receiver,
            method,
            args,
        } => {
            let recv_ty = check_expr(receiver, ctx, current_fn_ret);
            let arg_tys: Vec<TypeId> = args
                .iter()
                .map(|a| check_expr(&a.value, ctx, current_fn_ret))
                .collect();

            check_method_call(
                recv_ty,
                method.name,
                &arg_tys,
                receiver.span,
                expr.span,
                ctx,
            )
        }

        ExprKind::Pipeline { source, stages } => {
            let mut current_ty = check_expr(source, ctx, current_fn_ret);
            for stage in stages {
                current_ty =
                    check_pipeline_stage(stage, current_ty, source.span, ctx, current_fn_ret);
            }
            current_ty
        }

        ExprKind::To { expr: inner, arms } => {
            let inner_ty = check_expr(inner, ctx, current_fn_ret);
            check_to_narrowing(inner_ty, arms, expr.span, ctx, current_fn_ret)
        }

        ExprKind::PipelineApply {
            expr: inner,
            operation,
            args,
        } => {
            let inner_ty = check_expr(inner, ctx, current_fn_ret);
            check_pipeline_apply(inner_ty, operation, args, expr.span, ctx, current_fn_ret)
        }

        ExprKind::Range { start, end, .. } => {
            let _ = check_expr(start, ctx, current_fn_ret);
            let _ = check_expr(end, ctx, current_fn_ret);
            // Bare range type — not yet supported
            TypeId::ERROR
        }

        ExprKind::ListRange {
            start, end, step, ..
        } => {
            let start_ty = check_expr(start, ctx, current_fn_ret);
            let end_ty = check_expr(end, ctx, current_fn_ret);
            if let Some(s) = step {
                let step_ty = check_expr(s, ctx, current_fn_ret);
                if step_ty != ctx.builtins.int && !step_ty.is_error() {
                    ctx.diagnostics.push(
                        Diagnostic::error("list range step must be `Int`")
                            .with_label(Label::primary(s.span, "expected `Int`")),
                    );
                }
            }
            if start_ty != ctx.builtins.int && !start_ty.is_error() {
                ctx.diagnostics.push(
                    Diagnostic::error("list range start must be `Int`")
                        .with_label(Label::primary(start.span, "expected `Int`")),
                );
            }
            if end_ty != ctx.builtins.int && !end_ty.is_error() {
                ctx.diagnostics.push(
                    Diagnostic::error("list range end must be `Int`")
                        .with_label(Label::primary(end.span, "expected `Int`")),
                );
            }
            ctx.table.intern(Ty::List(ctx.builtins.int))
        }

        ExprKind::Partial { callee, args } => {
            let _ = check_expr(callee, ctx, current_fn_ret);
            for arg in args {
                let _ = check_expr(&arg.value, ctx, current_fn_ret);
            }
            TypeId::ERROR
        }

        ExprKind::Error => TypeId::ERROR,
    }
}

/// Type-checks a block expression.
pub fn check_block(
    block: &Block,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    for stmt in &block.stmts {
        check_stmt(stmt, ctx, current_fn_ret);
    }

    let ty = block.expr.as_ref().map_or(ctx.builtins.void, |expr| {
        check_expr(expr, ctx, current_fn_ret)
    });
    ctx.table.set_expr_type(block.id, ty);
    ty
}

// === Binary operator checking ===

fn check_binary_op(
    op: BinaryOp,
    lhs_ty: TypeId,
    rhs_ty: TypeId,
    op_span: Span,
    lhs_span: Span,
    rhs_span: Span,
    ctx: &mut TypeCheckCtx<'_>,
) -> TypeId {
    // Error types propagate silently
    if lhs_ty.is_error() || rhs_ty.is_error() {
        return TypeId::ERROR;
    }

    if op.is_arithmetic() {
        // Strip Mutable/Ref wrappers — `count + 1` where count is Int! or Ref[Int] should work
        let lhs_base = strip_ref(strip_mutable(lhs_ty, ctx.table), ctx.table);
        let rhs_base = strip_ref(strip_mutable(rhs_ty, ctx.table), ctx.table);
        let lhs_resolved = ctx.table.resolve(lhs_base);
        if !lhs_resolved.is_numeric() {
            ctx.diagnostics.push(
                Diagnostic::error(format!(
                    "operator `{}` requires numeric operands",
                    op.as_str()
                ))
                .with_label(Label::primary(lhs_span, "not numeric")),
            );
            return TypeId::ERROR;
        }
        if !is_compatible(lhs_base, rhs_base, ctx.table) {
            let l = type_name(lhs_base, ctx.table, &DefinitionTable::new());
            let r = type_name(rhs_base, ctx.table, &DefinitionTable::new());
            ctx.diagnostics.push(
                Diagnostic::error(format!(
                    "mismatched types for `{}`: `{l}` and `{r}`",
                    op.as_str()
                ))
                .with_label(Label::primary(lhs_span, format!("`{l}`")))
                .with_label(Label::secondary(rhs_span, format!("`{r}`"))),
            );
            return TypeId::ERROR;
        }
        // Arithmetic produces a new value, not a mutable binding
        return lhs_base;
    }

    if op.is_comparison() {
        let lhs_base = strip_ref(strip_mutable(lhs_ty, ctx.table), ctx.table);
        let rhs_base = strip_ref(strip_mutable(rhs_ty, ctx.table), ctx.table);
        if !is_compatible(lhs_base, rhs_base, ctx.table) {
            let l = type_name(lhs_base, ctx.table, &DefinitionTable::new());
            let r = type_name(rhs_base, ctx.table, &DefinitionTable::new());
            ctx.diagnostics.push(
                Diagnostic::error(format!("cannot compare `{l}` with `{r}`"))
                    .with_label(Label::primary(op_span, "type mismatch")),
            );
        }
        return ctx.builtins.bool_ty;
    }

    if op.is_logical() {
        if !is_compatible(lhs_ty, ctx.builtins.bool_ty, ctx.table) {
            ctx.diagnostics.push(
                Diagnostic::error(format!("operator `{}` requires Bool operands", op.as_str()))
                    .with_label(Label::primary(lhs_span, "expected Bool")),
            );
        }
        if !is_compatible(rhs_ty, ctx.builtins.bool_ty, ctx.table) {
            ctx.diagnostics.push(
                Diagnostic::error(format!("operator `{}` requires Bool operands", op.as_str()))
                    .with_label(Label::primary(rhs_span, "expected Bool")),
            );
        }
        return ctx.builtins.bool_ty;
    }

    if op.is_bitwise() {
        let lhs_base = strip_ref(strip_mutable(lhs_ty, ctx.table), ctx.table);
        let rhs_base = strip_ref(strip_mutable(rhs_ty, ctx.table), ctx.table);
        let lhs_resolved = ctx.table.resolve(lhs_base);
        if !lhs_resolved.is_integer() {
            ctx.diagnostics.push(
                Diagnostic::error(format!(
                    "operator `{}` requires integer operands",
                    op.as_str()
                ))
                .with_label(Label::primary(lhs_span, "not integer")),
            );
            return TypeId::ERROR;
        }
        if !is_compatible(lhs_base, rhs_base, ctx.table) {
            ctx.diagnostics.push(
                Diagnostic::error(format!("mismatched integer types for `{}`", op.as_str()))
                    .with_label(Label::primary(op_span, "type mismatch")),
            );
            return TypeId::ERROR;
        }
        return lhs_base;
    }

    TypeId::ERROR
}

/// Checks a unary operator application.
fn check_unary_op(
    op: UnaryOp,
    operand_ty: TypeId,
    _op_span: Span,
    operand_span: Span,
    table: &TypeTable,
    builtins: &BuiltinTypes,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    if operand_ty.is_error() {
        return TypeId::ERROR;
    }

    let operand_base = strip_ref(strip_mutable(operand_ty, table), table);

    match op {
        UnaryOp::Neg => {
            if !table.resolve(operand_base).is_numeric() {
                diagnostics.push(
                    Diagnostic::error("unary `-` requires a numeric operand")
                        .with_label(Label::primary(operand_span, "not numeric")),
                );
                return TypeId::ERROR;
            }
            operand_base
        }
        UnaryOp::Not => {
            if !is_compatible(operand_base, builtins.bool_ty, table) {
                diagnostics.push(
                    Diagnostic::error("`not` requires a Bool operand")
                        .with_label(Label::primary(operand_span, "expected Bool")),
                );
                return TypeId::ERROR;
            }
            builtins.bool_ty
        }
        UnaryOp::BitNot => {
            if !table.resolve(operand_base).is_integer() {
                diagnostics.push(
                    Diagnostic::error("`~` requires an integer operand")
                        .with_label(Label::primary(operand_span, "not integer")),
                );
                return TypeId::ERROR;
            }
            operand_base
        }
    }
}

/// Checks a function call, including generic function inference.
#[allow(clippy::too_many_lines)]
fn check_call(
    callee_ty: TypeId,
    args: &[spark_ast::CallArg],
    call_span: Span,
    callee_span: Span,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    if callee_ty.is_error() {
        // Still check args to report errors in them
        for arg in args {
            let _ = check_expr(&arg.value, ctx, current_fn_ret);
        }
        return TypeId::ERROR;
    }

    let callee_resolved = ctx.table.resolve(callee_ty).clone();
    match callee_resolved {
        Ty::Function {
            params,
            ret,
            is_async,
        } => {
            // Check if this is a generic function by finding its DefId.
            let callee_def_id = find_callee_def_id(callee_span, ctx.resolutions);
            let generic_params =
                callee_def_id.and_then(|did| ctx.table.get_generic_params(did).map(<[_]>::to_vec));

            if let Some(gparams) = generic_params.filter(|p| !p.is_empty()) {
                // Generic function — type-check args first, then infer type params.
                let arg_types: Vec<TypeId> = args
                    .iter()
                    .map(|a| check_expr(&a.value, ctx, current_fn_ret))
                    .collect();

                if args.len() != params.len() {
                    diagnostics::expect_args(params.len(), args.len(), call_span, ctx.diagnostics);
                    return TypeId::ERROR;
                }

                if let Some(subst) =
                    crate::substitute::infer_type_args(&gparams, &params, &arg_types, ctx.table)
                {
                    // Validate generic constraints before proceeding
                    crate::constraint::validate_constraints(
                        &gparams,
                        &subst,
                        ctx.table,
                        ctx.definitions,
                        call_span,
                        ctx.diagnostics,
                    );

                    // Re-check args against substituted param types
                    for (i, arg) in args.iter().enumerate() {
                        if let Some(&declared) = params.get(i) {
                            let expected =
                                crate::substitute::substitute_mut(declared, &subst, ctx.table);
                            if !is_compatible(arg_types[i], expected, ctx.table) {
                                let expected_name = type_name(expected, ctx.table, ctx.definitions);
                                let found_name =
                                    type_name(arg_types[i], ctx.table, ctx.definitions);
                                ctx.diagnostics.push(diagnostics::arg_type_mismatch(
                                    &expected_name,
                                    &found_name,
                                    arg.value.span,
                                ));
                            } else if is_mutable_ref_param(expected, ctx.table)
                                && !matches!(ctx.table.resolve(arg_types[i]), Ty::Mutable(_))
                            {
                                ctx.diagnostics
                                    .push(diagnostics::mutable_ref_required(arg.value.span));
                            }
                        }
                    }

                    // Record this mono instance for MIR monomorphization.
                    if let Some(def_id) = callee_def_id {
                        ctx.table
                            .set_mono_instance(call_span, def_id, subst.clone());
                    }

                    // Return the substituted return type
                    let substituted_ret = crate::substitute::substitute_mut(ret, &subst, ctx.table);
                    return if is_async {
                        ctx.table.intern(Ty::Future {
                            inner: substituted_ret,
                        })
                    } else {
                        substituted_ret
                    };
                }

                // Inference failed — report error
                ctx.diagnostics.push(
                    Diagnostic::error("cannot infer type arguments for generic function")
                        .with_label(Label::primary(call_span, "type inference failed")),
                );
                return TypeId::ERROR;
            }

            // Non-generic function call — check all arg types first,
            // then adapt builtin list function types if needed.
            let arg_tys: Vec<TypeId> = args
                .iter()
                .map(|a| check_expr(&a.value, ctx, current_fn_ret))
                .collect();

            // Adapt builtin list function types to match actual element type
            let (params, ret) = if let Some(did) = callee_def_id {
                adapt_builtin_list_fn(
                    did,
                    arg_tys.first().copied().unwrap_or(TypeId::ERROR),
                    params,
                    ret,
                    ctx.table,
                    ctx.builtins,
                    ctx.definitions,
                )
            } else {
                (params, ret)
            };

            diagnostics::expect_args(params.len(), arg_tys.len(), call_span, ctx.diagnostics);

            for (i, (arg, &arg_ty)) in args.iter().zip(arg_tys.iter()).enumerate() {
                if let Some(&expected) = params.get(i) {
                    if !is_compatible(arg_ty, expected, ctx.table) {
                        let expected_name = type_name(expected, ctx.table, ctx.definitions);
                        let found_name = type_name(arg_ty, ctx.table, ctx.definitions);
                        ctx.diagnostics.push(diagnostics::arg_type_mismatch(
                            &expected_name,
                            &found_name,
                            arg.value.span,
                        ));
                    } else if is_mutable_ref_param(expected, ctx.table)
                        && !matches!(ctx.table.resolve(arg_ty), Ty::Mutable(_))
                    {
                        ctx.diagnostics
                            .push(diagnostics::mutable_ref_required(arg.value.span));
                    }
                }
            }

            if is_async {
                ctx.table.intern(Ty::Future { inner: ret })
            } else {
                ret
            }
        }
        // Struct construction via call syntax is handled by StructInit
        Ty::Struct(_) | Ty::Enum(_) => {
            for arg in args {
                let _ = check_expr(&arg.value, ctx, current_fn_ret);
            }
            callee_ty
        }
        _ => {
            ctx.diagnostics.push(
                Diagnostic::error("expression is not callable")
                    .with_label(Label::primary(callee_span, "not a function")),
            );
            for arg in args {
                let _ = check_expr(&arg.value, ctx, current_fn_ret);
            }
            TypeId::ERROR
        }
    }
}

/// Tries to find the `DefId` for a callee expression (for generic function lookup).
fn find_callee_def_id(callee_span: Span, resolutions: Option<&ResolutionMap>) -> Option<DefId> {
    let res = resolutions?;
    // Try both path and ident resolution (callee could be either)
    res.get_path(callee_span)
        .or_else(|| res.get_ident(callee_span))
}

/// Checks struct field access.
fn check_field_access(
    base_ty: TypeId,
    field_name: spark_intern::Symbol,
    field_span: Span,
    table: &mut TypeTable,
    definitions: &DefinitionTable,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    if base_ty.is_error() {
        return TypeId::ERROR;
    }

    // If the base is immutable, field access must strip the Mutable wrapper
    // from the field type — an immutable struct's fields are not assignable.
    let base_is_mutable = matches!(table.resolve(base_ty), Ty::Mutable(_));

    let resolved = table
        .resolve(strip_ref(strip_mutable(base_ty, table), table))
        .clone();
    match resolved {
        Ty::Struct(def_id) => {
            if let Some(fields) = table.get_struct_fields(def_id) {
                for &(name, ty) in fields {
                    if name == field_name {
                        if let Some(field_def) =
                            crate::check_decl::find_field_def(field_name, def_id, definitions)
                        {
                            table.set_field_resolution(field_span, field_def);
                        }
                        if !base_is_mutable {
                            return strip_mutable(ty, table);
                        }
                        return ty;
                    }
                }
            }
            let tn = definitions
                .get(def_id)
                .map_or("<unknown>", |info| info.name.as_str());
            diagnostics.push(crate::diagnostics::field_not_found(
                field_name, tn, field_span,
            ));
            TypeId::ERROR
        }
        Ty::Applied { base, args } => {
            if let Some(fields) = table.get_struct_fields(base) {
                let fields = fields.to_vec();
                let params = table
                    .get_generic_params(base)
                    .map(<[_]>::to_vec)
                    .unwrap_or_default();
                let subst = crate::substitute::build_subst_map(&params, &args);
                for (name, ty) in fields {
                    if name == field_name {
                        if let Some(field_def) =
                            crate::check_decl::find_field_def(field_name, base, definitions)
                        {
                            table.set_field_resolution(field_span, field_def);
                        }
                        let field_ty = crate::substitute::substitute_mut(ty, &subst, table);
                        if !base_is_mutable {
                            return strip_mutable(field_ty, table);
                        }
                        return field_ty;
                    }
                }
            }
            let tn = definitions
                .get(base)
                .map_or("<unknown>", |info| info.name.as_str());
            diagnostics.push(crate::diagnostics::field_not_found(
                field_name, tn, field_span,
            ));
            TypeId::ERROR
        }
        Ty::Tuple(elems) => {
            let field_str = field_name.as_str();
            match field_str.parse::<usize>() {
                Ok(index) => {
                    if index >= elems.len() {
                        diagnostics.push(crate::diagnostics::tuple_oob(
                            index,
                            elems.len(),
                            field_span,
                        ));
                        TypeId::ERROR
                    } else {
                        let field_ty = elems[index];
                        if !base_is_mutable {
                            strip_mutable(field_ty, table)
                        } else {
                            field_ty
                        }
                    }
                }
                Err(_) => {
                    diagnostics.push(
                        Diagnostic::error("tuple fields must be accessed with numeric indices")
                            .with_label(Label::primary(field_span, "expected a numeric index")),
                    );
                    TypeId::ERROR
                }
            }
        }
        Ty::Error => TypeId::ERROR,
        _ => {
            diagnostics.push(
                Diagnostic::error("field access on non-struct type")
                    .with_label(Label::primary(field_span, "not a struct")),
            );
            TypeId::ERROR
        }
    }
}

/// Checks struct initialization.
fn check_struct_init(
    path: &spark_ast::Path,
    type_args: &[spark_ast::TypeExpr],
    fields: &[spark_ast::FieldInit],
    spread: Option<&Expr>,
    init_span: Span,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    // Resolve the struct path
    let def_id = ctx
        .resolutions
        .and_then(|res| res.get_path(path.span))
        .unwrap_or(DefId::DUMMY);

    if def_id.is_dummy() {
        // Struct not resolved — check field values anyway
        for field in fields {
            let _ = check_expr(&field.value, ctx, current_fn_ret);
        }
        if let Some(spread) = spread {
            let _ = check_expr(spread, ctx, current_fn_ret);
        }
        return TypeId::ERROR;
    }

    let struct_type = ctx.table.get_def_type(def_id).unwrap_or(TypeId::ERROR);

    // Check if this is a generic struct — if so, infer type args from field values.
    let generic_params = ctx.table.get_generic_params(def_id).map(<[_]>::to_vec);
    let is_generic = generic_params.as_ref().is_some_and(|p| !p.is_empty());

    // Check each field and collect value types for inference.
    if let Some(struct_fields) = ctx.table.get_struct_fields(def_id) {
        let struct_fields = struct_fields.to_vec();
        let mut field_val_types: Vec<(Symbol, TypeId)> = Vec::new();

        for field in fields {
            let val_ty = check_expr(&field.value, ctx, current_fn_ret);
            if let Some((_, expected_ty)) = struct_fields
                .iter()
                .find(|(name, _)| *name == field.name.name)
            {
                field_val_types.push((field.name.name, val_ty));

                // For generic structs, defer compatibility check until after inference.
                if !is_generic && !is_compatible(val_ty, *expected_ty, ctx.table) {
                    let expected_name = type_name(*expected_ty, ctx.table, ctx.definitions);
                    let found_name = type_name(val_ty, ctx.table, ctx.definitions);
                    ctx.diagnostics.push(
                        Diagnostic::error(format!(
                            "field `{}` type mismatch: expected `{expected_name}`, found `{found_name}`",
                            field.name.name
                        ))
                        .with_label(Label::primary(field.value.span, format!("expected `{expected_name}`"))),
                    );
                }
            } else {
                ctx.diagnostics.push(
                    Diagnostic::error(format!("unknown field `{}`", field.name.name)).with_label(
                        Label::primary(field.name.span, "not a field of this struct"),
                    ),
                );
            }
        }

        // For generic structs: use explicit type args or infer from field values.
        if is_generic {
            let params = generic_params.as_deref().unwrap_or_default();

            // Try explicit type args first, then fall back to inference.
            let subst = if !type_args.is_empty() {
                // Explicit type args provided: Pair[Int, String] { ... }
                if type_args.len() != params.len() {
                    ctx.diagnostics.push(
                        Diagnostic::error(format!(
                            "expected {} type argument(s), found {}",
                            params.len(),
                            type_args.len()
                        ))
                        .with_label(Label::primary(init_span, "wrong number of type arguments")),
                    );
                    None
                } else {
                    let lowered: Vec<TypeId> = type_args
                        .iter()
                        .map(|ta| lower_type_expr(ta, ctx))
                        .collect();
                    Some(crate::substitute::build_subst_map(params, &lowered))
                }
            } else {
                // Infer type args from field values.
                let mut declared_types = Vec::new();
                let mut actual_types = Vec::new();
                for (field_name, val_ty) in &field_val_types {
                    if let Some((_, field_ty)) = struct_fields.iter().find(|(n, _)| n == field_name)
                    {
                        declared_types.push(*field_ty);
                        actual_types.push(*val_ty);
                    }
                }
                crate::substitute::infer_type_args(
                    params,
                    &declared_types,
                    &actual_types,
                    ctx.table,
                )
            };

            if let Some(subst) = subst {
                // Re-check fields with substituted types
                for (field_name, val_ty) in &field_val_types {
                    if let Some((_, field_ty)) = struct_fields.iter().find(|(n, _)| n == field_name)
                    {
                        let substituted =
                            crate::substitute::substitute_mut(*field_ty, &subst, ctx.table);
                        if !is_compatible(*val_ty, substituted, ctx.table) {
                            let expected_name = type_name(substituted, ctx.table, ctx.definitions);
                            let found_name = type_name(*val_ty, ctx.table, ctx.definitions);
                            ctx.diagnostics.push(
                                Diagnostic::error(format!(
                                    "field `{field_name}` type mismatch: expected `{expected_name}`, found `{found_name}`"
                                ))
                                .with_label(Label::primary(init_span, format!("expected `{expected_name}`"))),
                            );
                        }
                    }
                }

                // Build the Applied type
                let args: Vec<TypeId> = params.iter().map(|p| subst[p]).collect();
                if let Some(spread) = spread {
                    let _ = check_expr(spread, ctx, current_fn_ret);
                }
                return ctx.table.intern(Ty::Applied { base: def_id, args });
            }
            // Inference/explicit args failed — fall through and return the plain struct type
        }
    }

    if let Some(spread) = spread {
        let _ = check_expr(spread, ctx, current_fn_ret);
    }

    struct_type
}

/// Checks enum variant construction.
fn check_enum_variant(
    path: &spark_ast::Path,
    _variant: &spark_ast::Ident,
    fields: &[Expr],
    _init_span: Span,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    let def_id = ctx
        .resolutions
        .and_then(|res| res.get_path(path.span))
        .unwrap_or(DefId::DUMMY);

    // Check field values regardless
    for field in fields {
        let _ = check_expr(field, ctx, current_fn_ret);
    }

    if def_id.is_dummy() {
        return TypeId::ERROR;
    }

    ctx.table.get_def_type(def_id).unwrap_or(TypeId::ERROR)
}

// === Type compatibility ===

/// Checks if `actual` is compatible with `expected`.
///
/// Compatibility rules:
/// - `Ty::Error` is compatible with everything
/// - `Ty::Never` is compatible with everything
/// - Equal types are compatible
/// - `Null` is compatible with `Optional(T)`
/// - `T` is compatible with `Optional(T)`
/// - `T` is a member of `Union([..., T, ...])`
#[must_use]
pub fn is_compatible(actual: TypeId, expected: TypeId, table: &TypeTable) -> bool {
    if actual == expected {
        return true;
    }
    if actual.is_error() || expected.is_error() {
        return true;
    }

    let actual_ty = table.resolve(actual);
    let expected_ty = table.resolve(expected);

    // Never is the bottom type
    if matches!(actual_ty, Ty::Never) || matches!(expected_ty, Ty::Never) {
        return true;
    }

    // Null is compatible with Optional
    if matches!(actual_ty, Ty::Null) && matches!(expected_ty, Ty::Optional(_)) {
        return true;
    }

    // T is compatible with Optional(T)
    if let Ty::Optional(inner) = expected_ty {
        if actual == *inner || is_compatible(actual, *inner, table) {
            return true;
        }
    }

    // T is a member of Union
    if let Ty::Union(members) = expected_ty {
        for &member in members {
            if is_compatible(actual, member, table) {
                return true;
            }
        }
    }

    // Applied types are compatible if same base + pairwise compatible args
    if let (Ty::Applied { base: b1, args: a1 }, Ty::Applied { base: b2, args: a2 }) =
        (actual_ty, expected_ty)
    {
        if b1 == b2 && a1.len() == a2.len() {
            return a1
                .iter()
                .zip(a2.iter())
                .all(|(a, e)| is_compatible(*a, *e, table));
        }
    }

    // Mutable T is compatible with T
    if let Ty::Mutable(inner) = actual_ty {
        if is_compatible(*inner, expected, table) {
            return true;
        }
    }

    // T is compatible with Mutable(T) — a plain value can initialize a mutable binding
    if let Ty::Mutable(inner) = expected_ty {
        if is_compatible(actual, *inner, table) {
            return true;
        }
    }

    // T is compatible with Ref[T] (auto-borrow coercion)
    if let Ty::Ref { inner, .. } = expected_ty {
        if is_compatible(actual, *inner, table) {
            return true;
        }
    }

    // Ref[T] is compatible with T (auto-deref coercion)
    if let Ty::Ref { inner, .. } = actual_ty {
        if is_compatible(*inner, expected, table) {
            return true;
        }
    }

    false
}

/// Unifies two branch types (e.g., if/else arms).
///
/// If both are the same type, returns it. If one is Never, returns the other.
/// Otherwise, returns a union of both.
fn unify_branches(a: TypeId, b: TypeId, table: &mut TypeTable, _builtins: &BuiltinTypes) -> TypeId {
    if a == b {
        return a;
    }
    if a.is_error() || b.is_error() {
        return TypeId::ERROR;
    }

    let a_ty = table.resolve(a);
    let b_ty = table.resolve(b);

    if matches!(a_ty, Ty::Never) {
        return b;
    }
    if matches!(b_ty, Ty::Never) {
        return a;
    }

    // Create a union of the two types
    let mut members = vec![a, b];
    members.sort();
    members.dedup();
    table.intern(Ty::Union(members))
}

/// Strips the `Mutable` wrapper if present.
pub(crate) fn strip_mutable(ty: TypeId, table: &TypeTable) -> TypeId {
    if let Ty::Mutable(inner) = table.resolve(ty) {
        *inner
    } else {
        ty
    }
}

/// Returns `true` if the type is `Mutable(Ref { .. })`, i.e. a mutable reference parameter.
fn is_mutable_ref_param(ty: TypeId, table: &TypeTable) -> bool {
    if let Ty::Mutable(inner) = table.resolve(ty) {
        matches!(table.resolve(*inner), Ty::Ref { .. })
    } else {
        false
    }
}

/// Validates the callback type for `each` / `each via`.
///
/// For `each`: callback must be `(T) -> Void` (though we don't enforce Void return).
/// For `each via`: callback must be `(T) -> U`; returns `U`.
///
/// Returns the callback's return type (used by `each via` to determine `List[U]`).
fn validate_each_callback(
    cb_ty: TypeId,
    elem_ty: TypeId,
    is_via: bool,
    span: Span,
    table: &TypeTable,
    definitions: &DefinitionTable,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    if cb_ty == TypeId::ERROR || elem_ty == TypeId::ERROR {
        return TypeId::ERROR;
    }
    match table.resolve(cb_ty).clone() {
        Ty::Function { params, ret, .. } => {
            if params.len() != 1 {
                diagnostics.push(
                    Diagnostic::error(format!(
                        "`each{}` callback must take exactly 1 parameter, found {}",
                        if is_via { " via" } else { "" },
                        params.len()
                    ))
                    .with_label(Label::primary(span, "wrong number of parameters")),
                );
                return TypeId::ERROR;
            }
            let param_ty = params[0];
            if param_ty != elem_ty && param_ty != TypeId::ERROR && elem_ty != TypeId::ERROR {
                let expected = type_name(elem_ty, table, definitions);
                let found = type_name(param_ty, table, definitions);
                diagnostics.push(
                    Diagnostic::error(format!(
                        "`each{}` callback parameter type mismatch: expected `{expected}`, found `{found}`",
                        if is_via { " via" } else { "" },
                    ))
                    .with_label(Label::primary(span, "type mismatch")),
                );
            }
            ret
        }
        _ => {
            let name = type_name(cb_ty, table, definitions);
            diagnostics.push(
                Diagnostic::error(format!(
                    "`each{}` callback must be a function, found `{name}`",
                    if is_via { " via" } else { "" },
                ))
                .with_label(Label::primary(span, "not a function")),
            );
            TypeId::ERROR
        }
    }
}

/// Validates a partial-application callback in an `each`/`each via` pipeline stage.
///
/// For `list each via func{b: val}`, the element fills the first parameter and the
/// bound arguments fill the rest. Returns the function's return type and stores a
/// reduced 1-parameter function type on `callback_expr` so downstream passes can
/// extract the return type via `extract_fn_ret_ty`.
fn validate_partial_each_callback(
    callee: &Expr,
    args: &[spark_ast::PartialArg],
    elem_ty: TypeId,
    is_via: bool,
    callback_expr: &Expr,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    let callee_ty = check_expr(callee, ctx, current_fn_ret);
    for arg in args {
        check_expr(&arg.value, ctx, current_fn_ret);
    }

    if callee_ty.is_error() || elem_ty.is_error() {
        ctx.table.set_expr_type(callback_expr.id, TypeId::ERROR);
        return TypeId::ERROR;
    }

    let func_ty = ctx.table.resolve(callee_ty).clone();
    let Ty::Function { params, ret, .. } = func_ty else {
        ctx.diagnostics.push(
            Diagnostic::error(format!(
                "`each{}` partial callback must be a function",
                if is_via { " via" } else { "" },
            ))
            .with_label(Label::primary(callee.span, "not a function")),
        );
        ctx.table.set_expr_type(callback_expr.id, TypeId::ERROR);
        return TypeId::ERROR;
    };

    // Arity: element fills first param, bound args fill the rest
    let total_args = 1 + args.len();
    if total_args != params.len() {
        ctx.diagnostics.push(
            Diagnostic::error(format!(
                "`each{}` partial callback expected {} parameter{}, found {} (1 element + {} bound)",
                if is_via { " via" } else { "" },
                params.len(),
                if params.len() == 1 { "" } else { "s" },
                total_args,
                args.len(),
            ))
            .with_label(Label::primary(callback_expr.span, "wrong number of arguments")),
        );
        ctx.table.set_expr_type(callback_expr.id, TypeId::ERROR);
        return TypeId::ERROR;
    }

    // Validate element type against first param
    if !is_compatible(elem_ty, params[0], ctx.table) {
        let expected = type_name(params[0], ctx.table, ctx.definitions);
        let found = type_name(elem_ty, ctx.table, ctx.definitions);
        ctx.diagnostics.push(
            Diagnostic::error(format!(
                "`each{}` element type mismatch: expected `{expected}`, found `{found}`",
                if is_via { " via" } else { "" },
            ))
            .with_label(Label::primary(callback_expr.span, "type mismatch")),
        );
    }

    // Validate bound arg types against remaining params
    for (i, arg) in args.iter().enumerate() {
        let arg_ty = ctx
            .table
            .get_expr_type(arg.value.id)
            .unwrap_or(TypeId::ERROR);
        let param_ty = params[i + 1];
        if !is_compatible(arg_ty, param_ty, ctx.table) {
            let expected = type_name(param_ty, ctx.table, ctx.definitions);
            let found = type_name(arg_ty, ctx.table, ctx.definitions);
            ctx.diagnostics.push(diagnostics::arg_type_mismatch(
                &expected,
                &found,
                arg.value.span,
            ));
        }
    }

    // Store a reduced 1-param function type so infer_stage_output_ty / extract_fn_ret_ty works
    let reduced_ty = ctx.table.intern(Ty::Function {
        params: vec![elem_ty],
        ret,
        is_async: false,
    });
    ctx.table.set_expr_type(callback_expr.id, reduced_ty);

    ret
}

/// Strips the `Ref` wrapper if present.
pub(crate) fn strip_ref(ty: TypeId, table: &TypeTable) -> TypeId {
    if let Ty::Ref { inner, .. } = table.resolve(ty) {
        *inner
    } else {
        ty
    }
}

/// Unwraps `Optional(T)` → `T`, or `Union([..., Null, ...])` → the non-null type.
/// Returns the input unchanged if not optional/nullable.
fn unwrap_optional(ty: TypeId, table: &TypeTable) -> TypeId {
    match table.resolve(ty) {
        Ty::Optional(inner) => *inner,
        Ty::Union(members) => {
            let null_id = table.lookup(&Ty::Null);
            let non_null: Vec<TypeId> = members
                .iter()
                .copied()
                .filter(|&m| Some(m) != null_id)
                .collect();
            match non_null.len() {
                0 => ty,
                1 => non_null[0],
                _ => {
                    // Check if the non-null union is already interned
                    let candidate = Ty::Union(non_null);
                    table.lookup(&candidate).unwrap_or(ty)
                }
            }
        }
        _ => ty,
    }
}

/// Type-checks a method call on a collection, string, or user-defined type receiver.
fn check_method_call(
    recv_ty: TypeId,
    method: Symbol,
    arg_tys: &[TypeId],
    recv_span: Span,
    call_span: Span,
    ctx: &mut TypeCheckCtx<'_>,
) -> TypeId {
    if recv_ty.is_error() {
        return TypeId::ERROR;
    }

    let (is_mutable, stripped) = match ctx.table.resolve(recv_ty).clone() {
        Ty::Mutable(inner) => (true, ctx.table.resolve(inner).clone()),
        other => (false, other),
    };

    // Auto-deref through Ref
    let stripped = match &stripped {
        Ty::Ref { inner, .. } => ctx.table.resolve(*inner).clone(),
        _ => stripped,
    };

    match stripped {
        Ty::List(elem) => {
            check_list_method(method, elem, is_mutable, arg_tys, call_span, recv_span, ctx)
        }
        Ty::Map(_k, _v) => check_map_method(method, is_mutable, arg_tys, call_span, recv_span, ctx),
        Ty::Set(_elem) => check_set_method(method, is_mutable, arg_tys, call_span, recv_span, ctx),
        Ty::String => {
            check_string_method(method, arg_tys, call_span, ctx.builtins, ctx.diagnostics)
        }
        Ty::Channel { inner } => check_channel_method(
            method,
            inner,
            arg_tys,
            call_span,
            ctx.table,
            ctx.builtins,
            ctx.diagnostics,
        ),
        Ty::Struct(def_id) | Ty::Enum(def_id) => {
            check_user_method_call(def_id, method, arg_tys, recv_span, call_span, ctx)
        }
        Ty::Applied { base, .. } => {
            check_user_method_call(base, method, arg_tys, recv_span, call_span, ctx)
        }
        _ => {
            ctx.diagnostics.push(
                Diagnostic::error(format!("type has no method `{method}`")).with_label(
                    Label::primary(recv_span, "no methods available on this type"),
                ),
            );
            TypeId::ERROR
        }
    }
}

/// Checks a method call on `List[T]`.
fn check_list_method(
    method: Symbol,
    elem: TypeId,
    is_mutable: bool,
    arg_tys: &[TypeId],
    call_span: Span,
    recv_span: Span,
    ctx: &mut TypeCheckCtx<'_>,
) -> TypeId {
    if method == kw::LEN {
        diagnostics::expect_args(0, arg_tys.len(), call_span, ctx.diagnostics);
        ctx.builtins.int
    } else if method == kw::APPEND {
        diagnostics::expect_args(1, arg_tys.len(), call_span, ctx.diagnostics);
        diagnostics::require_mutable("List", is_mutable, recv_span, ctx.diagnostics);
        if !arg_tys.is_empty() && !is_compatible(arg_tys[0], elem, ctx.table) {
            ctx.diagnostics.push(
                Diagnostic::error("argument type does not match list element type")
                    .with_label(Label::primary(call_span, "type mismatch")),
            );
        }
        ctx.builtins.void
    } else if method == kw::CONTAINS {
        diagnostics::expect_args(1, arg_tys.len(), call_span, ctx.diagnostics);
        if !arg_tys.is_empty() && !is_compatible(arg_tys[0], elem, ctx.table) {
            ctx.diagnostics.push(
                Diagnostic::error("argument type does not match list element type")
                    .with_label(Label::primary(call_span, "type mismatch")),
            );
        }
        ctx.builtins.bool_ty
    } else if method == kw::REMOVE {
        diagnostics::expect_args(1, arg_tys.len(), call_span, ctx.diagnostics);
        diagnostics::require_mutable("List", is_mutable, recv_span, ctx.diagnostics);
        ctx.builtins.void
    } else {
        ctx.diagnostics
            .push(diagnostics::no_method("List", method, call_span));
        TypeId::ERROR
    }
}

/// Checks a method call on `Map[K, V]`.
fn check_map_method(
    method: Symbol,
    is_mutable: bool,
    arg_tys: &[TypeId],
    call_span: Span,
    recv_span: Span,
    ctx: &mut TypeCheckCtx<'_>,
) -> TypeId {
    if method == kw::LEN {
        diagnostics::expect_args(0, arg_tys.len(), call_span, ctx.diagnostics);
        ctx.builtins.int
    } else if method == kw::CONTAINS {
        diagnostics::expect_args(1, arg_tys.len(), call_span, ctx.diagnostics);
        ctx.builtins.bool_ty
    } else if method == kw::REMOVE {
        diagnostics::expect_args(1, arg_tys.len(), call_span, ctx.diagnostics);
        diagnostics::require_mutable("Map", is_mutable, recv_span, ctx.diagnostics);
        ctx.builtins.void
    } else {
        ctx.diagnostics
            .push(diagnostics::no_method("Map", method, call_span));
        TypeId::ERROR
    }
}

/// Checks a method call on `Set[T]`.
fn check_set_method(
    method: Symbol,
    is_mutable: bool,
    arg_tys: &[TypeId],
    call_span: Span,
    recv_span: Span,
    ctx: &mut TypeCheckCtx<'_>,
) -> TypeId {
    if method == kw::LEN {
        diagnostics::expect_args(0, arg_tys.len(), call_span, ctx.diagnostics);
        ctx.builtins.int
    } else if method == kw::ADD {
        diagnostics::expect_args(1, arg_tys.len(), call_span, ctx.diagnostics);
        diagnostics::require_mutable("Set", is_mutable, recv_span, ctx.diagnostics);
        ctx.builtins.void
    } else if method == kw::CONTAINS {
        diagnostics::expect_args(1, arg_tys.len(), call_span, ctx.diagnostics);
        ctx.builtins.bool_ty
    } else if method == kw::REMOVE {
        diagnostics::expect_args(1, arg_tys.len(), call_span, ctx.diagnostics);
        diagnostics::require_mutable("Set", is_mutable, recv_span, ctx.diagnostics);
        ctx.builtins.void
    } else {
        ctx.diagnostics
            .push(diagnostics::no_method("Set", method, call_span));
        TypeId::ERROR
    }
}

/// Checks a method call on `String`.
fn check_string_method(
    method: Symbol,
    arg_tys: &[TypeId],
    call_span: Span,
    builtins: &BuiltinTypes,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    if method == kw::LEN {
        crate::diagnostics::expect_args(0, arg_tys.len(), call_span, diagnostics);
        builtins.int
    } else {
        diagnostics.push(crate::diagnostics::no_method("String", method, call_span));
        TypeId::ERROR
    }
}

/// Type-checks a `Channel[T]()` constructor expression.
///
/// Validates exactly 1 type argument and 0 value fields.
fn check_channel_constructor(
    type_args: &[spark_ast::TypeExpr],
    fields: &[spark_ast::FieldInit],
    init_span: Span,
    ctx: &mut TypeCheckCtx<'_>,
) -> TypeId {
    if type_args.len() != 1 {
        ctx.diagnostics.push(
            Diagnostic::error("Channel requires exactly one type argument")
                .with_label(Label::primary(init_span, "expected Channel[T]()")),
        );
        return TypeId::ERROR;
    }
    if !fields.is_empty() {
        ctx.diagnostics.push(
            Diagnostic::error("Channel constructor takes no arguments")
                .with_label(Label::primary(init_span, "unexpected fields")),
        );
    }
    let inner = lower_type_expr(&type_args[0], ctx);
    ctx.table.intern(Ty::Channel { inner })
}

/// Checks a method call on `Channel[T]`.
///
/// | Method   | Args       | Returns        |
/// |----------|------------|----------------|
/// | send     | 1 (T)      | Void           |
/// | recv     | 0          | T (blocking)   |
/// | tryRecv  | 0          | T? (Optional)  |
/// | close    | 0          | Void           |
fn check_channel_method(
    method: Symbol,
    inner: TypeId,
    arg_tys: &[TypeId],
    call_span: Span,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    if method == kw::SEND {
        crate::diagnostics::expect_args(1, arg_tys.len(), call_span, diagnostics);
        if !arg_tys.is_empty() && !is_compatible(arg_tys[0], inner, table) {
            diagnostics.push(
                Diagnostic::error("send argument type does not match channel element type")
                    .with_label(Label::primary(call_span, "type mismatch")),
            );
        }
        builtins.void
    } else if method == kw::RECV {
        crate::diagnostics::expect_args(0, arg_tys.len(), call_span, diagnostics);
        inner
    } else if method == kw::TRY_RECV {
        crate::diagnostics::expect_args(0, arg_tys.len(), call_span, diagnostics);
        table.intern(Ty::Optional(inner))
    } else if method == kw::CLOSE {
        crate::diagnostics::expect_args(0, arg_tys.len(), call_span, diagnostics);
        builtins.void
    } else {
        diagnostics.push(crate::diagnostics::no_method("Channel", method, call_span));
        TypeId::ERROR
    }
}

/// Checks a method call on a user-defined type (struct or enum with bindings).
///
/// Looks up the method in `TypeTable::type_methods`, validates arguments,
/// stores the resolved method `DefId` in `method_resolutions`, and returns
/// the method's return type.
fn check_user_method_call(
    type_def_id: DefId,
    method: Symbol,
    arg_tys: &[TypeId],
    recv_span: Span,
    call_span: Span,
    ctx: &mut TypeCheckCtx<'_>,
) -> TypeId {
    // Look up available methods for this type
    let methods = ctx
        .table
        .get_type_methods(type_def_id)
        .map(<[(spark_intern::Symbol, DefId)]>::to_vec);

    if let Some(methods) = methods {
        // Search for the method by name
        if let Some((_, method_def_id)) = methods.iter().find(|(name, _)| *name == method) {
            let method_def_id = *method_def_id;

            // Get the method's function type
            if let Some(fn_type_id) = ctx.table.get_def_type(method_def_id) {
                let fn_ty = ctx.table.resolve(fn_type_id).clone();
                if let Ty::Function { params, ret, .. } = fn_ty {
                    // The first parameter is the receiver (e.g., `self: Foo`),
                    // remaining parameters are the call arguments.
                    let call_params = if params.is_empty() {
                        &[][..]
                    } else {
                        &params[1..]
                    };

                    // Check argument count (excluding receiver)
                    diagnostics::expect_args(
                        call_params.len(),
                        arg_tys.len(),
                        call_span,
                        ctx.diagnostics,
                    );

                    // Check argument types
                    for (i, arg_ty) in arg_tys.iter().enumerate() {
                        if let Some(&expected) = call_params.get(i) {
                            if !is_compatible(*arg_ty, expected, ctx.table) {
                                let expected_name = type_name(expected, ctx.table, ctx.definitions);
                                let found_name = type_name(*arg_ty, ctx.table, ctx.definitions);
                                ctx.diagnostics.push(diagnostics::arg_type_mismatch(
                                    &expected_name,
                                    &found_name,
                                    call_span,
                                ));
                            }
                        }
                    }

                    // Store the method resolution for MIR lowering
                    ctx.table.set_method_resolution(call_span, method_def_id);

                    return ret;
                }
            }

            // Method found but no type — shouldn't happen, return error
            return TypeId::ERROR;
        }
    }

    // No method found
    let tn = ctx
        .definitions
        .get(type_def_id)
        .map_or("<unknown>", |info| info.name.as_str());
    ctx.diagnostics.push(
        Diagnostic::error(format!("type `{tn}` has no method `{method}`"))
            .with_label(Label::primary(recv_span, "no such method")),
    );
    TypeId::ERROR
}

/// Type-checks a `to` expression.
///
/// Checks a single pipeline stage, given the input type from the previous stage.
fn check_pipeline_stage(
    stage: &spark_ast::PipelineStage,
    input_ty: TypeId,
    source_span: Span,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    let base_ty = strip_mutable(input_ty, ctx.table);

    let label = if stage.parallel {
        match &stage.kind {
            PipelineStageKind::ForEach { .. } => "parallel each",
            PipelineStageKind::Map { .. } => "parallel each via",
            PipelineStageKind::Reduce { .. } => "parallel reduce",
        }
    } else {
        match &stage.kind {
            PipelineStageKind::ForEach { .. } => "each",
            PipelineStageKind::Map { .. } => "each via",
            PipelineStageKind::Reduce { .. } => "reduce",
        }
    };

    match &stage.kind {
        PipelineStageKind::ForEach { callback } => {
            let elem_ty = extract_list_elem(
                base_ty,
                label,
                source_span,
                ctx.table,
                ctx.definitions,
                ctx.diagnostics,
            );
            if let ExprKind::Partial { callee, args } = &callback.kind {
                validate_partial_each_callback(
                    callee,
                    args,
                    elem_ty,
                    false,
                    callback,
                    ctx,
                    current_fn_ret,
                );
            } else {
                let cb_ty = check_expr(callback, ctx, current_fn_ret);
                validate_each_callback(
                    cb_ty,
                    elem_ty,
                    false,
                    callback.span,
                    ctx.table,
                    ctx.definitions,
                    ctx.diagnostics,
                );
            }
            ctx.builtins.void
        }
        PipelineStageKind::Map { callback } => {
            let elem_ty = extract_list_elem(
                base_ty,
                label,
                source_span,
                ctx.table,
                ctx.definitions,
                ctx.diagnostics,
            );
            let ret_ty = if let ExprKind::Partial { callee, args } = &callback.kind {
                validate_partial_each_callback(
                    callee,
                    args,
                    elem_ty,
                    true,
                    callback,
                    ctx,
                    current_fn_ret,
                )
            } else {
                let cb_ty = check_expr(callback, ctx, current_fn_ret);
                validate_each_callback(
                    cb_ty,
                    elem_ty,
                    true,
                    callback.span,
                    ctx.table,
                    ctx.definitions,
                    ctx.diagnostics,
                )
            };
            ctx.table.intern(Ty::List(ret_ty))
        }
        PipelineStageKind::Reduce { initial, callback } => {
            let elem_ty = extract_list_elem(
                base_ty,
                label,
                source_span,
                ctx.table,
                ctx.definitions,
                ctx.diagnostics,
            );
            let init_ty = check_expr(initial, ctx, current_fn_ret);
            let cb_ty = check_expr(callback, ctx, current_fn_ret);
            validate_reduce_callback(
                cb_ty,
                init_ty,
                elem_ty,
                callback.span,
                ctx.table,
                ctx.definitions,
                ctx.diagnostics,
            )
        }
    }
}

/// Extracts the element type from a `List[T]`, reporting an error if not a list.
fn extract_list_elem(
    base_ty: TypeId,
    operation: &str,
    span: Span,
    table: &TypeTable,
    definitions: &DefinitionTable,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    match table.resolve(base_ty).clone() {
        Ty::List(elem) => elem,
        Ty::Error => TypeId::ERROR,
        _ => {
            let name = type_name(base_ty, table, definitions);
            diagnostics.push(
                Diagnostic::error(format!("`{operation}` requires a List, found `{name}`"))
                    .with_label(Label::primary(span, "not a List")),
            );
            TypeId::ERROR
        }
    }
}

/// Validates a reduce callback: `(Acc, T) -> Acc`.
///
/// - Must be a 2-parameter function
/// - First param must match the initial value type (accumulator)
/// - Second param must match the element type
/// - Return type must match the accumulator type
fn validate_reduce_callback(
    cb_ty: TypeId,
    init_ty: TypeId,
    elem_ty: TypeId,
    span: Span,
    table: &TypeTable,
    definitions: &DefinitionTable,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    if cb_ty.is_error() || init_ty.is_error() || elem_ty.is_error() {
        return TypeId::ERROR;
    }

    let cb = table.resolve(cb_ty).clone();
    let Ty::Function { params, ret, .. } = cb else {
        diagnostics.push(
            Diagnostic::error("`reduce` callback must be a function")
                .with_label(Label::primary(span, "not a function")),
        );
        return TypeId::ERROR;
    };

    if params.len() != 2 {
        diagnostics.push(
            Diagnostic::error(format!(
                "`reduce` callback must take 2 parameters (accumulator, element), found {}",
                params.len()
            ))
            .with_label(Label::primary(span, "expected 2 parameters")),
        );
        return TypeId::ERROR;
    }

    // First param should be compatible with init type (accumulator)
    if !is_compatible(init_ty, params[0], table) {
        let expected = type_name(params[0], table, definitions);
        let found = type_name(init_ty, table, definitions);
        diagnostics.push(
            Diagnostic::error(format!(
                "`reduce` initial value type mismatch: accumulator expects `{expected}`, found `{found}`"
            ))
            .with_label(Label::primary(span, format!("expected `{expected}`"))),
        );
    }

    // Second param should be compatible with elem type
    if !is_compatible(elem_ty, params[1], table) {
        let expected = type_name(params[1], table, definitions);
        let found = type_name(elem_ty, table, definitions);
        diagnostics.push(
            Diagnostic::error(format!(
                "`reduce` element type mismatch: callback expects `{expected}`, found `{found}`"
            ))
            .with_label(Label::primary(span, format!("expected `{expected}`"))),
        );
    }

    // Return type must match accumulator type
    if !is_compatible(ret, params[0], table) {
        let expected = type_name(params[0], table, definitions);
        let found = type_name(ret, table, definitions);
        diagnostics.push(
            Diagnostic::error(format!(
                "`reduce` callback return type must match accumulator: expected `{expected}`, found `{found}`"
            ))
            .with_label(Label::primary(span, format!("expected `{expected}`"))),
        );
    }

    ret
}

/// Checks a `to` narrowing expression (`to { Pattern -> ... }`).
///
/// - The inner expression must be a union type
/// - Each handler arm handles one or more union members
/// - Unhandled members pass through
/// - Result is the unified handler body types + unhandled member types
fn check_to_narrowing(
    inner_ty: TypeId,
    arms: &[spark_ast::MatchArm],
    span: Span,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    let members = crate::union::union_members(inner_ty, ctx.table);

    if members.len() <= 1 && !matches!(ctx.table.resolve(inner_ty), Ty::Union(_) | Ty::Optional(_))
    {
        ctx.diagnostics.push(
            Diagnostic::error("'to' expression requires a union type")
                .with_label(Label::primary(span, "not a union type")),
        );
    }

    let mut body_types = Vec::with_capacity(arms.len());
    let mut handled_types = Vec::new();

    for arm in arms {
        // Check the pattern — for type narrowing patterns, resolve the type
        if let spark_ast::PatternKind::TypeNarrowing { ty: pat_ty, .. } = &arm.pattern.kind {
            let narrowed_ty = crate::lower::lower_type_expr(pat_ty, ctx);
            handled_types.push(narrowed_ty);
        } else if let spark_ast::PatternKind::Variant { path, .. } = &arm.pattern.kind {
            // Enum variant pattern — resolve it
            let variant_ty = ctx
                .resolutions
                .and_then(|res| res.get_path(path.span))
                .and_then(|def_id| ctx.table.get_def_type(def_id))
                .unwrap_or(TypeId::ERROR);
            if !variant_ty.is_error() {
                handled_types.push(variant_ty);
            }
        }

        let body_ty = check_expr(&arm.body, ctx, current_fn_ret);
        body_types.push(body_ty);
    }

    // Determine unhandled member types
    let unhandled: Vec<TypeId> = members
        .iter()
        .copied()
        .filter(|m| !handled_types.contains(m))
        .collect();

    // Unify all body types
    let mut result_ty = if let Some(&first) = body_types.first() {
        first
    } else {
        ctx.builtins.void
    };
    for &bty in &body_types[1..] {
        result_ty = unify_branches(result_ty, bty, ctx.table, ctx.builtins);
    }

    // If there are unhandled types, the result includes them
    if unhandled.is_empty() {
        result_ty
    } else {
        // Result is union of handler body types + unhandled types
        for &u in &unhandled {
            result_ty = unify_branches(result_ty, u, ctx.table, ctx.builtins);
        }
        result_ty
    }
}

/// Adapts the parameter and return types of a builtin list function (sort, take, reverse)
/// to match the actual list element type passed as the first argument.
///
/// Builtin list functions are registered with `List[Int]` as a default, but should
/// accept any `List[T]`. This function substitutes the actual element type at call sites.
fn adapt_builtin_list_fn(
    def_id: DefId,
    first_arg_ty: TypeId,
    params: Vec<TypeId>,
    ret: TypeId,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
) -> (Vec<TypeId>, TypeId) {
    let Some(info) = definitions.get(def_id) else {
        return (params, ret);
    };
    if info.kind != DefinitionKind::BuiltinFunction {
        return (params, ret);
    }

    // Extract the actual element type from the first argument
    let actual_elem = match table.resolve(first_arg_ty).clone() {
        Ty::List(elem) => elem,
        _ => return (params, ret),
    };
    let actual_list = table.intern(Ty::List(actual_elem));

    if info.name == kw::SORT || info.name == kw::REVERSE {
        (vec![actual_list], actual_list)
    } else if info.name == kw::TAKE {
        (vec![actual_list, builtins.int], actual_list)
    } else {
        (params, ret)
    }
}

/// Checks a pipeline apply expression (`expr via func{args}` or `expr to func{args}`).
fn check_pipeline_apply(
    inner_ty: TypeId,
    operation: &spark_ast::Ident,
    args: &[spark_ast::PartialArg],
    span: Span,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    // Generic function-call validation: `expr via func{a: v}` → `func(expr, v)`
    let func_def_id = ctx.resolutions.and_then(|r| r.get_ident(operation.span));
    let Some(def_id) = func_def_id else {
        // Unresolved — still check arg values for errors
        for arg in args {
            let _ = check_expr(&arg.value, ctx, current_fn_ret);
        }
        return TypeId::ERROR;
    };
    let Some(func_ty_id) = ctx.table.get_def_type(def_id) else {
        return TypeId::ERROR;
    };
    let func_ty = ctx.table.resolve(func_ty_id).clone();
    let Ty::Function { params, ret, .. } = func_ty else {
        ctx.diagnostics.push(
            Diagnostic::error("`via` target must be a function")
                .with_label(Label::primary(operation.span, "not a function")),
        );
        return TypeId::ERROR;
    };

    // Adapt builtin list function types to match actual input type
    let (params, ret) = adapt_builtin_list_fn(
        def_id,
        inner_ty,
        params,
        ret,
        ctx.table,
        ctx.builtins,
        ctx.definitions,
    );

    // Build full arg list: [inner_ty, partial_arg_types...]
    let mut all_arg_types = vec![inner_ty];
    for arg in args {
        all_arg_types.push(check_expr(&arg.value, ctx, current_fn_ret));
    }

    // Arity check
    if all_arg_types.len() != params.len() {
        ctx.diagnostics.push(
            Diagnostic::error(format!(
                "`via {}` expected {} argument{}, found {}",
                operation.name,
                params.len(),
                if params.len() == 1 { "" } else { "s" },
                all_arg_types.len()
            ))
            .with_label(Label::primary(span, "wrong number of arguments")),
        );
        return TypeId::ERROR;
    }

    // Type-check each argument against function params
    for (i, (&arg_ty, &param_ty)) in all_arg_types.iter().zip(params.iter()).enumerate() {
        if !is_compatible(arg_ty, param_ty, ctx.table) {
            let expected_name = type_name(param_ty, ctx.table, ctx.definitions);
            let found_name = type_name(arg_ty, ctx.table, ctx.definitions);
            if i == 0 {
                ctx.diagnostics.push(
                    Diagnostic::error(format!(
                        "`via {}` first argument type mismatch: expected `{expected_name}`, found `{found_name}`",
                        operation.name
                    ))
                    .with_label(Label::primary(span, format!("expected `{expected_name}`"))),
                );
            } else {
                let arg_span = args[i - 1].value.span;
                ctx.diagnostics.push(diagnostics::arg_type_mismatch(
                    &expected_name,
                    &found_name,
                    arg_span,
                ));
            }
        }
    }

    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::register_builtins;

    #[test]
    fn compatible_same_types() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        assert!(is_compatible(builtins.int, builtins.int, &table));
    }

    #[test]
    fn compatible_error_with_anything() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        assert!(is_compatible(TypeId::ERROR, builtins.int, &table));
        assert!(is_compatible(builtins.int, TypeId::ERROR, &table));
    }

    #[test]
    fn compatible_never_with_anything() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        assert!(is_compatible(builtins.never, builtins.int, &table));
        assert!(is_compatible(builtins.int, builtins.never, &table));
    }

    #[test]
    fn compatible_null_with_optional() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let opt_int = table.intern(Ty::Optional(builtins.int));
        assert!(is_compatible(builtins.null, opt_int, &table));
    }

    #[test]
    fn compatible_t_with_optional_t() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let opt_int = table.intern(Ty::Optional(builtins.int));
        assert!(is_compatible(builtins.int, opt_int, &table));
    }

    #[test]
    fn compatible_member_of_union() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let union_ty = table.intern(Ty::Union(vec![builtins.int, builtins.string]));
        assert!(is_compatible(builtins.int, union_ty, &table));
        assert!(is_compatible(builtins.string, union_ty, &table));
        assert!(!is_compatible(builtins.bool_ty, union_ty, &table));
    }

    #[test]
    fn compatible_t_with_ref_t() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let ref_int = table.intern(Ty::Ref {
            inner: builtins.int,
            mutable: false,
            lifetime: crate::ty::LifetimeId::ANONYMOUS,
        });
        // T is compatible with Ref[T] (auto-borrow)
        assert!(is_compatible(builtins.int, ref_int, &table));
        // Ref[T] is compatible with T (auto-deref)
        assert!(is_compatible(ref_int, builtins.int, &table));
    }

    #[test]
    fn incompatible_different_types() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        assert!(!is_compatible(builtins.int, builtins.string, &table));
    }

    #[test]
    fn unify_same() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        assert_eq!(
            unify_branches(builtins.int, builtins.int, &mut table, &builtins),
            builtins.int
        );
    }

    #[test]
    fn unify_with_never() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        assert_eq!(
            unify_branches(builtins.never, builtins.int, &mut table, &builtins),
            builtins.int
        );
        assert_eq!(
            unify_branches(builtins.int, builtins.never, &mut table, &builtins),
            builtins.int
        );
    }

    #[test]
    fn unify_different_creates_union() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let result = unify_branches(builtins.int, builtins.string, &mut table, &builtins);
        assert!(matches!(table.resolve(result), Ty::Union(_)));
    }
}
