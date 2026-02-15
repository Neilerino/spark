//! Expression type checking (Pass 2).
//!
//! Walks expression trees and returns the type of each expression,
//! recording the result in the type table's `expr_types` map.

use spark_ast::{BinaryOp, Block, Expr, ExprKind, LiteralKind, StringPart, ToHandler, UnaryOp};
use spark_diagnostics::{Diagnostic, Label};
use spark_intern::{kw, Symbol};
use spark_resolve::{DefId, DefinitionTable, ResolutionMap};
use spark_span::Span;

use crate::builtins::BuiltinTypes;
use crate::check_stmt::check_stmt;
use crate::lower::lower_type_expr;
use crate::table::TypeTable;
use crate::ty::{Ty, TypeId};

/// Type-checks an expression and returns its type.
///
/// Records the resulting `TypeId` in `table.expr_types` keyed by the
/// expression's span.
#[allow(clippy::too_many_arguments)]
pub fn check_expr(
    expr: &Expr,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    check_expr_expected(expr, table, builtins, definitions, resolutions, diagnostics, current_fn_ret, None)
}

/// Type-checks an expression with an expected type hint from context.
///
/// The `expected_ty` is used by propagation (`!`) to determine which union
/// members are the "success" type vs error types to propagate.
#[allow(clippy::too_many_arguments)]
pub fn check_expr_expected(
    expr: &Expr,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
    current_fn_ret: Option<TypeId>,
    expected_ty: Option<TypeId>,
) -> TypeId {
    let ty = check_expr_inner(expr, table, builtins, definitions, resolutions, diagnostics, current_fn_ret, expected_ty);
    table.set_expr_type(expr.span, ty);
    ty
}

