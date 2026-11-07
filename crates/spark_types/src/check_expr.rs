//! Expression type checking (Pass 2).
//!
//! Walks expression trees and returns the type of each expression,
//! recording the result in the type table's `expr_types` map.

use spark_ast::{
    BinaryOp, Block, Expr, ExprKind, FnParam, LiteralKind, PipelineStageKind, StringPart, TypeExpr,
    UnaryOp,
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
use crate::ty::{LifetimeId, Ty, TypeId};

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
                    // Check for type narrowing (e.g., after `if x is Null { return }`)
                    if let Some(narrowed) = ctx.get_narrowed_type(def_id) {
                        return narrowed;
                    }
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
            if let Some(protocol) = crate::parsing::protocol_for_call(callee, ctx) {
                return protocol.check_call(args, expr.span, expected_ty, ctx, current_fn_ret);
            }
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
        ExprKind::Index { base, index } => check_index_access(base, index, ctx, current_fn_ret),

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
        } => check_if_expr(
            condition,
            then_branch,
            else_branch.as_deref(),
            ctx,
            current_fn_ret,
        ),

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
        } => check_lambda(params, ret_type.as_ref(), body, ctx),

        // === Coalesce (??) ===
        ExprKind::Coalesce { lhs, rhs } => check_coalesce(lhs, rhs, ctx, current_fn_ret),

        // === Propagate (!) ===
        ExprKind::Propagate(inner_expr) => {
            check_propagate(inner_expr, expr.span, ctx, current_fn_ret, expected_ty)
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
            let expected_elem = extract_expected_elem(expected_ty, ctx.table, |ty| match ty {
                Ty::List(elem) => Some(*elem),
                _ => None,
            });

            if elems.is_empty() {
                ctx.table
                    .intern(Ty::List(expected_elem.unwrap_or(TypeId::ERROR)))
            } else {
                let first_ty = check_expr_expected(&elems[0], ctx, current_fn_ret, expected_elem);
                for elem in &elems[1..] {
                    let _ = check_expr_expected(elem, ctx, current_fn_ret, expected_elem);
                }
                ctx.table.intern(Ty::List(resolve_with_expected(
                    first_ty,
                    expected_elem,
                    ctx.table,
                )))
            }
        }

        ExprKind::Map(entries) => {
            let expected_kv = extract_expected_elem(expected_ty, ctx.table, |ty| match ty {
                Ty::Map(k, v) => Some((*k, *v)),
                _ => None,
            });
            let expected_key = expected_kv.map(|(k, _)| k);
            let expected_val = expected_kv.map(|(_, v)| v);

            if entries.is_empty() {
                let key_ty = expected_key.unwrap_or(TypeId::ERROR);
                let val_ty = expected_val.unwrap_or(TypeId::ERROR);
                ctx.table.intern(Ty::Map(key_ty, val_ty))
            } else {
                let key_ty =
                    check_expr_expected(&entries[0].key, ctx, current_fn_ret, expected_key);
                let val_ty =
                    check_expr_expected(&entries[0].value, ctx, current_fn_ret, expected_val);
                for entry in &entries[1..] {
                    let _ = check_expr_expected(&entry.key, ctx, current_fn_ret, expected_key);
                    let _ = check_expr_expected(&entry.value, ctx, current_fn_ret, expected_val);
                }
                let final_key = resolve_with_expected(key_ty, expected_key, ctx.table);
                let final_val = resolve_with_expected(val_ty, expected_val, ctx.table);
                ctx.table.intern(Ty::Map(final_key, final_val))
            }
        }

        ExprKind::Set(elems) => {
            let expected_elem = extract_expected_elem(expected_ty, ctx.table, |ty| match ty {
                Ty::Set(elem) => Some(*elem),
                _ => None,
            });

            if elems.is_empty() {
                ctx.table
                    .intern(Ty::Set(expected_elem.unwrap_or(TypeId::ERROR)))
            } else {
                let first_ty = check_expr_expected(&elems[0], ctx, current_fn_ret, expected_elem);
                for elem in &elems[1..] {
                    let _ = check_expr_expected(elem, ctx, current_fn_ret, expected_elem);
                }
                ctx.table.intern(Ty::Set(resolve_with_expected(
                    first_ty,
                    expected_elem,
                    ctx.table,
                )))
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
        ExprKind::Paren(inner) => check_expr_expected(inner, ctx, current_fn_ret, expected_ty),

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
                args.first().map_or(expr.span, |arg| arg.value.span),
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
        } => check_list_range(start, end, step.as_deref(), ctx, current_fn_ret),

        ExprKind::Partial { callee, args } => check_partial(callee, args, ctx, current_fn_ret),

        ExprKind::Error => TypeId::ERROR,
    }
}

/// Type-checks a block expression.
pub fn check_block(
    block: &Block,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    ctx.push_narrowing_scope();

    for stmt in &block.stmts {
        check_stmt(stmt, ctx, current_fn_ret);
    }

    let ty = block.expr.as_ref().map_or(ctx.builtins.void, |expr| {
        check_expr(expr, ctx, current_fn_ret)
    });
    ctx.table.set_expr_type(block.id, ty);

    ctx.pop_narrowing_scope();
    ty
}

// === If expression ===

