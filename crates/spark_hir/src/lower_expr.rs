//! Expression lowering.
//!
//! Converts AST [`Expr`] nodes into HIR [`HirExpr`] nodes, resolving
//! identifiers to [`DefId`]s and looking up types from the [`TypeTable`].

use spark_ast::{Expr, ExprKind, LiteralKind, StringPart, ToHandler};
use spark_diagnostics::Diagnostics;
use spark_resolve::{DefinitionTable, ResolutionMap};
use spark_span::Span;
use spark_types::{TypeId, TypeTable};

use crate::desugar;
use crate::hir::{HirExpr, HirExprKind, HirLiteral, HirMatchArm, HirParam, HirPattern, HirStmt};
use crate::lower_pattern::lower_pattern_with_guard;
use crate::lower_stmt::lower_stmt;
use crate::lower_type_expr;

/// Lowers an expression from the AST to HIR.
#[allow(clippy::too_many_arguments)]
pub fn lower_expr(
    expr: &Expr,
    table: &TypeTable,
    definitions: &mut DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut Diagnostics,
    current_fn_ret: Option<TypeId>,
) -> HirExpr {
    let ty = expr_type(expr.span, table);
    let kind = lower_expr_kind(expr, table, definitions, resolutions, diagnostics, current_fn_ret);
    HirExpr::new(kind, ty, expr.span)
}

/// Lowers a block expression.
pub fn lower_block(
    block: &spark_ast::Block,
    table: &TypeTable,
    definitions: &mut DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut Diagnostics,
    current_fn_ret: Option<TypeId>,
) -> HirExpr {
    let stmts: Vec<HirStmt> = block
        .stmts
        .iter()
        .map(|s| lower_stmt(s, table, definitions, resolutions, diagnostics, current_fn_ret))
        .collect();

    let trailing = block.expr.as_ref().map(|e| {
        Box::new(lower_expr(e, table, definitions, resolutions, diagnostics, current_fn_ret))
    });

    let ty = trailing
        .as_ref()
        .map_or_else(|| expr_type(block.span, table), |e| e.ty);

    HirExpr::new(
        HirExprKind::Block { stmts, expr: trailing },
        ty,
        block.span,
    )
}