/// Inner expression type checker — dispatches on `ExprKind`.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn check_expr_inner(
    expr: &Expr,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
    current_fn_ret: Option<TypeId>,
    expected_ty: Option<TypeId>,
) -> TypeId {
    match &expr.kind {
        // === Literals ===
        ExprKind::Literal(lit) => match &lit.kind {
            LiteralKind::Int(_) => builtins.int,
            LiteralKind::Float(_) => builtins.float,
            LiteralKind::String(s) => {
                // Type-check each interpolated sub-expression.
                for part in &s.parts {
                    if let StringPart::Interpolation(sub_expr) = part {
                        let sub_ty = check_expr(
                            sub_expr, table, builtins, definitions,
                            resolutions, diagnostics, current_fn_ret,
                        );
                        // Verify the type supports string conversion.
                        let resolved = table.resolve(sub_ty);
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
                            let name = type_name(sub_ty, table, definitions);
                            diagnostics.push(
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
                builtins.string
            }
            LiteralKind::Char(_) => builtins.char_ty,
            LiteralKind::Bool(_) => builtins.bool_ty,
            LiteralKind::Null => builtins.null,
        },

        // === Identifiers ===
        ExprKind::Ident(ident) => {
            if let Some(res) = resolutions {
                if let Some(def_id) = res.get_ident(ident.span) {
                    return table.get_def_type(def_id).unwrap_or(TypeId::ERROR);
                }
            }
            // Unresolved identifier — the resolver should have caught this,
            // so just return error silently.
            TypeId::ERROR
        }

        ExprKind::Path(path) => {
            if let Some(res) = resolutions {
                if let Some(def_id) = res.get_path(path.span) {
                    return table.get_def_type(def_id).unwrap_or(TypeId::ERROR);
                }
            }
            TypeId::ERROR
        }

        // === Binary operators ===
        ExprKind::Binary { op, lhs, rhs } => {
            let lhs_ty = check_expr(lhs, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let rhs_ty = check_expr(rhs, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);

            check_binary_op(*op, lhs_ty, rhs_ty, expr.span, lhs.span, rhs.span, table, builtins, diagnostics)
        }

        // === Unary operators ===
        ExprKind::Unary { op, operand } => {
            let operand_ty = check_expr(operand, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            check_unary_op(*op, operand_ty, expr.span, operand.span, table, builtins, diagnostics)
        }

        // === Function call ===
        ExprKind::Call { callee, args } => {
            let callee_ty = check_expr(callee, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            check_call(callee_ty, args, expr.span, callee.span, table, builtins, definitions, resolutions, diagnostics, current_fn_ret)
        }

        // === Field access ===
        ExprKind::Field { base, field } => {
            let base_ty = check_expr(base, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            check_field_access(base_ty, field.name, field.span, table, definitions, diagnostics)
        }

        // === Optional field access ===
        ExprKind::OptionalField { base, field } => {
            let base_ty = check_expr(base, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let inner_ty = unwrap_optional(base_ty, table);
            let field_ty = check_field_access(inner_ty, field.name, field.span, table, definitions, diagnostics);
            if field_ty.is_error() {
                return TypeId::ERROR;
            }
            table.intern(Ty::Optional(field_ty))
        }

        // === Index access ===
        ExprKind::Index { base, index } => {
            let base_ty = check_expr(base, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let _index_ty = check_expr(index, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);

            match table.resolve(strip_mutable(base_ty, table)).clone() {
                Ty::List(elem) => table.intern(Ty::Optional(elem)),
                Ty::Map(_, value) => table.intern(Ty::Optional(value)),
                Ty::Tuple(elems) => {
                    // For now, return the first element type or Error
                    elems.first().copied().unwrap_or(TypeId::ERROR)
                }
                Ty::Error => TypeId::ERROR,
                _ => {
                    diagnostics.push(
                        Diagnostic::error("type is not indexable")
                            .with_label(Label::primary(base.span, "cannot index this type")),
                    );
                    TypeId::ERROR
                }
            }
        }

        // === Struct initialization ===
        ExprKind::StructInit { path, type_args, fields, spread } => {
            check_struct_init(path, type_args, fields, spread.as_deref(), expr.span, table, builtins, definitions, resolutions, diagnostics, current_fn_ret)
        }

        // === Enum variant ===
        ExprKind::EnumVariant { path, variant, fields } => {
            check_enum_variant(path, variant, fields, expr.span, table, builtins, definitions, resolutions, diagnostics, current_fn_ret)
        }

        // === Block ===
        ExprKind::Block(block) => {
            check_block(block, table, builtins, definitions, resolutions, diagnostics, current_fn_ret)
        }

        // === If expression ===
        ExprKind::If { condition, then_branch, else_branch } => {
            let cond_ty = check_expr(condition, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);

            if !is_compatible(cond_ty, builtins.bool_ty, table) {
                diagnostics.push(
                    Diagnostic::error("if condition must be Bool")
                        .with_label(Label::primary(condition.span, "expected Bool")),
                );
            }

            let then_ty = check_expr(then_branch, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);

            else_branch.as_ref().map_or(builtins.void, |else_branch| {
                let else_ty = check_expr(else_branch, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
                unify_branches(then_ty, else_ty, table, builtins)
            })
        }

        // === Match expression ===
        ExprKind::Match { scrutinee, arms } => {
            let scrutinee_ty = check_expr(scrutinee, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);

            let mut arm_types = Vec::with_capacity(arms.len());
            for arm in arms {
                // Pattern checking is simplified — we just check the body
                let body_ty = check_expr(&arm.body, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
                arm_types.push(body_ty);
            }

            // Check exhaustiveness
            crate::exhaustive::check_exhaustiveness(
                scrutinee_ty, arms, expr.span,
                table, builtins, definitions, resolutions, diagnostics,
            );

            if arm_types.is_empty() {
                builtins.never
            } else {
                let mut result = arm_types[0];
                for &ty in &arm_types[1..] {
                    result = unify_branches(result, ty, table, builtins);
                }
                result
            }
        }

        // === Loop ===
        ExprKind::Loop { body } => {
            let _ = check_expr(body, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            // Loop's type comes from break expressions — for now, Void
            builtins.void
        }

        // === Break ===
        ExprKind::Break(value) => {
            if let Some(val) = value {
                let _ = check_expr(val, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            }
            builtins.never
        }

        // === Continue ===
        ExprKind::Continue => builtins.never,

        // === Return ===
        ExprKind::Return(value) => {
            let val_ty = value.as_ref().map_or(builtins.void, |val| {
                check_expr(val, table, builtins, definitions, resolutions, diagnostics, current_fn_ret)
            });

            if let Some(expected) = current_fn_ret {
                if !is_compatible(val_ty, expected, table) {
                    let expected_name = type_name(expected, table, definitions);
                    let found_name = type_name(val_ty, table, definitions);
                    diagnostics.push(
                        Diagnostic::error(format!("return type mismatch: expected `{expected_name}`, found `{found_name}`"))
                            .with_label(Label::primary(expr.span, format!("expected `{expected_name}`"))),
                    );
                }
            }

            builtins.never
        }

        // === Lambda ===
        ExprKind::Lambda { params, ret_type, body } => {
            let param_types: Vec<TypeId> = params
                .iter()
                .map(|p| lower_type_expr(&p.ty, table, builtins, definitions, resolutions, diagnostics))
                .collect();

            // Register param types
            for (i, param) in params.iter().enumerate() {
                if let Some(res) = resolutions {
                    if let Some(def_id) = res.get_binding(param.name.span) {
                        table.set_def_type(def_id, param_types[i]);
                    }
                }
            }

            let declared_ret = ret_type
                .as_ref()
                .map(|rt| lower_type_expr(rt, table, builtins, definitions, resolutions, diagnostics));

            let body_ty = check_expr(body, table, builtins, definitions, resolutions, diagnostics, declared_ret);
            let ret = declared_ret.unwrap_or(body_ty);

            table.intern(Ty::Function {
                params: param_types,
                ret,
                is_async: false,
            })
        }

        // === Coalesce (??) ===
        ExprKind::Coalesce { lhs, rhs } => {
            let lhs_ty = check_expr(lhs, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let rhs_ty = check_expr(rhs, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);

            let inner = unwrap_optional(lhs_ty, table);
            if inner == lhs_ty && !lhs_ty.is_error() {
                diagnostics.push(
                    Diagnostic::error("left side of `??` must be an optional type")
                        .with_label(Label::primary(lhs.span, "not optional")),
                );
            }

            // The result is the non-optional inner type unified with rhs
            if inner.is_error() { rhs_ty } else { unify_branches(inner, rhs_ty, table, builtins) }
        }

        // === Propagate (!) ===
        ExprKind::Propagate(inner_expr) => {
            let inner_ty = check_expr(inner_expr, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);

            // For Optional(T) → success type is T
            if let Ty::Optional(success) = table.resolve(inner_ty) {
                return *success;
            }

            // For Union types → use split_union to determine success type
            if matches!(table.resolve(inner_ty), Ty::Union(_)) {
                // Try to split using the expected type (from context) or fn return type
                if let Some((success_ty, _errors)) =
                    crate::union::split_union(inner_ty, expected_ty, current_fn_ret, table)
                {
                    return success_ty;
                }
            }

            // Fallback: unwrap nullable
            unwrap_optional(inner_ty, table)
        }

        // === Is ===
        ExprKind::Is { expr: inner_expr, ty } => {
            let _ = check_expr(inner_expr, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let _ = lower_type_expr(ty, table, builtins, definitions, resolutions, diagnostics);
            builtins.bool_ty
        }

        // === As ===
        ExprKind::As { expr: inner_expr, ty } => {
            let _ = check_expr(inner_expr, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            lower_type_expr(ty, table, builtins, definitions, resolutions, diagnostics)
        }

        // === Collections ===
        ExprKind::List(elems) => {
            if elems.is_empty() {
                // Empty list — element type unknown, use Error for now
                table.intern(Ty::List(TypeId::ERROR))
            } else {
                let first_ty = check_expr(&elems[0], table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
                for elem in &elems[1..] {
                    let _ = check_expr(elem, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
                }
                table.intern(Ty::List(first_ty))
            }
        }

        ExprKind::Map(entries) => {
            if entries.is_empty() {
                table.intern(Ty::Map(TypeId::ERROR, TypeId::ERROR))
            } else {
                let key_ty = check_expr(&entries[0].key, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
                let val_ty = check_expr(&entries[0].value, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
                for entry in &entries[1..] {
                    let _ = check_expr(&entry.key, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
                    let _ = check_expr(&entry.value, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
                }
                table.intern(Ty::Map(key_ty, val_ty))
            }
        }

        ExprKind::Set(elems) => {
            if elems.is_empty() {
                table.intern(Ty::Set(TypeId::ERROR))
            } else {
                let first_ty = check_expr(&elems[0], table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
                for elem in &elems[1..] {
                    let _ = check_expr(elem, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
                }
                table.intern(Ty::Set(first_ty))
            }
        }

        ExprKind::Tuple(elems) => {
            let elem_types: Vec<TypeId> = elems
                .iter()
                .map(|e| check_expr(e, table, builtins, definitions, resolutions, diagnostics, current_fn_ret))
                .collect();
            table.intern(Ty::Tuple(elem_types))
        }

        // === Paren ===
        ExprKind::Paren(inner) => {
            check_expr(inner, table, builtins, definitions, resolutions, diagnostics, current_fn_ret)
        }

        // === Await ===
        ExprKind::Await(inner) | ExprKind::BlockAwait(inner) => {
            let inner_ty = check_expr(inner, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            match table.resolve(inner_ty) {
                Ty::Future { inner } => *inner,
                _ if inner_ty.is_error() => TypeId::ERROR,
                _ => {
                    diagnostics.push(
                        Diagnostic::error("cannot `await` a non-future type")
                            .with_label(Label::primary(expr.span, "not a future")),
                    );
                    TypeId::ERROR
                }
            }
        }

        // === Spawn ===
        ExprKind::Spawn(inner) => {
            let inner_ty = check_expr(inner, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            match table.resolve(inner_ty) {
                Ty::Future { .. } => inner_ty,
                _ if inner_ty.is_error() => TypeId::ERROR,
                _ => {
                    diagnostics.push(
                        Diagnostic::error("cannot `spawn` a non-async expression")
                            .with_label(Label::primary(inner.span, "not a future")),
                    );
                    TypeId::ERROR
                }
            }
        }

        // === Method calls on collections and strings ===
        ExprKind::MethodCall { receiver, method, args } => {
            let recv_ty = check_expr(receiver, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let arg_tys: Vec<TypeId> = args
                .iter()
                .map(|a| check_expr(&a.value, table, builtins, definitions, resolutions, diagnostics, current_fn_ret))
                .collect();

            check_method_call(recv_ty, method.name, &arg_tys, receiver.span, expr.span, table, builtins, definitions, diagnostics)
        }

        ExprKind::To { expr: inner, handler } => {
            let inner_ty = check_expr(inner, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            check_to_expr(inner_ty, handler, expr.span, table, builtins, definitions, resolutions, diagnostics, current_fn_ret)
        }

        ExprKind::Each { iterable, callback } => {
            let iter_ty = check_expr(iterable, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let base_ty = strip_mutable(iter_ty, table);
            let elem_ty = match table.resolve(base_ty).clone() {
                Ty::List(elem) => elem,
                Ty::Error => TypeId::ERROR,
                _ => {
                    let name = type_name(base_ty, table, definitions);
                    diagnostics.push(
                        Diagnostic::error(format!("`each` requires a List, found `{name}`"))
                            .with_label(Label::primary(iterable.span, "not a List")),
                    );
                    TypeId::ERROR
                }
            };

            let cb_ty = check_expr(callback, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            validate_each_callback(cb_ty, elem_ty, false, callback.span, table, definitions, diagnostics);
            builtins.void
        }

        ExprKind::EachVia { iterable, callback } => {
            let iter_ty = check_expr(iterable, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let base_ty = strip_mutable(iter_ty, table);
            let elem_ty = match table.resolve(base_ty).clone() {
                Ty::List(elem) => elem,
                Ty::Error => TypeId::ERROR,
                _ => {
                    let name = type_name(base_ty, table, definitions);
                    diagnostics.push(
                        Diagnostic::error(format!("`each via` requires a List, found `{name}`"))
                            .with_label(Label::primary(iterable.span, "not a List")),
                    );
                    TypeId::ERROR
                }
            };

            let cb_ty = check_expr(callback, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let ret_ty = validate_each_callback(cb_ty, elem_ty, true, callback.span, table, definitions, diagnostics);
            table.intern(Ty::List(ret_ty))
        }

        ExprKind::ParallelEach { iterable, callback } => {
            let iter_ty = check_expr(iterable, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let base_ty = strip_mutable(iter_ty, table);
            let elem_ty = match table.resolve(base_ty).clone() {
                Ty::List(elem) => elem,
                Ty::Error => TypeId::ERROR,
                _ => {
                    let name = type_name(base_ty, table, definitions);
                    diagnostics.push(
                        Diagnostic::error(format!("`parallel each` requires a List, found `{name}`"))
                            .with_label(Label::primary(iterable.span, "not a List")),
                    );
                    TypeId::ERROR
                }
            };

            let cb_ty = check_expr(callback, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            validate_each_callback(cb_ty, elem_ty, false, callback.span, table, definitions, diagnostics);
            builtins.void
        }

        ExprKind::ParallelEachVia { iterable, callback } => {
            let iter_ty = check_expr(iterable, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let base_ty = strip_mutable(iter_ty, table);
            let elem_ty = match table.resolve(base_ty).clone() {
                Ty::List(elem) => elem,
                Ty::Error => TypeId::ERROR,
                _ => {
                    let name = type_name(base_ty, table, definitions);
                    diagnostics.push(
                        Diagnostic::error(format!("`parallel each via` requires a List, found `{name}`"))
                            .with_label(Label::primary(iterable.span, "not a List")),
                    );
                    TypeId::ERROR
                }
            };

            let cb_ty = check_expr(callback, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let ret_ty = validate_each_callback(cb_ty, elem_ty, true, callback.span, table, definitions, diagnostics);
            table.intern(Ty::List(ret_ty))
        }

        ExprKind::Range { start, end, .. } => {
            let _ = check_expr(start, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let _ = check_expr(end, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            // Bare range type — not yet supported
            TypeId::ERROR
        }

        ExprKind::ListRange { start, end, step, .. } => {
            let start_ty = check_expr(start, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            let end_ty = check_expr(end, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            if let Some(s) = step {
                let step_ty = check_expr(s, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
                if step_ty != builtins.int && !step_ty.is_error() {
                    diagnostics.push(
                        Diagnostic::error("list range step must be `Int`")
                            .with_label(Label::primary(s.span, "expected `Int`")),
                    );
                }
            }
            if start_ty != builtins.int && !start_ty.is_error() {
                diagnostics.push(
                    Diagnostic::error("list range start must be `Int`")
                        .with_label(Label::primary(start.span, "expected `Int`")),
                );
            }
            if end_ty != builtins.int && !end_ty.is_error() {
                diagnostics.push(
                    Diagnostic::error("list range end must be `Int`")
                        .with_label(Label::primary(end.span, "expected `Int`")),
                );
            }
            table.intern(Ty::List(builtins.int))
        }

        ExprKind::Partial { callee, args } => {
            let _ = check_expr(callee, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            for arg in args {
                let _ = check_expr(&arg.value, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            }
            TypeId::ERROR
        }

        ExprKind::Error => TypeId::ERROR,
    }
}

/// Type-checks a block expression.
pub fn check_block(
    block: &Block,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    for stmt in &block.stmts {
        check_stmt(stmt, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
    }

    block.expr.as_ref().map_or(builtins.void, |expr| {
        check_expr(expr, table, builtins, definitions, resolutions, diagnostics, current_fn_ret)
    })
}

// === Binary operator checking ===

#[allow(clippy::too_many_arguments)]
fn check_binary_op(
    op: BinaryOp,
    lhs_ty: TypeId,
    rhs_ty: TypeId,
    op_span: Span,
    lhs_span: Span,
    rhs_span: Span,
    table: &TypeTable,
    builtins: &BuiltinTypes,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    // Error types propagate silently
    if lhs_ty.is_error() || rhs_ty.is_error() {
        return TypeId::ERROR;
    }

    if op.is_arithmetic() {
        // Strip Mutable/Ref wrappers — `count + 1` where count is Int! or Ref[Int] should work
        let lhs_base = strip_ref(strip_mutable(lhs_ty, table), table);
        let rhs_base = strip_ref(strip_mutable(rhs_ty, table), table);
        let lhs_resolved = table.resolve(lhs_base);
        if !lhs_resolved.is_numeric() {
            diagnostics.push(
                Diagnostic::error(format!("operator `{}` requires numeric operands", op.as_str()))
                    .with_label(Label::primary(lhs_span, "not numeric")),
            );
            return TypeId::ERROR;
        }
        if !is_compatible(lhs_base, rhs_base, table) {
            let l = type_name(lhs_base, table, &DefinitionTable::new());
            let r = type_name(rhs_base, table, &DefinitionTable::new());
            diagnostics.push(
                Diagnostic::error(format!("mismatched types for `{}`: `{l}` and `{r}`", op.as_str()))
                    .with_label(Label::primary(lhs_span, format!("`{l}`")))
                    .with_label(Label::secondary(rhs_span, format!("`{r}`"))),
            );
            return TypeId::ERROR;
        }
        // Arithmetic produces a new value, not a mutable binding
        return lhs_base;
    }

    if op.is_comparison() {
        let lhs_base = strip_ref(strip_mutable(lhs_ty, table), table);
        let rhs_base = strip_ref(strip_mutable(rhs_ty, table), table);
        if !is_compatible(lhs_base, rhs_base, table) {
            let l = type_name(lhs_base, table, &DefinitionTable::new());
            let r = type_name(rhs_base, table, &DefinitionTable::new());
            diagnostics.push(
                Diagnostic::error(format!("cannot compare `{l}` with `{r}`"))
                    .with_label(Label::primary(op_span, "type mismatch")),
            );
        }
        return builtins.bool_ty;
    }

    if op.is_logical() {
        if !is_compatible(lhs_ty, builtins.bool_ty, table) {
            diagnostics.push(
                Diagnostic::error(format!("operator `{}` requires Bool operands", op.as_str()))
                    .with_label(Label::primary(lhs_span, "expected Bool")),
            );
        }
        if !is_compatible(rhs_ty, builtins.bool_ty, table) {
            diagnostics.push(
                Diagnostic::error(format!("operator `{}` requires Bool operands", op.as_str()))
                    .with_label(Label::primary(rhs_span, "expected Bool")),
            );
        }
        return builtins.bool_ty;
    }

    if op.is_bitwise() {
        let lhs_base = strip_ref(strip_mutable(lhs_ty, table), table);
        let rhs_base = strip_ref(strip_mutable(rhs_ty, table), table);
        let lhs_resolved = table.resolve(lhs_base);
        if !lhs_resolved.is_integer() {
            diagnostics.push(
                Diagnostic::error(format!("operator `{}` requires integer operands", op.as_str()))
                    .with_label(Label::primary(lhs_span, "not integer")),
            );
            return TypeId::ERROR;
        }
        if !is_compatible(lhs_base, rhs_base, table) {
            diagnostics.push(
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
#[allow(clippy::too_many_arguments)]
fn check_call(
    callee_ty: TypeId,
    args: &[spark_ast::CallArg],
    call_span: Span,
    callee_span: Span,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    if callee_ty.is_error() {
        // Still check args to report errors in them
        for arg in args {
            let _ = check_expr(&arg.value, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
        }
        return TypeId::ERROR;
    }

    let callee_resolved = table.resolve(callee_ty).clone();
    match callee_resolved {
        Ty::Function { params, ret, is_async } => {
            // Check if this is a generic function by finding its DefId.
            let callee_def_id = find_callee_def_id(callee_span, resolutions);
            let generic_params = callee_def_id
                .and_then(|did| table.get_generic_params(did).map(<[_]>::to_vec));

            if let Some(gparams) = generic_params.filter(|p| !p.is_empty()) {
                // Generic function — type-check args first, then infer type params.
                let arg_types: Vec<TypeId> = args.iter()
                    .map(|a| check_expr(&a.value, table, builtins, definitions, resolutions, diagnostics, current_fn_ret))
                    .collect();

                if args.len() != params.len() {
                    diagnostics.push(
                        Diagnostic::error(format!(
                            "expected {} argument{}, found {}",
                            params.len(),
                            if params.len() == 1 { "" } else { "s" },
                            args.len()
                        ))
                        .with_label(Label::primary(call_span, "wrong number of arguments")),
                    );
                    return TypeId::ERROR;
                }

                if let Some(subst) = crate::substitute::infer_type_args(&gparams, &params, &arg_types, table) {
                    // Re-check args against substituted param types
                    for (i, arg) in args.iter().enumerate() {
                        if let Some(&declared) = params.get(i) {
                            let expected = crate::substitute::substitute_mut(declared, &subst, table);
                            if !is_compatible(arg_types[i], expected, table) {
                                let expected_name = type_name(expected, table, definitions);
                                let found_name = type_name(arg_types[i], table, definitions);
                                diagnostics.push(
                                    Diagnostic::error(format!(
                                        "argument type mismatch: expected `{expected_name}`, found `{found_name}`"
                                    ))
                                    .with_label(Label::primary(arg.value.span, format!("expected `{expected_name}`"))),
                                );
                            }
                        }
                    }

                    // Record this mono instance for MIR monomorphization.
                    if let Some(def_id) = callee_def_id {
                        table.set_mono_instance(call_span, def_id, subst.clone());
                    }

                    // Return the substituted return type
                    let substituted_ret = crate::substitute::substitute_mut(ret, &subst, table);
                    return if is_async {
                        table.intern(Ty::Future { inner: substituted_ret })
                    } else {
                        substituted_ret
                    };
                }

                // Inference failed — report error
                diagnostics.push(
                    Diagnostic::error("cannot infer type arguments for generic function")
                        .with_label(Label::primary(call_span, "type inference failed")),
                );
                return TypeId::ERROR;
            }

            // Non-generic function call
            if args.len() != params.len() {
                diagnostics.push(
                    Diagnostic::error(format!(
                        "expected {} argument{}, found {}",
                        params.len(),
                        if params.len() == 1 { "" } else { "s" },
                        args.len()
                    ))
                    .with_label(Label::primary(call_span, "wrong number of arguments")),
                );
            }

            for (i, arg) in args.iter().enumerate() {
                let arg_ty = check_expr(&arg.value, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
                if let Some(&expected) = params.get(i) {
                    if !is_compatible(arg_ty, expected, table) {
                        let expected_name = type_name(expected, table, definitions);
                        let found_name = type_name(arg_ty, table, definitions);
                        diagnostics.push(
                            Diagnostic::error(format!(
                                "argument type mismatch: expected `{expected_name}`, found `{found_name}`"
                            ))
                            .with_label(Label::primary(arg.value.span, format!("expected `{expected_name}`"))),
                        );
                    }
                }
            }

            if is_async {
                table.intern(Ty::Future { inner: ret })
            } else {
                ret
            }
        }
        // Struct construction via call syntax is handled by StructInit
        Ty::Struct(_) | Ty::Enum(_) => {
            for arg in args {
                let _ = check_expr(&arg.value, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            }
            callee_ty
        }
        _ => {
            diagnostics.push(
                Diagnostic::error("expression is not callable")
                    .with_label(Label::primary(callee_span, "not a function")),
            );
            for arg in args {
                let _ = check_expr(&arg.value, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            }
            TypeId::ERROR
        }
    }
}

/// Tries to find the `DefId` for a callee expression (for generic function lookup).
fn find_callee_def_id(
    callee_span: Span,
    resolutions: Option<&ResolutionMap>,
) -> Option<DefId> {
    let res = resolutions?;
    // Try both path and ident resolution (callee could be either)
    res.get_path(callee_span).or_else(|| res.get_ident(callee_span))
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

    let resolved = table.resolve(strip_ref(strip_mutable(base_ty, table), table)).clone();
    match resolved {
        Ty::Struct(def_id) => {
            if let Some(fields) = table.get_struct_fields(def_id) {
                for &(name, ty) in fields {
                    if name == field_name {
                        if let Some(field_def) = crate::check_decl::find_field_def(field_name, def_id, definitions) {
                            table.set_field_resolution(field_span, field_def);
                        }
                        return ty;
                    }
                }
            }
            let type_name = definitions
                .get(def_id)
                .map_or("<unknown>", |info| info.name.as_str());
            diagnostics.push(
                Diagnostic::error(format!("no field `{field_name}` on type `{type_name}`"))
                    .with_label(Label::primary(field_span, "unknown field")),
            );
            TypeId::ERROR
        }
        Ty::Applied { base, args } => {
            if let Some(fields) = table.get_struct_fields(base) {
                let fields = fields.to_vec();
                let params = table.get_generic_params(base).map(<[_]>::to_vec).unwrap_or_default();
                let subst = crate::substitute::build_subst_map(&params, &args);
                for (name, ty) in fields {
                    if name == field_name {
                        if let Some(field_def) = crate::check_decl::find_field_def(field_name, base, definitions) {
                            table.set_field_resolution(field_span, field_def);
                        }
                        return crate::substitute::substitute_mut(ty, &subst, table);
                    }
                }
            }
            let type_name = definitions
                .get(base)
                .map_or("<unknown>", |info| info.name.as_str());
            diagnostics.push(
                Diagnostic::error(format!("no field `{field_name}` on type `{type_name}`"))
                    .with_label(Label::primary(field_span, "unknown field")),
            );
            TypeId::ERROR
        }
        Ty::Tuple(elems) => {
            // Tuple field access via numeric name (e.g., .0, .1)
            // For now, just return Error
            let _ = elems;
            TypeId::ERROR
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
#[allow(clippy::too_many_arguments)]
fn check_struct_init(
    path: &spark_ast::Path,
    type_args: &[spark_ast::TypeExpr],
    fields: &[spark_ast::FieldInit],
    spread: Option<&Expr>,
    init_span: Span,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    // Resolve the struct path
    let def_id = resolutions
        .and_then(|res| res.get_path(path.span))
        .unwrap_or(DefId::DUMMY);

    if def_id.is_dummy() {
        // Struct not resolved — check field values anyway
        for field in fields {
            let _ = check_expr(&field.value, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
        }
        if let Some(spread) = spread {
            let _ = check_expr(spread, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
        }
        return TypeId::ERROR;
    }

    let struct_type = table.get_def_type(def_id).unwrap_or(TypeId::ERROR);

    // Check if this is a generic struct — if so, infer type args from field values.
    let generic_params = table.get_generic_params(def_id).map(<[_]>::to_vec);
    let is_generic = generic_params.as_ref().is_some_and(|p| !p.is_empty());

    // Check each field and collect value types for inference.
    if let Some(struct_fields) = table.get_struct_fields(def_id) {
        let struct_fields = struct_fields.to_vec();
        let mut field_val_types: Vec<(Symbol, TypeId)> = Vec::new();

        for field in fields {
            let val_ty = check_expr(&field.value, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
            if let Some((_, expected_ty)) = struct_fields.iter().find(|(name, _)| *name == field.name.name) {
                field_val_types.push((field.name.name, val_ty));

                // For generic structs, defer compatibility check until after inference.
                if !is_generic && !is_compatible(val_ty, *expected_ty, table) {
                    let expected_name = type_name(*expected_ty, table, definitions);
                    let found_name = type_name(val_ty, table, definitions);
                    diagnostics.push(
                        Diagnostic::error(format!(
                            "field `{}` type mismatch: expected `{expected_name}`, found `{found_name}`",
                            field.name.name
                        ))
                        .with_label(Label::primary(field.value.span, format!("expected `{expected_name}`"))),
                    );
                }
            } else {
                diagnostics.push(
                    Diagnostic::error(format!("unknown field `{}`", field.name.name))
                        .with_label(Label::primary(field.name.span, "not a field of this struct")),
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
                    diagnostics.push(
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
                        .map(|ta| lower_type_expr(ta, table, builtins, definitions, resolutions, diagnostics))
                        .collect();
                    Some(crate::substitute::build_subst_map(params, &lowered))
                }
            } else {
                // Infer type args from field values.
                let mut declared_types = Vec::new();
                let mut actual_types = Vec::new();
                for (field_name, val_ty) in &field_val_types {
                    if let Some((_, field_ty)) = struct_fields.iter().find(|(n, _)| n == field_name) {
                        declared_types.push(*field_ty);
                        actual_types.push(*val_ty);
                    }
                }
                crate::substitute::infer_type_args(params, &declared_types, &actual_types, table)
            };

            if let Some(subst) = subst {
                // Re-check fields with substituted types
                for (field_name, val_ty) in &field_val_types {
                    if let Some((_, field_ty)) = struct_fields.iter().find(|(n, _)| n == field_name) {
                        let substituted = crate::substitute::substitute_mut(*field_ty, &subst, table);
                        if !is_compatible(*val_ty, substituted, table) {
                            let expected_name = type_name(substituted, table, definitions);
                            let found_name = type_name(*val_ty, table, definitions);
                            diagnostics.push(
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
                    let _ = check_expr(spread, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
                }
                return table.intern(Ty::Applied { base: def_id, args });
            }
            // Inference/explicit args failed — fall through and return the plain struct type
        }
    }

    if let Some(spread) = spread {
        let _ = check_expr(spread, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
    }

    struct_type
}

/// Checks enum variant construction.
#[allow(clippy::too_many_arguments)]
fn check_enum_variant(
    path: &spark_ast::Path,
    _variant: &spark_ast::Ident,
    fields: &[Expr],
    _init_span: Span,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    let def_id = resolutions
        .and_then(|res| res.get_path(path.span))
        .unwrap_or(DefId::DUMMY);

    // Check field values regardless
    for field in fields {
        let _ = check_expr(field, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
    }

    if def_id.is_dummy() {
        return TypeId::ERROR;
    }

    table.get_def_type(def_id).unwrap_or(TypeId::ERROR)
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
            return a1.iter().zip(a2.iter()).all(|(a, e)| is_compatible(*a, *e, table));
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
fn strip_mutable(ty: TypeId, table: &TypeTable) -> TypeId {
    if let Ty::Mutable(inner) = table.resolve(ty) {
        *inner
    } else {
        ty
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
            if param_ty != elem_ty
                && param_ty != TypeId::ERROR
                && elem_ty != TypeId::ERROR
            {
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

/// Strips the `Ref` wrapper if present.
fn strip_ref(ty: TypeId, table: &TypeTable) -> TypeId {
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

/// Returns a human-readable name for a type.
#[must_use]
pub fn type_name(ty: TypeId, table: &TypeTable, definitions: &DefinitionTable) -> String {
    match table.resolve(ty) {
        Ty::Int(w) => w.to_string(),
        Ty::Float(w) => w.to_string(),
        Ty::Bool => "Bool".to_string(),
        Ty::Char => "Char".to_string(),
        Ty::String => "String".to_string(),
        Ty::Byte => "Byte".to_string(),
        Ty::Void => "Void".to_string(),
        Ty::Null => "Null".to_string(),
        Ty::Never => "Never".to_string(),
        Ty::List(elem) => format!("List[{}]", type_name(*elem, table, definitions)),
        Ty::Map(k, v) => format!(
            "Map[{}, {}]",
            type_name(*k, table, definitions),
            type_name(*v, table, definitions)
        ),
        Ty::Set(elem) => format!("Set[{}]", type_name(*elem, table, definitions)),
        Ty::Tuple(elems) => {
            let parts: Vec<_> = elems.iter().map(|e| type_name(*e, table, definitions)).collect();
            format!("Tup[{}]", parts.join(", "))
        }
        Ty::Struct(def_id) => definitions
            .get(*def_id)
            .map_or_else(|| "<struct>".to_string(), |info| info.name.to_string()),
        Ty::Enum(def_id) => definitions
            .get(*def_id)
            .map_or_else(|| "<enum>".to_string(), |info| info.name.to_string()),
        Ty::Union(members) => {
            let parts: Vec<_> = members.iter().map(|m| type_name(*m, table, definitions)).collect();
            parts.join(" | ")
        }
        Ty::Optional(inner) => format!("{}?", type_name(*inner, table, definitions)),
        Ty::Mutable(inner) => format!("{}!", type_name(*inner, table, definitions)),
        Ty::Ref { inner, mutable, .. } => {
            if *mutable {
                format!("Ref[{}]!", type_name(*inner, table, definitions))
            } else {
                format!("Ref[{}]", type_name(*inner, table, definitions))
            }
        }
        Ty::Function { params, ret, is_async } => {
            let params_str: Vec<_> = params.iter().map(|p| type_name(*p, table, definitions)).collect();
            let async_prefix = if *is_async { "async " } else { "" };
            format!("{}({}): {}", async_prefix, params_str.join(", "), type_name(*ret, table, definitions))
        }
        Ty::Param(def_id) => definitions
            .get(*def_id)
            .map_or_else(|| "<param>".to_string(), |info| info.name.to_string()),
        Ty::Applied { base, args } => {
            let base_name = definitions
                .get(*base)
                .map_or_else(|| "<generic>".to_string(), |info| info.name.to_string());
            let arg_names: Vec<_> = args.iter().map(|a| type_name(*a, table, definitions)).collect();
            format!("{}[{}]", base_name, arg_names.join(", "))
        }
        Ty::Future { inner } => format!("Future[{}]", type_name(*inner, table, definitions)),
        Ty::AsyncState { .. } => "<async-state>".to_string(),
        Ty::Error => "<error>".to_string(),
    }
}

/// Type-checks a method call on a collection, string, or user-defined type receiver.
#[allow(clippy::too_many_arguments)]
fn check_method_call(
    recv_ty: TypeId,
    method: Symbol,
    arg_tys: &[TypeId],
    recv_span: Span,
    call_span: Span,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    if recv_ty.is_error() {
        return TypeId::ERROR;
    }

    let (is_mutable, stripped) = match table.resolve(recv_ty).clone() {
        Ty::Mutable(inner) => (true, table.resolve(inner).clone()),
        other => (false, other),
    };

    // Auto-deref through Ref
    let stripped = match &stripped {
        Ty::Ref { inner, .. } => table.resolve(*inner).clone(),
        _ => stripped,
    };

    match stripped {
        Ty::List(elem) => check_list_method(method, elem, is_mutable, arg_tys, call_span, recv_span, table, builtins, diagnostics),
        Ty::Map(k, v) => check_map_method(method, k, v, is_mutable, arg_tys, call_span, recv_span, table, builtins, diagnostics),
        Ty::Set(elem) => check_set_method(method, elem, is_mutable, arg_tys, call_span, recv_span, table, builtins, diagnostics),
        Ty::String => check_string_method(method, arg_tys, call_span, builtins, diagnostics),
        Ty::Struct(def_id) | Ty::Enum(def_id) => {
            check_user_method_call(def_id, method, arg_tys, recv_span, call_span, table, definitions, diagnostics)
        }
        Ty::Applied { base, .. } => {
            check_user_method_call(base, method, arg_tys, recv_span, call_span, table, definitions, diagnostics)
        }
        _ => {
            diagnostics.push(
                Diagnostic::error(format!("type has no method `{method}`"))
                    .with_label(Label::primary(recv_span, "no methods available on this type")),
            );
            TypeId::ERROR
        }
    }
}

/// Checks a method call on `List[T]`.
#[allow(clippy::too_many_arguments)]
fn check_list_method(
    method: Symbol,
    elem: TypeId,
    is_mutable: bool,
    arg_tys: &[TypeId],
    call_span: Span,
    recv_span: Span,
    table: &TypeTable,
    builtins: &BuiltinTypes,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    if method == kw::LEN {
        expect_args(0, arg_tys.len(), call_span, diagnostics);
        builtins.int
    } else if method == kw::APPEND {
        expect_args(1, arg_tys.len(), call_span, diagnostics);
        require_mutable("List", is_mutable, recv_span, diagnostics);
        if !arg_tys.is_empty() && !is_compatible(arg_tys[0], elem, table) {
            diagnostics.push(
                Diagnostic::error("argument type does not match list element type")
                    .with_label(Label::primary(call_span, "type mismatch")),
            );
        }
        builtins.void
    } else if method == kw::CONTAINS {
        expect_args(1, arg_tys.len(), call_span, diagnostics);
        builtins.bool_ty
    } else if method == kw::REMOVE {
        expect_args(1, arg_tys.len(), call_span, diagnostics);
        require_mutable("List", is_mutable, recv_span, diagnostics);
        builtins.void
    } else {
        diagnostics.push(
            Diagnostic::error(format!("List has no method `{method}`"))
                .with_label(Label::primary(call_span, "unknown method")),
        );
        TypeId::ERROR
    }
}

/// Checks a method call on `Map[K, V]`.
#[allow(clippy::too_many_arguments)]
fn check_map_method(
    method: Symbol,
    _key: TypeId,
    _value: TypeId,
    is_mutable: bool,
    arg_tys: &[TypeId],
    call_span: Span,
    recv_span: Span,
    _table: &TypeTable,
    builtins: &BuiltinTypes,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    if method == kw::LEN {
        expect_args(0, arg_tys.len(), call_span, diagnostics);
        builtins.int
    } else if method == kw::CONTAINS {
        expect_args(1, arg_tys.len(), call_span, diagnostics);
        builtins.bool_ty
    } else if method == kw::REMOVE {
        expect_args(1, arg_tys.len(), call_span, diagnostics);
        require_mutable("Map", is_mutable, recv_span, diagnostics);
        builtins.void
    } else {
        diagnostics.push(
            Diagnostic::error(format!("Map has no method `{method}`"))
                .with_label(Label::primary(call_span, "unknown method")),
        );
        TypeId::ERROR
    }
}

/// Checks a method call on `Set[T]`.
#[allow(clippy::too_many_arguments)]
fn check_set_method(
    method: Symbol,
    _elem: TypeId,
    is_mutable: bool,
    arg_tys: &[TypeId],
    call_span: Span,
    recv_span: Span,
    _table: &TypeTable,
    builtins: &BuiltinTypes,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    if method == kw::LEN {
        expect_args(0, arg_tys.len(), call_span, diagnostics);
        builtins.int
    } else if method == kw::ADD {
        expect_args(1, arg_tys.len(), call_span, diagnostics);
        require_mutable("Set", is_mutable, recv_span, diagnostics);
        builtins.void
    } else if method == kw::CONTAINS {
        expect_args(1, arg_tys.len(), call_span, diagnostics);
        builtins.bool_ty
    } else if method == kw::REMOVE {
        expect_args(1, arg_tys.len(), call_span, diagnostics);
        require_mutable("Set", is_mutable, recv_span, diagnostics);
        builtins.void
    } else {
        diagnostics.push(
            Diagnostic::error(format!("Set has no method `{method}`"))
                .with_label(Label::primary(call_span, "unknown method")),
        );
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
        expect_args(0, arg_tys.len(), call_span, diagnostics);
        builtins.int
    } else {
        diagnostics.push(
            Diagnostic::error(format!("String has no method `{method}`"))
                .with_label(Label::primary(call_span, "unknown method")),
        );
        TypeId::ERROR
    }
}

/// Checks a method call on a user-defined type (struct or enum with bindings).
///
/// Looks up the method in `TypeTable::type_methods`, validates arguments,
/// stores the resolved method `DefId` in `method_resolutions`, and returns
/// the method's return type.
#[allow(clippy::too_many_arguments)]
fn check_user_method_call(
    type_def_id: DefId,
    method: Symbol,
    arg_tys: &[TypeId],
    recv_span: Span,
    call_span: Span,
    table: &mut TypeTable,
    definitions: &DefinitionTable,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) -> TypeId {
    // Look up available methods for this type
    let methods = table.get_type_methods(type_def_id).map(<[(spark_intern::Symbol, DefId)]>::to_vec);

    if let Some(methods) = methods {
        // Search for the method by name
        if let Some((_, method_def_id)) = methods.iter().find(|(name, _)| *name == method) {
            let method_def_id = *method_def_id;

            // Get the method's function type
            if let Some(fn_type_id) = table.get_def_type(method_def_id) {
                let fn_ty = table.resolve(fn_type_id).clone();
                if let Ty::Function { params, ret, .. } = fn_ty {
                    // The first parameter is the receiver (e.g., `self: Foo`),
                    // remaining parameters are the call arguments.
                    let call_params = if params.is_empty() {
                        &[][..]
                    } else {
                        &params[1..]
                    };

                    // Check argument count (excluding receiver)
                    if arg_tys.len() != call_params.len() {
                        diagnostics.push(
                            Diagnostic::error(format!(
                                "expected {} argument{}, found {}",
                                call_params.len(),
                                if call_params.len() == 1 { "" } else { "s" },
                                arg_tys.len()
                            ))
                            .with_label(Label::primary(call_span, "wrong number of arguments")),
                        );
                    }

                    // Check argument types
                    for (i, arg_ty) in arg_tys.iter().enumerate() {
                        if let Some(&expected) = call_params.get(i) {
                            if !is_compatible(*arg_ty, expected, table) {
                                let expected_name = type_name(expected, table, definitions);
                                let found_name = type_name(*arg_ty, table, definitions);
                                diagnostics.push(
                                    Diagnostic::error(format!(
                                        "argument type mismatch: expected `{expected_name}`, found `{found_name}`"
                                    ))
                                    .with_label(Label::primary(call_span, format!("expected `{expected_name}`"))),
                                );
                            }
                        }
                    }

                    // Store the method resolution for MIR lowering
                    table.set_method_resolution(call_span, method_def_id);

                    return ret;
                }
            }

            // Method found but no type — shouldn't happen, return error
            return TypeId::ERROR;
        }
    }

    // No method found
    let type_name_str = definitions
        .get(type_def_id)
        .map_or("<unknown>", |info| info.name.as_str());
    diagnostics.push(
        Diagnostic::error(format!("type `{type_name_str}` has no method `{method}`"))
            .with_label(Label::primary(recv_span, "no such method")),
    );
    TypeId::ERROR
}

/// Reports an error if expected != actual argument count.
fn expect_args(
    expected: usize,
    actual: usize,
    span: Span,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    if actual != expected {
        diagnostics.push(
            Diagnostic::error(format!(
                "expected {expected} argument{}, found {actual}",
                if expected == 1 { "" } else { "s" }
            ))
            .with_label(Label::primary(span, "wrong number of arguments")),
        );
    }
}

/// Type-checks a `to` expression.
///
/// For narrowing handlers (`to { NotFound -> ..., DbError(e) -> ... }`):
/// - The inner expression must be a union type
/// - Each handler arm handles one or more union members
/// - Unhandled members pass through
/// - Result is the unified handler body types + unhandled member types
#[allow(clippy::too_many_arguments)]
fn check_to_expr(
    inner_ty: TypeId,
    handler: &ToHandler,
    span: Span,
    table: &mut TypeTable,
    builtins: &BuiltinTypes,
    definitions: &DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut spark_diagnostics::Diagnostics,
    current_fn_ret: Option<TypeId>,
) -> TypeId {
    match handler {
        ToHandler::Narrowing(arms) => {
            let members = crate::union::union_members(inner_ty, table);

            if members.len() <= 1 && !matches!(table.resolve(inner_ty), Ty::Union(_) | Ty::Optional(_)) {
                diagnostics.push(
                    Diagnostic::error("'to' expression requires a union type")
                        .with_label(Label::primary(span, "not a union type")),
                );
            }

            let mut body_types = Vec::with_capacity(arms.len());
            let mut handled_types = Vec::new();

            for arm in arms {
                // Check the pattern — for type narrowing patterns, resolve the type
                if let spark_ast::PatternKind::TypeNarrowing { ty: pat_ty, .. } = &arm.pattern.kind {
                    let narrowed_ty = crate::lower::lower_type_expr(
                        pat_ty, table, builtins, definitions, resolutions, diagnostics,
                    );
                    handled_types.push(narrowed_ty);
                } else if let spark_ast::PatternKind::Variant { path, .. } = &arm.pattern.kind {
                    // Enum variant pattern — resolve it
                    let variant_ty = resolutions
                        .and_then(|res| res.get_path(path.span))
                        .and_then(|def_id| table.get_def_type(def_id))
                        .unwrap_or(TypeId::ERROR);
                    if !variant_ty.is_error() {
                        handled_types.push(variant_ty);
                    }
                }

                let body_ty = check_expr(&arm.body, table, builtins, definitions, resolutions, diagnostics, current_fn_ret);
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
                builtins.void
            };
            for &bty in &body_types[1..] {
                result_ty = unify_branches(result_ty, bty, table, builtins);
            }

            // If there are unhandled types, the result includes them
            if unhandled.is_empty() {
                result_ty
            } else {
                // Result is union of handler body types + unhandled types
                for &u in &unhandled {
                    result_ty = unify_branches(result_ty, u, table, builtins);
                }
                result_ty
            }
        }
        ToHandler::Pipeline { operation, args } => {
            // Generic function-call validation: `expr via func{a: v}` → `func(expr, v)`
            let func_def_id = resolutions.and_then(|r| r.get_ident(operation.span));
            let Some(def_id) = func_def_id else {
                // Unresolved — still check arg values for errors
                for arg in args {
                    let _ = check_expr(
                        &arg.value, table, builtins, definitions,
                        resolutions, diagnostics, current_fn_ret,
                    );
                }
                return TypeId::ERROR;
            };
            let Some(func_ty_id) = table.get_def_type(def_id) else {
                return TypeId::ERROR;
            };
            let func_ty = table.resolve(func_ty_id).clone();
            let Ty::Function { params, ret, .. } = func_ty else {
                diagnostics.push(
                    Diagnostic::error("`via` target must be a function")
                        .with_label(Label::primary(operation.span, "not a function")),
                );
                return TypeId::ERROR;
            };

            // Build full arg list: [inner_ty, partial_arg_types...]
            let mut all_arg_types = vec![inner_ty];
            for arg in args {
                all_arg_types.push(check_expr(
                    &arg.value, table, builtins, definitions,
                    resolutions, diagnostics, current_fn_ret,
                ));
            }

            // Arity check
            if all_arg_types.len() != params.len() {
                diagnostics.push(
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
            for (i, (&arg_ty, &param_ty)) in
                all_arg_types.iter().zip(params.iter()).enumerate()
            {
                if !is_compatible(arg_ty, param_ty, table) {
                    let expected_name = type_name(param_ty, table, definitions);
                    let found_name = type_name(arg_ty, table, definitions);
                    if i == 0 {
                        diagnostics.push(
                            Diagnostic::error(format!(
                                "`via {}` first argument type mismatch: expected `{expected_name}`, found `{found_name}`",
                                operation.name
                            ))
                            .with_label(Label::primary(span, format!("expected `{expected_name}`"))),
                        );
                    } else {
                        let arg_span = args[i - 1].value.span;
                        diagnostics.push(
                            Diagnostic::error(format!(
                                "argument type mismatch: expected `{expected_name}`, found `{found_name}`"
                            ))
                            .with_label(Label::primary(arg_span, format!("expected `{expected_name}`"))),
                        );
                    }
                }
            }

            ret
        }
    }
}

/// Reports an error when calling a mutating method on an immutable receiver.
fn require_mutable(
    type_name: &str,
    is_mutable: bool,
    recv_span: Span,
    diagnostics: &mut spark_diagnostics::Diagnostics,
) {
    if !is_mutable {
        diagnostics.push(
            Diagnostic::error(format!("cannot mutate immutable {type_name}"))
                .with_label(Label::primary(recv_span, "value is not mutable")),
        );
    }
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
    fn type_name_primitives() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();

        assert_eq!(type_name(builtins.int, &table, &defs), "Int64");
        assert_eq!(type_name(builtins.bool_ty, &table, &defs), "Bool");
        assert_eq!(type_name(builtins.string, &table, &defs), "String");
        assert_eq!(type_name(builtins.void, &table, &defs), "Void");
    }

    #[test]
    fn type_name_optional() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let defs = DefinitionTable::new();
        let opt_int = table.intern(Ty::Optional(builtins.int));
        assert_eq!(type_name(opt_int, &table, &defs), "Int64?");
    }

    #[test]
    fn unify_same() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        assert_eq!(unify_branches(builtins.int, builtins.int, &mut table, &builtins), builtins.int);
    }

    #[test]
    fn unify_with_never() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        assert_eq!(unify_branches(builtins.never, builtins.int, &mut table, &builtins), builtins.int);
        assert_eq!(unify_branches(builtins.int, builtins.never, &mut table, &builtins), builtins.int);
    }

    #[test]
    fn unify_different_creates_union() {
        let mut table = TypeTable::new();
        let builtins = register_builtins(&mut table);
        let result = unify_branches(builtins.int, builtins.string, &mut table, &builtins);
        assert!(matches!(table.resolve(result), Ty::Union(_)));
    }
}