/// Type-checks an `if` expression, including type narrowing from `is` tests.
fn check_if_expr(
    condition: &Expr,
    then_branch: &Expr,
    else_branch: Option<&Expr>,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    let cond_ty = check_expr(condition, ctx, current_fn_ret);

    if !is_compatible(cond_ty, ctx.builtins.bool_ty, ctx.table) {
        ctx.diagnostics.push(
            Diagnostic::error("if condition must be Bool")
                .with_label(Label::primary(condition.span, "expected Bool")),
        );
    }

    // Detect `is` narrowing: `if x is T { ... }`
    let narrowing_info = extract_is_narrowing(condition, ctx);

    // Positive narrowing: inside then-branch, narrow ident to the tested type
    let then_ty = if let Some((def_id, tested_ty)) = narrowing_info {
        ctx.push_narrowing_scope();
        ctx.add_narrowing(def_id, tested_ty);
        let ty = check_expr(then_branch, ctx, current_fn_ret);
        ctx.pop_narrowing_scope();
        ty
    } else {
        check_expr(then_branch, ctx, current_fn_ret)
    };

    // Negative narrowing: if then-branch diverges (Never), narrow
    // the ident to the original type minus the tested type for the
    // rest of the current scope.
    if let Some((def_id, tested_ty)) = narrowing_info {
        let then_resolved = ctx.table.resolve(then_ty);
        if matches!(then_resolved, Ty::Never) {
            let base_ty = ctx
                .get_narrowed_type(def_id)
                .unwrap_or_else(|| ctx.table.get_def_type(def_id).unwrap_or(TypeId::ERROR));
            let narrowed = subtract_type(base_ty, tested_ty, ctx.table);
            if narrowed != base_ty {
                ctx.add_narrowing(def_id, narrowed);
            }
        }
    }

    else_branch.map_or(ctx.builtins.void, |else_branch| {
        let else_ty = check_expr(else_branch, ctx, current_fn_ret);
        unify_branches(then_ty, else_ty, ctx.table, ctx.builtins)
    })
}

// === Lambda ===

/// Type-checks a lambda expression, registering parameter types and returning
/// the resulting function type.
fn check_lambda(
    params: &[FnParam],
    ret_type: Option<&TypeExpr>,
    body: &Expr,
    ctx: &mut TypeCheckCtx<'_>,
) -> TypeId {
    let param_types: Vec<TypeId> = params.iter().map(|p| lower_type_expr(&p.ty, ctx)).collect();

    // Register param types
    for (i, param) in params.iter().enumerate() {
        if let Some(res) = ctx.resolutions {
            if let Some(def_id) = res.get_binding(param.name.span) {
                ctx.table.set_def_type(def_id, param_types[i]);
            }
        }
    }

    let declared_ret = ret_type.map(|rt| lower_type_expr(rt, ctx));

    let body_ty = check_expr(body, ctx, declared_ret);
    let ret = declared_ret.unwrap_or(body_ty);

    ctx.table.intern(Ty::Function {
        params: param_types,
        ret,
        is_async: false,
    })
}

// === Coalesce (??) ===

/// Type-checks the null-coalescing operator `??`.
fn check_coalesce(
    lhs: &Expr,
    rhs: &Expr,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
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

/// Type-checks the error propagation operator `!`.
///
/// Handles three cases:
/// 1. `Optional(T)` — unwraps to `T`, propagates `Null`
/// 2. `Union` types — splits into success/error types via `split_union`
/// 3. Fallback — unwraps nullable types
fn check_propagate(
    inner_expr: &Expr,
    span: Span,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
    expected_ty: Option<TypeId>,
) -> TypeId {
    let expected_result = if let ExprKind::Call { callee, .. } = &ungroup(inner_expr).kind {
        crate::parsing::protocol_for_call(callee, ctx)
            .zip(expected_ty)
            .map(|(protocol, success)| protocol.result_type(success, ctx.table))
    } else {
        None
    };
    let inner_ty = check_expr_expected(inner_expr, ctx, current_fn_ret, expected_result);

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
                    .with_label(Label::primary(span, "propagates `Null` here")),
                );
            }
        } else {
            ctx.diagnostics
                .push(diagnostics::propagation_outside_fn(span));
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
                            .with_label(Label::primary(span, format!("propagates `{error_name}` here"))),
                        );
                    }
                }
            } else {
                ctx.diagnostics
                    .push(diagnostics::propagation_outside_fn(span));
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
                    .with_label(Label::primary(span, "propagates `Null` here")),
                );
            }
        } else {
            ctx.diagnostics
                .push(diagnostics::propagation_outside_fn(span));
        }
    }
    result
}

// === Index access ===