/// Inner dispatch on `ExprKind`.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn lower_expr_kind(
    expr: &Expr,
    table: &TypeTable,
    definitions: &mut DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut Diagnostics,
    current_fn_ret: Option<TypeId>,
) -> HirExprKind {
    match &expr.kind {
        // === Literals ===
        ExprKind::Literal(lit) => lower_literal_expr(&lit.kind, expr, table, definitions, resolutions, diagnostics, current_fn_ret),

        // === Identifiers -> Local(DefId) ===
        ExprKind::Ident(ident) => {
            let def_id = resolutions
                .and_then(|res| res.get_ident(ident.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            if def_id.is_dummy() {
                HirExprKind::Error
            } else {
                HirExprKind::Local(def_id)
            }
        }

        ExprKind::Path(path) => {
            let def_id = resolutions
                .and_then(|res| res.get_path(path.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            if def_id.is_dummy() {
                HirExprKind::Error
            } else {
                HirExprKind::Local(def_id)
            }
        }

        // === Operators ===
        ExprKind::Binary { op, lhs, rhs } => HirExprKind::BinaryOp {
            op: *op,
            lhs: Box::new(lower_expr(lhs, table, definitions, resolutions, diagnostics, current_fn_ret)),
            rhs: Box::new(lower_expr(rhs, table, definitions, resolutions, diagnostics, current_fn_ret)),
        },

        ExprKind::Unary { op, operand } => HirExprKind::UnaryOp {
            op: *op,
            operand: Box::new(lower_expr(operand, table, definitions, resolutions, diagnostics, current_fn_ret)),
        },

        // === Access ===
        ExprKind::Field { base, field } => HirExprKind::FieldAccess {
            base: Box::new(lower_expr(base, table, definitions, resolutions, diagnostics, current_fn_ret)),
            field: field.name,
        },

        ExprKind::Index { base, index } => HirExprKind::Index {
            base: Box::new(lower_expr(base, table, definitions, resolutions, diagnostics, current_fn_ret)),
            index: Box::new(lower_expr(index, table, definitions, resolutions, diagnostics, current_fn_ret)),
        },

        // === Calls ===
        ExprKind::Call { callee, args } => HirExprKind::Call {
            callee: Box::new(lower_expr(callee, table, definitions, resolutions, diagnostics, current_fn_ret)),
            args: args.iter().map(|a| lower_expr(&a.value, table, definitions, resolutions, diagnostics, current_fn_ret)).collect(),
        },

        ExprKind::MethodCall { receiver, method, args } => HirExprKind::MethodCall {
            receiver: Box::new(lower_expr(receiver, table, definitions, resolutions, diagnostics, current_fn_ret)),
            method: method.name,
            args: args.iter().map(|a| lower_expr(&a.value, table, definitions, resolutions, diagnostics, current_fn_ret)).collect(),
        },

        // === Constructors ===
        ExprKind::StructInit { path, type_args: _, fields, spread: _ } => {
            let def_id = resolutions
                .and_then(|res| res.get_path(path.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            HirExprKind::StructInit {
                def_id,
                fields: fields
                    .iter()
                    .map(|f| (f.name.name, lower_expr(&f.value, table, definitions, resolutions, diagnostics, current_fn_ret)))
                    .collect(),
            }
        }

        ExprKind::EnumVariant { path, variant, fields } => {
            let def_id = resolutions
                .and_then(|res| res.get_path(path.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            HirExprKind::EnumVariant {
                def_id,
                variant: variant.name,
                fields: fields
                    .iter()
                    .map(|f| lower_expr(f, table, definitions, resolutions, diagnostics, current_fn_ret))
                    .collect(),
            }
        }

        ExprKind::List(elems) => HirExprKind::ListLit(
            elems.iter().map(|e| lower_expr(e, table, definitions, resolutions, diagnostics, current_fn_ret)).collect(),
        ),

        ExprKind::Map(entries) => HirExprKind::MapLit(
            entries
                .iter()
                .map(|e| {
                    let key = lower_expr(&e.key, table, definitions, resolutions, diagnostics, current_fn_ret);
                    let val = lower_expr(&e.value, table, definitions, resolutions, diagnostics, current_fn_ret);
                    (key, val)
                })
                .collect(),
        ),

        ExprKind::Set(elems) => HirExprKind::SetLit(
            elems.iter().map(|e| lower_expr(e, table, definitions, resolutions, diagnostics, current_fn_ret)).collect(),
        ),

        ExprKind::Tuple(elems) => HirExprKind::TupleLit(
            elems.iter().map(|e| lower_expr(e, table, definitions, resolutions, diagnostics, current_fn_ret)).collect(),
        ),

        // === Control flow ===
        ExprKind::Block(block) => {
            let lowered = lower_block(block, table, definitions, resolutions, diagnostics, current_fn_ret);
            lowered.kind
        }

        ExprKind::If { condition, then_branch, else_branch } => HirExprKind::If {
            cond: Box::new(lower_expr(condition, table, definitions, resolutions, diagnostics, current_fn_ret)),
            then_: Box::new(lower_expr(then_branch, table, definitions, resolutions, diagnostics, current_fn_ret)),
            else_: else_branch.as_ref().map(|e| Box::new(lower_expr(e, table, definitions, resolutions, diagnostics, current_fn_ret))),
        },

        ExprKind::Match { scrutinee, arms } => HirExprKind::Match {
            scrutinee: Box::new(lower_expr(scrutinee, table, definitions, resolutions, diagnostics, current_fn_ret)),
            arms: arms
                .iter()
                .map(|arm| {
                    let (pattern, guard) = lower_pattern_with_guard(&arm.pattern, resolutions, diagnostics, table, definitions, current_fn_ret);
                    HirMatchArm {
                        pattern,
                        guard,
                        body: lower_expr(&arm.body, table, definitions, resolutions, diagnostics, current_fn_ret),
                        span: arm.span,
                    }
                })
                .collect(),
        },

        ExprKind::Loop { body } => HirExprKind::Loop {
            body: Box::new(lower_expr(body, table, definitions, resolutions, diagnostics, current_fn_ret)),
        },

        ExprKind::Break(value) => HirExprKind::Break(
            value.as_ref().map(|v| Box::new(lower_expr(v, table, definitions, resolutions, diagnostics, current_fn_ret))),
        ),

        ExprKind::Continue => HirExprKind::Continue,

        ExprKind::Return(value) => HirExprKind::Return(
            value.as_ref().map(|v| Box::new(lower_expr(v, table, definitions, resolutions, diagnostics, current_fn_ret))),
        ),

        // === Closures ===
        ExprKind::Lambda { params, ret_type: _, body } => {
            let hir_params: Vec<HirParam> = params
                .iter()
                .map(|p| {
                    let def_id = resolutions
                        .and_then(|res| res.get_binding(p.name.span))
                        .unwrap_or(spark_resolve::DefId::DUMMY);
                    let ty = table.get_def_type(def_id).unwrap_or(TypeId::ERROR);
                    HirParam { def_id, ty, span: p.span }
                })
                .collect();
            HirExprKind::Lambda {
                params: hir_params,
                body: Box::new(lower_expr(body, table, definitions, resolutions, diagnostics, current_fn_ret)),
            }
        }

        // === Type operations ===
        ExprKind::Is { expr: inner, ty } => HirExprKind::Is {
            expr: Box::new(lower_expr(inner, table, definitions, resolutions, diagnostics, current_fn_ret)),
            target_ty: lower_type_expr(ty, table),
        },

        ExprKind::As { expr: inner, ty } => HirExprKind::As {
            expr: Box::new(lower_expr(inner, table, definitions, resolutions, diagnostics, current_fn_ret)),
            target_ty: lower_type_expr(ty, table),
        },

        // === Async ===
        ExprKind::Await(inner) | ExprKind::BlockAwait(inner) => HirExprKind::Await(
            Box::new(lower_expr(inner, table, definitions, resolutions, diagnostics, current_fn_ret)),
        ),

        ExprKind::Spawn(inner) => HirExprKind::Spawn(
            Box::new(lower_expr(inner, table, definitions, resolutions, diagnostics, current_fn_ret)),
        ),

        // === Desugared: Paren ===
        ExprKind::Paren(inner) => {
            lower_expr_kind(inner, table, definitions, resolutions, diagnostics, current_fn_ret)
        }

        // === Desugared: Coalesce (??) ===
        ExprKind::Coalesce { lhs, rhs } => {
            let lhs_hir = lower_expr(lhs, table, definitions, resolutions, diagnostics, current_fn_ret);
            let rhs_hir = lower_expr(rhs, table, definitions, resolutions, diagnostics, current_fn_ret);
            let ty = expr_type(expr.span, table);
            desugar::desugar_coalesce(lhs_hir, rhs_hir, expr.span, ty, definitions).kind
        }

        // === Desugared: OptionalField (?.) ===
        ExprKind::OptionalField { base, field } => {
            let base_hir = lower_expr(base, table, definitions, resolutions, diagnostics, current_fn_ret);
            let ty = expr_type(expr.span, table);
            let inner_ty = spark_types::union::strip_null(base_hir.ty, table);
            desugar::desugar_optional_field(base_hir, field.name, expr.span, ty, inner_ty, definitions).kind
        }

        // === Desugared: Propagate (!) ===
        ExprKind::Propagate(inner) => {
            let inner_hir = lower_expr(inner, table, definitions, resolutions, diagnostics, current_fn_ret);
            let ty = expr_type(expr.span, table);
            let inner_ty = inner_hir.ty;
            desugar_propagate_expr(inner_hir, inner_ty, ty, current_fn_ret, expr.span, table, definitions)
        }

        // === Deferred constructs -> Error ===
        ExprKind::Each { iterable, body, .. }
        | ExprKind::EachVia { iterable, body, .. } => {
            let _ = lower_expr(iterable, table, definitions, resolutions, diagnostics, current_fn_ret);
            let _ = lower_expr(body, table, definitions, resolutions, diagnostics, current_fn_ret);
            HirExprKind::Error
        }

        ExprKind::To { expr: inner, handler } => {
            let inner_hir = lower_expr(inner, table, definitions, resolutions, diagnostics, current_fn_ret);
            let ty = expr_type(expr.span, table);
            lower_to_expr(inner_hir, handler, ty, table, definitions, resolutions, diagnostics, current_fn_ret, expr.span)
        }

        ExprKind::Partial { callee, args } => {
            let _ = lower_expr(callee, table, definitions, resolutions, diagnostics, current_fn_ret);
            for arg in args {
                let _ = lower_expr(&arg.value, table, definitions, resolutions, diagnostics, current_fn_ret);
            }
            HirExprKind::Error
        }

        ExprKind::Range { start, end, .. } | ExprKind::ListRange { start, end, .. } => {
            let _ = lower_expr(start, table, definitions, resolutions, diagnostics, current_fn_ret);
            let _ = lower_expr(end, table, definitions, resolutions, diagnostics, current_fn_ret);
            HirExprKind::Error
        }

        ExprKind::Error => HirExprKind::Error,
    }
}

/// Desugars a propagate expression (`expr!`) by inspecting the inner type.
fn desugar_propagate_expr(
    inner_hir: HirExpr,
    inner_ty: TypeId,
    result_ty: TypeId,
    current_fn_ret: Option<TypeId>,
    span: Span,
    table: &TypeTable,
    definitions: &mut DefinitionTable,
) -> HirExprKind {
    use spark_types::ty::Ty;

    let resolved = table.resolve(inner_ty);
    match resolved {
        Ty::Optional(success) => {
            let success_ty = *success;
            let null_ty = table.lookup(&spark_types::ty::Ty::Null).unwrap_or(TypeId::ERROR);
            desugar::desugar_propagate_optional(inner_hir, span, success_ty, null_ty, definitions).kind
        }
        Ty::Union(_) => {
            if let Some((success_ty, error_types)) =
                spark_types::union::split_union(inner_ty, Some(result_ty), current_fn_ret, table)
            {
                desugar::desugar_propagate_union(inner_hir, span, success_ty, &error_types, definitions).kind
            } else {
                // Can't split — fallback to simple binding
                let fresh_id = definitions.next_synthetic_id(span);
                HirExprKind::Match {
                    scrutinee: Box::new(inner_hir),
                    arms: vec![HirMatchArm {
                        pattern: HirPattern::Binding { def_id: fresh_id },
                        guard: None,
                        body: HirExpr::new(HirExprKind::Local(fresh_id), result_ty, span),
                        span,
                    }],
                }
            }
        }
        _ => {
            // Not optional or union — just pass through
            inner_hir.kind
        }
    }
}

/// Lowers a `to` expression into a match.
#[allow(clippy::too_many_arguments)]
fn lower_to_expr(
    inner_hir: HirExpr,
    handler: &ToHandler,
    ty: TypeId,
    table: &TypeTable,
    definitions: &mut DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut Diagnostics,
    current_fn_ret: Option<TypeId>,
    span: Span,
) -> HirExprKind {
    match handler {
        ToHandler::Narrowing(arms) => {
            let mut hir_arms: Vec<HirMatchArm> = Vec::with_capacity(arms.len() + 1);

            for arm in arms {
                let pattern = lower_to_pattern(&arm.pattern, definitions, resolutions, table);
                let body = lower_expr(&arm.body, table, definitions, resolutions, diagnostics, current_fn_ret);
                hir_arms.push(HirMatchArm {
                    pattern,
                    guard: None,
                    body,
                    span: arm.span,
                });
            }

            // Add a passthrough arm for unhandled members
            let pass_id = definitions.next_synthetic_id(span);
            hir_arms.push(HirMatchArm {
                pattern: HirPattern::Binding { def_id: pass_id },
                guard: None,
                body: HirExpr::new(HirExprKind::Local(pass_id), ty, span),
                span,
            });

            HirExprKind::Match {
                scrutinee: Box::new(inner_hir),
                arms: hir_arms,
            }
        }
        ToHandler::Pipeline { .. } => {
            // Pipeline handlers are deferred
            HirExprKind::Error
        }
    }
}

/// Lowers a pattern from a `to` expression handler arm.
fn lower_to_pattern(
    pattern: &spark_ast::Pattern,
    definitions: &mut DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    table: &TypeTable,
) -> HirPattern {
    match &pattern.kind {
        spark_ast::PatternKind::TypeNarrowing { name, ty } => {
            let def_id = resolutions
                .and_then(|res| res.get_binding(name.span))
                .unwrap_or_else(|| definitions.next_synthetic_id(pattern.span));
            let narrowed_ty = table.get_expr_type(ty.span).unwrap_or(TypeId::ERROR);
            HirPattern::TypeNarrow { def_id, ty: narrowed_ty }
        }
        spark_ast::PatternKind::Variant { path, fields } => {
            let def_id = resolutions
                .and_then(|res| res.get_path(path.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            HirPattern::Variant {
                def_id,
                fields: fields.iter().map(|f| lower_to_pattern(f, definitions, resolutions, table)).collect(),
            }
        }
        spark_ast::PatternKind::Ident { name, .. } => {
            let def_id = resolutions
                .and_then(|res| res.get_binding(name.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            HirPattern::Binding { def_id }
        }
        spark_ast::PatternKind::Wildcard => HirPattern::Wildcard,
        _ => crate::lower_pattern::lower_pattern(pattern, resolutions),
    }
}

/// Lowers a literal expression, producing either a simple `HirExprKind::Literal`
/// or a `HirExprKind::StringInterp` for interpolated strings.
#[allow(clippy::too_many_arguments)]
fn lower_literal_expr(
    kind: &LiteralKind,
    expr: &Expr,
    table: &TypeTable,
    definitions: &mut DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut Diagnostics,
    current_fn_ret: Option<TypeId>,
) -> HirExprKind {
    match kind {
        LiteralKind::String(s) if !s.is_simple() => {
            // Interpolated string: produce StringInterp with parts.
            let string_ty = expr_type(expr.span, table);
            let parts = s
                .parts
                .iter()
                .filter_map(|part| match part {
                    StringPart::Text(sym) => Some(HirExpr::new(
                        HirExprKind::Literal(HirLiteral::String(*sym)),
                        string_ty,
                        expr.span,
                    )),
                    StringPart::Interpolation(sub_expr) => {
                        let lowered = lower_expr(
                            sub_expr, table, definitions, resolutions, diagnostics, current_fn_ret,
                        );
                        Some(lowered)
                    }
                })
                .collect();
            HirExprKind::StringInterp { parts }
        }
        _ => HirExprKind::Literal(lower_literal(kind)),
    }
}

/// Lowers a literal value.
fn lower_literal(kind: &LiteralKind) -> HirLiteral {
    match kind {
        LiteralKind::Int(v) => HirLiteral::Int(*v),
        LiteralKind::Float(v) => HirLiteral::Float(*v),
        LiteralKind::Bool(v) => HirLiteral::Bool(*v),
        LiteralKind::Char(v) => HirLiteral::Char(*v),
        LiteralKind::Null => HirLiteral::Null,
        LiteralKind::String(s) => {
            if let Some(StringPart::Text(sym)) = s.parts.first() {
                HirLiteral::String(*sym)
            } else {
                HirLiteral::String(spark_intern::sym(""))
            }
        }
    }
}

/// Looks up the type for an expression by its span.
fn expr_type(span: Span, table: &TypeTable) -> TypeId {
    table.get_expr_type(span).unwrap_or(TypeId::ERROR)
}