/// Type-checks an index expression (`base[index]`).
///
/// Supports `List[Int]`, `Map[K]`, `Tuple[literal]`, with appropriate
/// error diagnostics for invalid index types or out-of-bounds access.
fn check_index_access(
    base: &Expr,
    index: &Expr,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    let base_ty = check_expr(base, ctx, current_fn_ret);
    let index_ty = check_expr(index, ctx, current_fn_ret);

    match ctx.table.resolve(strip_mutable(base_ty, ctx.table)).clone() {
        Ty::List(elem) => {
            let resolved_idx = ctx.table.resolve(strip_mutable(index_ty, ctx.table));
            if !resolved_idx.is_integer() && index_ty != TypeId::ERROR {
                ctx.diagnostics.push(
                    Diagnostic::error("list index must be an integer").with_label(Label::primary(
                        index.span,
                        format!(
                            "expected `Int`, found `{}`",
                            type_name(index_ty, ctx.table, ctx.definitions)
                        ),
                    )),
                );
                return TypeId::ERROR;
            }
            ctx.table.intern(Ty::Optional(elem))
        }
        Ty::Map(key, value) => {
            if check_collection_argument(index_ty, key, index.span, "map key type mismatch", ctx) {
                ctx.table.intern(Ty::Optional(value))
            } else {
                TypeId::ERROR
            }
        }
        Ty::Tuple(elems) => {
            if let ExprKind::Literal(spark_ast::Literal {
                kind: LiteralKind::Int(val),
                ..
            }) = &index.kind
            {
                #[allow(clippy::cast_sign_loss)]
                let idx = *val as usize;
                if idx >= elems.len() {
                    ctx.diagnostics
                        .push(diagnostics::tuple_oob(idx, elems.len(), index.span));
                    TypeId::ERROR
                } else {
                    elems[idx]
                }
            } else {
                ctx.diagnostics.push(
                    Diagnostic::error("tuple index must be an integer literal")
                        .with_label(Label::primary(index.span, "expected an integer literal")),
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

// === List range ===

/// Type-checks a list range expression (`[start..end]` or `[start..end..step]`).
///
/// All components must be `Int`; returns `List[Int]`.
fn check_list_range(
    start: &Expr,
    end: &Expr,
    step: Option<&Expr>,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
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

            // Check if the callee is print — Displayable structs are accepted
            let is_print_callee = callee_def_id.is_some_and(|did| {
                ctx.definitions.get(did).is_some_and(|info| {
                    info.name == spark_intern::kw::PRINT
                        && matches!(info.kind, DefinitionKind::BuiltinFunction)
                })
            });

            for (i, (arg, &arg_ty)) in args.iter().zip(arg_tys.iter()).enumerate() {
                if let Some(&expected) = params.get(i) {
                    if !is_compatible(arg_ty, expected, ctx.table) {
                        // Allow Displayable structs as print arguments
                        let is_displayable_struct =
                            is_print_callee && is_displayable_type(arg_ty, ctx.table);
                        if !is_displayable_struct {
                            let expected_name = type_name(expected, ctx.table, ctx.definitions);
                            let found_name = type_name(arg_ty, ctx.table, ctx.definitions);
                            ctx.diagnostics.push(diagnostics::arg_type_mismatch(
                                &expected_name,
                                &found_name,
                                arg.value.span,
                            ));
                        }
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
pub(crate) fn find_callee_def_id(
    callee_span: Span,
    resolutions: Option<&ResolutionMap>,
) -> Option<DefId> {
    let res = resolutions?;
    // Try both path and ident resolution (callee could be either)
    res.get_path(callee_span)
        .or_else(|| res.get_ident(callee_span))
}

pub(crate) fn ungroup(mut expr: &Expr) -> &Expr {
    while let ExprKind::Paren(inner) = &expr.kind {
        expr = inner;
    }
    expr
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
            if let Ok(index) = field_str.parse::<usize>() {
                if index >= elems.len() {
                    diagnostics.push(crate::diagnostics::tuple_oob(
                        index,
                        elems.len(),
                        field_span,
                    ));
                    TypeId::ERROR
                } else {
                    let field_ty = elems[index];
                    if base_is_mutable {
                        field_ty
                    } else {
                        strip_mutable(field_ty, table)
                    }
                }
            } else {
                diagnostics.push(
                    Diagnostic::error("tuple fields must be accessed with numeric indices")
                        .with_label(Label::primary(field_span, "expected a numeric index")),
                );
                TypeId::ERROR
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
#[allow(clippy::too_many_lines)]
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
            let subst = if type_args.is_empty() {
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
            } else {
                // Explicit type args provided: Pair[Int, String] { ... }
                if type_args.len() == params.len() {
                    let lowered: Vec<TypeId> = type_args
                        .iter()
                        .map(|ta| lower_type_expr(ta, ctx))
                        .collect();
                    Some(crate::substitute::build_subst_map(params, &lowered))
                } else {
                    ctx.diagnostics.push(
                        Diagnostic::error(format!(
                            "expected {} type argument(s), found {}",
                            params.len(),
                            type_args.len()
                        ))
                        .with_label(Label::primary(init_span, "wrong number of type arguments")),
                    );
                    None
                }
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

    // List[A] is compatible with List[B] if A is compatible with B
    if let (Ty::List(a_elem), Ty::List(b_elem)) = (actual_ty, expected_ty) {
        return is_compatible(*a_elem, *b_elem, table);
    }

    // Map[AK, AV] is compatible with Map[BK, BV] if keys and values are compatible
    if let (Ty::Map(ak, av), Ty::Map(bk, bv)) = (actual_ty, expected_ty) {
        return is_compatible(*ak, *bk, table) && is_compatible(*av, *bv, table);
    }

    // Set[A] is compatible with Set[B] if A is compatible with B
    if let (Ty::Set(a_elem), Ty::Set(b_elem)) = (actual_ty, expected_ty) {
        return is_compatible(*a_elem, *b_elem, table);
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

    // Create a union of the two types, flattening nested unions.
    let mut members = Vec::new();
    match a_ty {
        Ty::Union(ms) => members.extend_from_slice(ms),
        _ => members.push(a),
    }
    match b_ty {
        Ty::Union(ms) => members.extend_from_slice(ms),
        _ => members.push(b),
    }
    members.sort();
    members.dedup();
    if members.len() == 1 {
        return members[0];
    }
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

/// If `expected` is `Some` and compatible with `inferred`, use `expected`; otherwise use `inferred`.
fn resolve_with_expected(inferred: TypeId, expected: Option<TypeId>, table: &TypeTable) -> TypeId {
    match expected {
        Some(exp) if is_compatible(inferred, exp, table) => exp,
        _ => inferred,
    }
}

/// Extracts an inner type from an expected collection type after stripping mutability.
///
/// `extractor` receives the resolved, unwrapped type and returns the inner component(s)
/// if the type matches the expected collection shape (e.g., `Ty::List(elem) => Some(*elem)`).
fn extract_expected_elem<T>(
    expected_ty: Option<TypeId>,
    table: &TypeTable,
    extractor: impl FnOnce(&Ty) -> Option<T>,
) -> Option<T> {
    expected_ty.and_then(|ty| {
        let stripped = strip_mutable(ty, table);
        extractor(table.resolve(stripped))
    })
}

/// Returns `true` if the type is `Mutable(Ref { .. })`, i.e. a mutable reference parameter.
fn is_mutable_ref_param(ty: TypeId, table: &TypeTable) -> bool {
    if let Ty::Mutable(inner) = table.resolve(ty) {
        matches!(table.resolve(*inner), Ty::Ref { .. })
    } else {
        false
    }
}

/// Validates the callback type for `each` / `map`.
///
/// For `each`: callback must be `(T) -> Void` (though we don't enforce Void return).
/// For `map`: callback must be `(T) -> U`; returns `U`.
///
/// Returns the callback's return type (used by `map` to determine `List[U]`).
fn validate_each_callback(
    cb_ty: TypeId,
    elem_ty: TypeId,
    is_map: bool,
    span: Span,
    table: &TypeTable,
    definitions: &DefinitionTable,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    if cb_ty == TypeId::ERROR || elem_ty == TypeId::ERROR {
        return TypeId::ERROR;
    }
    let label = if is_map { "map" } else { "each" };
    if let Ty::Function { params, ret, .. } = table.resolve(cb_ty).clone() {
        if params.len() != 1 {
            diagnostics.push(
                Diagnostic::error(format!(
                    "`{label}` callback must take exactly 1 parameter, found {}",
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
                    "`{label}` callback parameter type mismatch: expected `{expected}`, found `{found}`",
                ))
                .with_label(Label::primary(span, "type mismatch")),
            );
        }
        ret
    } else {
        let name = type_name(cb_ty, table, definitions);
        diagnostics.push(
            Diagnostic::error(format!(
                "`{label}` callback must be a function, found `{name}`",
            ))
            .with_label(Label::primary(span, "not a function")),
        );
        TypeId::ERROR
    }
}

/// Validates a partial-application callback in an `each`/`map` pipeline stage.
///
/// For `list map func{b: val}`, the element fills the first parameter and the
/// bound arguments fill the rest. Returns the function's return type and stores a
/// reduced 1-parameter function type on `callback_expr` so downstream passes can
/// extract the return type via `extract_fn_ret_ty`.
fn validate_partial_each_callback(
    callee: &Expr,
    args: &[spark_ast::PartialArg],
    elem_ty: TypeId,
    is_map: bool,
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

    let label = if is_map { "map" } else { "each" };
    let func_ty = ctx.table.resolve(callee_ty).clone();
    let Ty::Function { params, ret, .. } = func_ty else {
        ctx.diagnostics.push(
            Diagnostic::error(format!("`{label}` partial callback must be a function",))
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
                "`{label}` partial callback expected {} parameter{}, found {} (1 element + {} bound)",
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
                "`{label}` element type mismatch: expected `{expected}`, found `{found}`",
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

/// Type-checks a standalone partial application expression.
///
/// For `func{a: v1, c: v3}` where `func(a: A, b: B, c: C): R`:
/// 1. Validates the callee is a function.
/// 2. Resolves named arguments to parameter positions.
/// 3. Validates argument types against parameter types.
/// 4. Returns a new function type with only the unbound parameters: `(B): R`.
fn check_partial(
    callee: &Expr,
    args: &[spark_ast::PartialArg],
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    let callee_ty = check_expr(callee, ctx, current_fn_ret);
    // Check all arg values for diagnostics even if we bail early
    let arg_types: Vec<TypeId> = args
        .iter()
        .map(|a| check_expr(&a.value, ctx, current_fn_ret))
        .collect();

    if callee_ty.is_error() {
        return TypeId::ERROR;
    }

    let callee_resolved = ctx.table.resolve(callee_ty).clone();
    let Ty::Function {
        params,
        ret,
        is_async,
    } = callee_resolved
    else {
        ctx.diagnostics.push(
            Diagnostic::error("partial application requires a function")
                .with_label(Label::primary(callee.span, "not a function")),
        );
        return TypeId::ERROR;
    };

    // Look up parameter names via the callee's DefId
    let callee_def_id = find_callee_def_id(callee.span, ctx.resolutions);
    let param_names = callee_def_id.and_then(|did| ctx.table.get_fn_param_names(did));
    let Some(param_names) = param_names else {
        ctx.diagnostics.push(
            Diagnostic::error("cannot resolve parameter names for partial application")
                .with_label(Label::primary(callee.span, "parameter names unavailable")),
        );
        return TypeId::ERROR;
    };
    let param_names = param_names.to_vec();

    // Resolve each named arg to a parameter index and validate
    let mut bound: Vec<bool> = vec![false; params.len()];

    for (arg_idx, arg) in args.iter().enumerate() {
        let name = arg.name.name;

        // Find matching parameter by name
        let pos = param_names.iter().position(|&n| n == name);
        let Some(pos) = pos else {
            ctx.diagnostics.push(
                Diagnostic::error(format!(
                    "unknown parameter `{}` in partial application",
                    name.as_str(),
                ))
                .with_label(Label::primary(arg.name.span, "no such parameter")),
            );
            continue;
        };

        // Check for duplicate binding
        if bound[pos] {
            ctx.diagnostics.push(
                Diagnostic::error(format!("parameter `{}` is already bound", name.as_str(),))
                    .with_label(Label::primary(arg.name.span, "duplicate binding")),
            );
            continue;
        }

        bound[pos] = true;

        // Validate argument type against parameter type
        let param_ty = params[pos];
        let actual_ty = arg_types[arg_idx];
        if !actual_ty.is_error() && !is_compatible(actual_ty, param_ty, ctx.table) {
            let expected = type_name(param_ty, ctx.table, ctx.definitions);
            let found = type_name(actual_ty, ctx.table, ctx.definitions);
            ctx.diagnostics.push(diagnostics::arg_type_mismatch(
                &expected,
                &found,
                arg.value.span,
            ));
        }
    }

    // Build the reduced function type with only unbound parameters
    let unbound_params: Vec<TypeId> = params
        .iter()
        .enumerate()
        .filter(|(i, _)| !bound[*i])
        .map(|(_, &ty)| ty)
        .collect();

    ctx.table.intern(Ty::Function {
        params: unbound_params,
        ret,
        is_async,
    })
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

/// Subtracts `to_remove` from `original`, returning the narrowed type.
///
/// - `Optional(T)` minus `Null` → `T`
/// - `Optional(T)` minus `T` → `Null`
/// - `Union([...])` minus a member → remaining union or single type
/// - `Mutable(T)` — recurse on `T`, re-wrap if changed
/// - `Ref { inner, .. }` — recurse on `inner`, re-wrap if changed
/// - Otherwise returns `original` unchanged.
fn subtract_type(original: TypeId, to_remove: TypeId, table: &mut TypeTable) -> TypeId {
    // Two-phase approach: read from the type table first, then mutate.
    // This avoids cloning the resolved Ty to break the borrow conflict.
    enum Action {
        ReturnInner(TypeId),
        ReturnNull,
        RebuildUnion(Vec<TypeId>),
        RewrapMutable(TypeId),
        RewrapRef {
            inner: TypeId,
            mutable: bool,
            lifetime: LifetimeId,
        },
        NoChange,
    }

    let action = match table.resolve(original) {
        Ty::Mutable(inner) => {
            let inner = *inner;
            Action::RewrapMutable(inner)
        }
        Ty::Ref {
            inner,
            mutable,
            lifetime,
        } => {
            let inner = *inner;
            let mutable = *mutable;
            let lifetime = *lifetime;
            Action::RewrapRef {
                inner,
                mutable,
                lifetime,
            }
        }
        Ty::Optional(inner) => {
            let inner = *inner;
            if to_remove == inner {
                Action::ReturnNull
            } else {
                // Will check against Null after releasing the borrow.
                Action::ReturnInner(inner)
            }
        }
        Ty::Union(members) => {
            let remaining: Vec<TypeId> = members
                .iter()
                .copied()
                .filter(|&m| m != to_remove)
                .collect();
            if remaining.len() == members.len() {
                Action::NoChange
            } else {
                Action::RebuildUnion(remaining)
            }
        }
        _ => Action::NoChange,
    };

    // Immutable borrow released — safe to call intern().
    match action {
        Action::RewrapMutable(inner) => {
            let narrowed = subtract_type(inner, to_remove, table);
            if narrowed == inner {
                original
            } else {
                table.intern(Ty::Mutable(narrowed))
            }
        }
        Action::RewrapRef {
            inner,
            mutable,
            lifetime,
        } => {
            let narrowed = subtract_type(inner, to_remove, table);
            if narrowed == inner {
                original
            } else {
                table.intern(Ty::Ref {
                    inner: narrowed,
                    mutable,
                    lifetime,
                })
            }
        }
        Action::ReturnNull => table.intern(Ty::Null),
        Action::ReturnInner(inner) => {
            let null_id = table.intern(Ty::Null);
            if to_remove == null_id {
                inner
            } else {
                original
            }
        }
        Action::RebuildUnion(remaining) => match remaining.len() {
            0 => original,
            1 => remaining[0],
            _ => table.intern(Ty::Union(remaining)),
        },
        Action::NoChange => original,
    }
}

/// Extracts narrowing information from an `is` condition.
///
/// If the condition is `<ident> is <Type>`, returns the ident's `DefId`
/// and the tested `TypeId`. Returns `None` for non-ident expressions
/// or non-`is` conditions.
fn extract_is_narrowing(condition: &Expr, ctx: &mut TypeCheckCtx<'_>) -> Option<(DefId, TypeId)> {
    if let ExprKind::Is { expr: inner, ty } = &condition.kind {
        if let ExprKind::Ident(ident) = &inner.kind {
            let def_id = ctx.resolutions?.get_ident(ident.span)?;
            let tested_ty = lower_type_expr(ty, ctx);
            return Some((def_id, tested_ty));
        }
    }
    None
}

/// Type-checks a method call on a collection, string, or user-defined type receiver.
fn check_method_call(
    recv_ty: TypeId,
    method: Symbol,
    arg_tys: &[TypeId],
    arg_span: Span,
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
        Ty::Map(key, _) | Ty::Set(key) => {
            let is_set = matches!(stripped, Ty::Set(_));
            let (collection, diagnostic) = if is_set {
                ("Set", "set element type mismatch")
            } else {
                ("Map", "map key type mismatch")
            };
            let (result, mutates) = match method {
                kw::LEN => {
                    diagnostics::expect_args(0, arg_tys.len(), call_span, ctx.diagnostics);
                    return ctx.builtins.int;
                }
                kw::CONTAINS => (ctx.builtins.bool_ty, false),
                kw::REMOVE => (ctx.builtins.void, true),
                kw::ADD if is_set => (ctx.builtins.void, true),
                _ => {
                    ctx.diagnostics
                        .push(diagnostics::no_method(collection, method, call_span));
                    return TypeId::ERROR;
                }
            };

            diagnostics::expect_args(1, arg_tys.len(), call_span, ctx.diagnostics);
            if mutates {
                diagnostics::require_mutable(collection, is_mutable, recv_span, ctx.diagnostics);
            }
            let [arg] = arg_tys else {
                return TypeId::ERROR;
            };
            if check_collection_argument(*arg, key, arg_span, diagnostic, ctx) {
                result
            } else {
                TypeId::ERROR
            }
        }
        Ty::String => check_string_method(method, arg_tys, call_span, ctx),
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
            check_user_method_call(def_id, method, arg_tys, recv_span, call_span, &[], ctx)
        }
        Ty::Applied { base, ref args } => {
            check_user_method_call(base, method, arg_tys, recv_span, call_span, args, ctx)
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

fn check_collection_argument(
    actual: TypeId,
    expected: TypeId,
    span: Span,
    diagnostic: &str,
    ctx: &mut TypeCheckCtx<'_>,
) -> bool {
    if actual.is_error() {
        return false;
    }
    // The runtime reads key and element bytes using the collection's storage type.
    if expected.is_error() || strip_mutable(actual, ctx.table) == strip_mutable(expected, ctx.table)
    {
        return true;
    }

    let actual = type_name(actual, ctx.table, ctx.definitions);
    let expected = type_name(expected, ctx.table, ctx.definitions);
    ctx.diagnostics
        .push(Diagnostic::error(diagnostic).with_label(Label::primary(
            span,
            format!("expected `{expected}`, found `{actual}`"),
        )));
    false
}

/// Checks a method call on `String`.
fn check_string_method(
    method: Symbol,
    arg_tys: &[TypeId],
    call_span: Span,
    ctx: &mut TypeCheckCtx<'_>,
) -> TypeId {
    if method == kw::SUBSTRING {
        let substring = ctx
            .definitions
            .iter()
            .find(|(_, info)| {
                info.name == kw::SUBSTRING
                    && info.kind == DefinitionKind::Function
                    && info.module.segments() == [kw::SPARK, kw::TEXT]
            })
            .and_then(|(id, _)| {
                let ty = ctx.table.get_def_type(id)?;
                let Ty::Function { params, ret, .. } = ctx.table.resolve(ty) else {
                    return None;
                };
                Some((id, params.get(1..).unwrap_or_default().to_vec(), *ret))
            });
        if let Some((def_id, params, ret)) = substring {
            validate_call_args(&params, arg_tys, call_span, ctx);
            ctx.table.set_method_resolution(call_span, def_id);
            return ret;
        }
    }
    let result = match method {
        kw::LEN => ctx.builtins.int,
        kw::LINES | kw::SPLIT | kw::SPLIT_WHITESPACE | kw::SECTIONS => {
            ctx.table.intern(Ty::List(ctx.builtins.string))
        }
        kw::TRIM => ctx.builtins.string,
        kw::CHARS => ctx.table.intern(Ty::List(ctx.builtins.char_ty)),
        _ => {
            ctx.diagnostics
                .push(crate::diagnostics::no_method("String", method, call_span));
            return TypeId::ERROR;
        }
    };
    let string_param = [ctx.builtins.string];
    let params = if method == kw::SPLIT {
        &string_param[..]
    } else {
        &[]
    };
    validate_call_args(params, arg_tys, call_span, ctx);
    result
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

/// Validates argument count and types against expected parameter types.
fn validate_call_args(
    expected_params: &[TypeId],
    arg_tys: &[TypeId],
    call_span: Span,
    ctx: &mut TypeCheckCtx<'_>,
) {
    diagnostics::expect_args(
        expected_params.len(),
        arg_tys.len(),
        call_span,
        ctx.diagnostics,
    );
    for (i, arg_ty) in arg_tys.iter().enumerate() {
        if let Some(&expected) = expected_params.get(i) {
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
    applied_args: &[TypeId],
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

                    validate_call_args(call_params, arg_tys, call_span, ctx);

                    // Store the method resolution for MIR lowering
                    ctx.table.set_method_resolution(call_span, method_def_id);

                    return ret;
                }
            }

            // Method found but no type — shouldn't happen, return error
            return TypeId::ERROR;
        }
    }

    // No method found — check if the type has a callable field with this name.
    let field_ty_raw = ctx.table.get_struct_fields(type_def_id).and_then(|fields| {
        fields
            .iter()
            .find(|(name, _)| *name == method)
            .map(|(_, ty)| *ty)
    });
    if let Some(raw_ty) = field_ty_raw {
        let mut field_ty = strip_mutable(raw_ty, ctx.table);

        // Apply generic substitution for Ty::Applied types.
        if !applied_args.is_empty() {
            let params = ctx
                .table
                .get_generic_params(type_def_id)
                .map(<[_]>::to_vec)
                .unwrap_or_default();
            let subst = crate::substitute::build_subst_map(&params, applied_args);
            field_ty = crate::substitute::substitute_mut(field_ty, &subst, ctx.table);
        }

        if let Ty::Function { params, ret, .. } = ctx.table.resolve(field_ty).clone() {
            validate_call_args(&params, arg_tys, call_span, ctx);

            // Record field resolution for HIR lowering
            if let Some(field_def) =
                crate::check_decl::find_field_def(method, type_def_id, ctx.definitions)
            {
                ctx.table.set_field_resolution(call_span, field_def);
            }
            ctx.table.set_field_call_fn_type(call_span, field_ty);

            return ret;
        }

        // Field exists but is not callable
        let tn = ctx
            .definitions
            .get(type_def_id)
            .map_or("<unknown>", |info| info.name.as_str());
        let ft = type_name(field_ty, ctx.table, ctx.definitions);
        ctx.diagnostics.push(
            Diagnostic::error(format!(
                "field `{method}` on type `{tn}` is not callable (has type `{ft}`)"
            ))
            .with_label(Label::primary(recv_span, "not a function")),
        );
        return TypeId::ERROR;
    }

    // No method or callable field found
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
#[allow(clippy::too_many_lines)]
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
            PipelineStageKind::Map { .. } => "parallel map",
            PipelineStageKind::Reduce { .. } => "parallel reduce",
            PipelineStageKind::Filter { .. } => "parallel filter",
            PipelineStageKind::Find { .. } => "parallel find",
        }
    } else {
        match &stage.kind {
            PipelineStageKind::ForEach { .. } => "each",
            PipelineStageKind::Map { .. } => "map",
            PipelineStageKind::Reduce { .. } => "reduce",
            PipelineStageKind::Filter { .. } => "filter",
            PipelineStageKind::Find { .. } => "find",
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
        PipelineStageKind::Filter { callback } => {
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
            if ret_ty != TypeId::ERROR && ret_ty != ctx.builtins.bool_ty {
                let found = type_name(ret_ty, ctx.table, ctx.definitions);
                ctx.diagnostics.push(
                    Diagnostic::error(format!(
                        "`filter` callback must return `Bool`, found `{found}`"
                    ))
                    .with_label(Label::primary(callback.span, "expected Bool return")),
                );
            }
            ctx.table.intern(Ty::List(elem_ty))
        }
        PipelineStageKind::Find { callback } => {
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
            if ret_ty != TypeId::ERROR && ret_ty != ctx.builtins.bool_ty {
                let found = type_name(ret_ty, ctx.table, ctx.definitions);
                ctx.diagnostics.push(
                    Diagnostic::error(format!(
                        "`find` callback must return `Bool`, found `{found}`"
                    ))
                    .with_label(Label::primary(callback.span, "expected Bool return")),
                );
            }
            ctx.table.intern(Ty::Optional(elem_ty))
        }
    }
}

/// Extracts the element type from a `List[T]` or an `Iterable[E]` struct,
/// reporting an error if neither.
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
        Ty::Struct(def_id) => {
            // Check if the struct binds Iterable[E]
            if let Some(iterable_id) = table.well_known.iterable {
                if table.type_binds_interface(def_id, iterable_id) {
                    // Extract E from the toList method's return type List[E]
                    if let Some(methods) = table.get_type_methods(def_id) {
                        for (name, method_def) in methods {
                            if *name == spark_intern::kw::TO_LIST {
                                if let Some(fn_ty) = table.get_def_type(*method_def) {
                                    if let Ty::Function { ret, .. } = table.resolve(fn_ty) {
                                        if let Ty::List(elem) = table.resolve(*ret) {
                                            return *elem;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let name = type_name(base_ty, table, definitions);
            diagnostics.push(
                Diagnostic::error(format!(
                    "`{operation}` requires a List or Iterable, found `{name}`"
                ))
                .with_label(Label::primary(span, "not a List or Iterable")),
            );
            TypeId::ERROR
        }
        _ => {
            let name = type_name(base_ty, table, definitions);
            diagnostics.push(
                Diagnostic::error(format!(
                    "`{operation}` requires a List or Iterable, found `{name}`"
                ))
                .with_label(Label::primary(span, "not a List or Iterable")),
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

/// Registers the narrowed type for a `TypeNarrowing` pattern binding.
///
/// Returns `Some(narrowed_ty)` if the arm has a `TypeNarrowing` pattern, `None` otherwise.
fn register_narrowing_binding(
    arm: &spark_ast::MatchArm,
    ctx: &mut TypeCheckCtx<'_>,
) -> Option<TypeId> {
    if let spark_ast::PatternKind::TypeNarrowing { name, ty: pat_ty } = &arm.pattern.kind {
        let narrowed_ty = crate::lower::lower_type_expr(pat_ty, ctx);
        if let Some(def_id) = ctx.resolutions.and_then(|res| res.get_binding(name.span)) {
            ctx.table.set_def_type(def_id, narrowed_ty);
        }
        Some(narrowed_ty)
    } else {
        None
    }
}

/// Resolves an identifier name as a union member type.
///
/// When a bare type name is used as a `to` arm pattern (e.g., `Int -> ...`),
/// this function checks if the name matches a built-in type or a user-defined
/// type (struct/enum) that is a member of the scrutinee union.
fn resolve_ident_as_union_member(
    name: Symbol,
    members: &[TypeId],
    ctx: &TypeCheckCtx<'_>,
) -> Option<TypeId> {
    // Check builtins first (Int, String, Bool, etc.)
    if let Some(builtin_ty) = ctx.builtins.lookup(name) {
        if members.contains(&builtin_ty) {
            return Some(builtin_ty);
        }
    }

    // Check user-defined types (structs, enums) by looking up definitions
    // whose name matches the identifier and whose type is a union member.
    for &member in members {
        match ctx.table.resolve(member) {
            Ty::Struct(def_id) | Ty::Enum(def_id) => {
                if let Some(info) = ctx.definitions.get(*def_id) {
                    if info.name == name {
                        return Some(member);
                    }
                }
            }
            _ => {}
        }
    }

    None
}

/// Checks a `to` expression (`expr to { Pattern -> ... }`).
///
/// Behaviour depends on the scrutinee type:
/// - **Union/Optional**: pass-through mode — unhandled members included in result type
/// - **Non-union** (Int, Bool, Enum, etc.): exhaustive mode — all cases must be covered
fn check_to_narrowing(
    inner_ty: TypeId,
    arms: &[spark_ast::MatchArm],
    span: Span,
    ctx: &mut TypeCheckCtx<'_>,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    let is_union = matches!(ctx.table.resolve(inner_ty), Ty::Union(_) | Ty::Optional(_));

    if is_union {
        // === Union/Optional pass-through mode ===
        let members = crate::union::union_members(inner_ty, ctx.table);

        let mut body_types = Vec::with_capacity(arms.len());
        let mut handled_types = Vec::new();

        for arm in arms {
            if let Some(narrowed_ty) = register_narrowing_binding(arm, ctx) {
                handled_types.push(narrowed_ty);
            } else if let spark_ast::PatternKind::Variant { path, .. } = &arm.pattern.kind {
                let variant_ty = ctx
                    .resolutions
                    .and_then(|res| res.get_path(path.span))
                    .and_then(|def_id| ctx.table.get_def_type(def_id))
                    .unwrap_or(TypeId::ERROR);
                if !variant_ty.is_error() {
                    handled_types.push(variant_ty);
                }
            } else if let spark_ast::PatternKind::Ident { name, .. } = &arm.pattern.kind {
                // Bare type name (e.g., `Int -> ...`): resolve as a union member
                // by matching the identifier against builtin type names or
                // user-defined type definitions.
                if let Some(member_ty) = resolve_ident_as_union_member(name.name, &members, ctx) {
                    handled_types.push(member_ty);
                }
            }

            let body_ty = check_expr(&arm.body, ctx, current_fn_ret);
            body_types.push(body_ty);
        }

        let unhandled: Vec<TypeId> = members
            .iter()
            .copied()
            .filter(|m| !handled_types.contains(m))
            .collect();

        let mut result_ty =
            unify_type_list(&body_types, ctx.builtins.void, ctx.table, ctx.builtins);

        // Fold in unhandled member types
        for &u in &unhandled {
            result_ty = unify_branches(result_ty, u, ctx.table, ctx.builtins);
        }
        result_ty
    } else {
        // === Non-union exhaustive mode ===
        let mut arm_types = Vec::with_capacity(arms.len());
        for arm in arms {
            register_narrowing_binding(arm, ctx);
            let body_ty = check_expr(&arm.body, ctx, current_fn_ret);
            arm_types.push(body_ty);
        }

        crate::exhaustive::check_exhaustiveness(inner_ty, arms, span, ctx);

        unify_type_list(&arm_types, ctx.builtins.never, ctx.table, ctx.builtins)
    }
}

/// Unifies a list of types via `unify_branches`, returning `empty_fallback` for empty lists.
fn unify_type_list(
    types: &[TypeId],
    empty_fallback: TypeId,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
) -> TypeId {
    match types.first() {
        None => empty_fallback,
        Some(&first) => types[1..]
            .iter()
            .copied()
            .fold(first, |acc, ty| unify_branches(acc, ty, table, builtins)),
    }
}

/// Returns `true` if the given type is a struct that binds the `Displayable` protocol.
fn is_displayable_type(ty: TypeId, table: &TypeTable) -> bool {
    let Some(displayable_id) = table.well_known.displayable else {
        return false;
    };
    let stripped = strip_mutable(ty, table);
    match table.resolve(stripped) {
        Ty::Struct(def_id) => table.type_binds_interface(*def_id, displayable_id),
        _ => false,
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
    let Ty::List(actual_elem) = table.resolve(first_arg_ty).clone() else {
        return (params, ret);
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
            Diagnostic::error("`to` target must be a function")
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
                "`to {}` expected {} argument{}, found {}",
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
                        "`to {}` first argument type mismatch: expected `{expected_name}`, found `{found_name}`",
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
