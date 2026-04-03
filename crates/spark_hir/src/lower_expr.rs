//! Expression lowering.
//!
//! Converts AST [`Expr`] nodes into HIR [`HirExpr`] nodes, resolving
//! identifiers to [`DefId`]s and looking up types from the [`TypeTable`].

use spark_ast::{BinaryOp, Expr, ExprKind, LiteralKind, PipelineStageKind, StringPart};
use spark_diagnostics::Diagnostics;
use spark_intern::kw;
use spark_resolve::{DefinitionKind, DefinitionTable, ResolutionMap};
use spark_span::Span;
use spark_types::builtins::BuiltinTypes;
use spark_types::ty::Ty;
use spark_types::{TypeId, TypeTable};

use crate::desugar;
use crate::hir::{HirExpr, HirExprKind, HirLiteral, HirMatchArm, HirParam, HirPattern, HirStmt};
use crate::lower_pattern::lower_pattern_with_guard;
use crate::lower_stmt::lower_stmt;
use crate::lower_type_expr;

/// Shared lowering context threaded through all expression/statement lowering.
///
/// Bundles the read-only type tables and mutable state (definitions, diagnostics)
/// that every lowering function needs, reducing repetitive parameter passing.
pub(crate) struct LowerCtx<'a> {
    /// The type table for resolving type IDs.
    pub table: &'a TypeTable,
    /// The definition table for generating synthetic IDs.
    pub definitions: &'a mut DefinitionTable,
    /// Name resolution results (None during desugaring phases).
    pub resolutions: Option<&'a ResolutionMap>,
    /// Built-in type mappings for name-to-type lookups.
    pub builtins: &'a BuiltinTypes,
    /// Diagnostic accumulator.
    pub diagnostics: &'a mut Diagnostics,
    /// Return type of the enclosing function (for propagation desugaring).
    pub current_fn_ret: Option<TypeId>,
}

/// Lowers an expression from the AST to HIR.
pub(crate) fn lower_expr(
    expr: &Expr,
    ctx: &mut LowerCtx<'_>,
) -> HirExpr {
    let ty = expr_type(expr.id, ctx.table);
    let kind = lower_expr_kind(
        expr,
        ctx,
    );
    HirExpr::new(kind, ty, expr.span)
}

/// Lowers a block expression.
pub(crate) fn lower_block(
    block: &spark_ast::Block,
    ctx: &mut LowerCtx<'_>,
) -> HirExpr {
    let stmts: Vec<HirStmt> = block
        .stmts
        .iter()
        .map(|s| {
            lower_stmt(s, ctx)
        })
        .collect();

    let trailing = block.expr.as_ref().map(|e| {
        Box::new(lower_expr(e, ctx))
    });

    let ty = trailing
        .as_ref()
        .map_or_else(|| expr_type(block.id, ctx.table), |e| e.ty);

    HirExpr::new(
        HirExprKind::Block {
            stmts,
            expr: trailing,
        },
        ty,
        block.span,
    )
}

/// Inner dispatch on `ExprKind`.
#[allow(clippy::too_many_lines)]
fn lower_expr_kind(
    expr: &Expr,
    ctx: &mut LowerCtx<'_>,
) -> HirExprKind {
    match &expr.kind {
        // === Literals ===
        ExprKind::Literal(lit) => lower_literal_expr(
            &lit.kind,
            expr,
            ctx,
        ),

        // === Identifiers -> Local(DefId) ===
        ExprKind::Ident(ident) => {
            let def_id = ctx.resolutions
                .and_then(|res| res.get_ident(ident.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            if def_id.is_dummy() {
                HirExprKind::Error
            } else {
                HirExprKind::Local(def_id)
            }
        }

        ExprKind::Path(path) => {
            let def_id = ctx.resolutions
                .and_then(|res| res.get_path(path.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            if def_id.is_dummy() {
                HirExprKind::Error
            } else {
                HirExprKind::Local(def_id)
            }
        }

        // === Operators ===
        ExprKind::Binary { op, lhs, rhs } => {
            let lhs_hir = auto_deref_ref(
                lower_expr(lhs, ctx),
                ctx.table,
            );
            let rhs_hir = auto_deref_ref(
                lower_expr(rhs, ctx),
                ctx.table,
            );
            // Equatable: desugar `a == b` / `a != b` to method calls
            if matches!(op, BinaryOp::Eq | BinaryOp::Ne)
                && should_desugar_equatable(&lhs_hir, ctx.table)
            {
                return desugar_equatable(*op, lhs_hir, rhs_hir, ctx.table);
            }
            // Comparable: desugar `a < b` etc. to compareTo() calls
            if matches!(
                op,
                BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
            ) && should_desugar_comparable(&lhs_hir, ctx.table)
            {
                return desugar_comparable(*op, lhs_hir, rhs_hir, ctx.table);
            }
            HirExprKind::BinaryOp {
                op: *op,
                lhs: Box::new(lhs_hir),
                rhs: Box::new(rhs_hir),
            }
        }

        ExprKind::Unary { op, operand } => {
            let operand_hir = auto_deref_ref(
                lower_expr(operand, ctx),
                ctx.table,
            );
            HirExprKind::UnaryOp {
                op: *op,
                operand: Box::new(operand_hir),
            }
        }

        // === Access ===
        ExprKind::Field { base, field } => {
            let mut base_hir = lower_expr(base, ctx);
            // Auto-deref through Ref (and Mutable(Ref))
            let base_ty = strip_mutable_ty(base_hir.ty, ctx.table);
            if let Ty::Ref { inner, .. } = ctx.table.resolve(base_ty) {
                let inner_ty = *inner;
                base_hir =
                    HirExpr::new(HirExprKind::Deref(Box::new(base_hir)), inner_ty, base.span);
            }
            HirExprKind::FieldAccess {
                base: Box::new(base_hir),
                field: field.name,
            }
        }

        ExprKind::Index { base, index } => HirExprKind::Index {
            base: Box::new(lower_expr(base, ctx)),
            index: Box::new(lower_expr(index, ctx)),
        },

        // === Calls ===
        ExprKind::Call { callee, args } => {
            let callee_hir = lower_expr(callee, ctx);
            let param_types = extract_param_types(callee_hir.ty, ctx.table);
            let is_print = is_print_call(callee, ctx.resolutions, ctx.definitions);
            let lowered_args: Vec<HirExpr> = args
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let mut arg = lower_expr(&a.value, ctx);
                    if let Some(&param_ty) = param_types.get(i) {
                        arg = coerce_ref(arg, param_ty, a.value.span, ctx.table);
                    }
                    // Displayable: wrap struct args in print() with toString()
                    if is_print {
                        arg = maybe_wrap_displayable(arg, ctx.table);
                    }
                    arg
                })
                .collect();
            HirExprKind::Call {
                callee: Box::new(callee_hir),
                args: lowered_args,
            }
        }

        ExprKind::MethodCall {
            receiver,
            method,
            args,
        } => {
            let mut recv_hir = lower_expr(receiver, ctx);
            // Auto-deref through Ref (and Mutable(Ref))
            let recv_ty = strip_mutable_ty(recv_hir.ty, ctx.table);
            if let Ty::Ref { inner, .. } = ctx.table.resolve(recv_ty) {
                let inner_ty = *inner;
                recv_hir = HirExpr::new(
                    HirExprKind::Deref(Box::new(recv_hir)),
                    inner_ty,
                    receiver.span,
                );
            }

            // Field call: the "method" is actually a struct field with a function type.
            if let Some(fn_ty) = ctx.table.get_field_call_fn_type(expr.span) {
                let callee = HirExpr::new(
                    HirExprKind::FieldAccess {
                        base: Box::new(recv_hir),
                        field: method.name,
                    },
                    fn_ty,
                    method.span,
                );

                // Get param types for ref coercion
                let param_types: Vec<TypeId> = match ctx.table.resolve(fn_ty) {
                    Ty::Function { params, .. } => params.clone(),
                    _ => vec![],
                };

                let lowered_args = args
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let mut arg = lower_expr(&a.value, ctx);
                        if let Some(&param_ty) = param_types.get(i) {
                            arg = coerce_ref(arg, param_ty, a.value.span, ctx.table);
                        }
                        arg
                    })
                    .collect();

                HirExprKind::Call {
                    callee: Box::new(callee),
                    args: lowered_args,
                }
            } else {
                HirExprKind::MethodCall {
                    receiver: Box::new(recv_hir),
                    method: method.name,
                    args: args
                        .iter()
                        .map(|a| {
                            lower_expr(&a.value, ctx)
                        })
                        .collect(),
                }
            }
        }

        // === Constructors ===
        ExprKind::StructInit {
            path,
            type_args: _,
            fields,
            spread: _,
        } => {
            let def_id = ctx.resolutions
                .and_then(|res| res.get_path(path.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            HirExprKind::StructInit {
                def_id,
                fields: fields
                    .iter()
                    .map(|f| {
                        (
                            f.name.name,
                            lower_expr(&f.value, ctx),
                        )
                    })
                    .collect(),
            }
        }

        ExprKind::EnumVariant {
            path,
            variant,
            fields,
        } => {
            let def_id = ctx.resolutions
                .and_then(|res| res.get_path(path.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            HirExprKind::EnumVariant {
                def_id,
                variant: variant.name,
                fields: fields
                    .iter()
                    .map(|f| {
                        lower_expr(f, ctx)
                    })
                    .collect(),
            }
        }

        ExprKind::List(elems) => HirExprKind::ListLit(
            elems
                .iter()
                .map(|e| {
                    lower_expr(e, ctx)
                })
                .collect(),
        ),

        ExprKind::Map(entries) => HirExprKind::MapLit(
            entries
                .iter()
                .map(|e| {
                    let key = lower_expr(&e.key, ctx);
                    let val = lower_expr(&e.value, ctx);
                    (key, val)
                })
                .collect(),
        ),

        ExprKind::Set(elems) => HirExprKind::SetLit(
            elems
                .iter()
                .map(|e| {
                    lower_expr(e, ctx)
                })
                .collect(),
        ),

        ExprKind::Tuple(elems) => HirExprKind::TupleLit(
            elems
                .iter()
                .map(|e| {
                    lower_expr(e, ctx)
                })
                .collect(),
        ),

        // === Control flow ===
        ExprKind::Block(block) => {
            let lowered = lower_block(block, ctx);
            lowered.kind
        }

        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => HirExprKind::If {
            cond: Box::new(lower_expr(condition, ctx)),
            then_: Box::new(lower_expr(then_branch, ctx)),
            else_: else_branch.as_ref().map(|e| {
                Box::new(lower_expr(e, ctx))
            }),
        },

        ExprKind::Loop { body } => HirExprKind::Loop {
            body: Box::new(lower_expr(body, ctx)),
        },

        ExprKind::Break(value) => HirExprKind::Break(value.as_ref().map(|v| {
            Box::new(lower_expr(v, ctx))
        })),

        ExprKind::Continue => HirExprKind::Continue,

        ExprKind::Return(value) => HirExprKind::Return(value.as_ref().map(|v| {
            let mut lowered = lower_expr(v, ctx);
            if let Some(ret_ty) = ctx.current_fn_ret {
                let ret_stripped = strip_mutable_ty(ret_ty, ctx.table);
                // Auto-borrow: if function returns Ref[T] and value is T,
                // wrap in Borrow so the borrow checker can detect dangling refs.
                if let Ty::Ref {
                    mutable: ref_mut, ..
                } = ctx.table.resolve(ret_stripped)
                {
                    let val_stripped = strip_mutable_ty(lowered.ty, ctx.table);
                    if !matches!(ctx.table.resolve(val_stripped), Ty::Ref { .. })
                        && !lowered.ty.is_error()
                    {
                        lowered = HirExpr::new(
                            HirExprKind::Borrow {
                                operand: Box::new(lowered),
                                mutable: *ref_mut,
                            },
                            ret_ty,
                            v.span,
                        );
                    }
                }
                // Auto-deref: if function returns T and value is Ref[T],
                // wrap in Deref.
                if !matches!(ctx.table.resolve(ret_stripped), Ty::Ref { .. }) {
                    let val_stripped = strip_mutable_ty(lowered.ty, ctx.table);
                    if let Ty::Ref { inner, .. } = ctx.table.resolve(val_stripped) {
                        let inner_ty = *inner;
                        lowered =
                            HirExpr::new(HirExprKind::Deref(Box::new(lowered)), inner_ty, v.span);
                    }
                }
            }
            Box::new(lowered)
        })),

        // === Closures ===
        ExprKind::Lambda {
            params,
            ret_type: _,
            body,
        } => {
            let hir_params: Vec<HirParam> = params
                .iter()
                .map(|p| {
                    let def_id = ctx.resolutions
                        .and_then(|res| res.get_binding(p.name.span))
                        .unwrap_or(spark_resolve::DefId::DUMMY);
                    let ty = ctx.table.get_def_type(def_id).unwrap_or(TypeId::ERROR);
                    HirParam {
                        def_id,
                        ty,
                        span: p.span,
                    }
                })
                .collect();
            HirExprKind::Lambda {
                params: hir_params,
                body: Box::new(lower_expr(body, ctx)),
            }
        }

        // === Type operations ===
        ExprKind::Is { expr: inner, ty } => HirExprKind::Is {
            expr: Box::new(lower_expr(inner, ctx)),
            target_ty: lower_type_expr(ty, ctx.table),
        },

        ExprKind::As { expr: inner, ty } => HirExprKind::As {
            expr: Box::new(lower_expr(inner, ctx)),
            target_ty: lower_type_expr(ty, ctx.table),
        },

        // === Async ===
        ExprKind::Await(inner) | ExprKind::BlockAwait(inner) => {
            HirExprKind::Await(Box::new(lower_expr(inner, ctx)))
        }

        ExprKind::Spawn(inner) => HirExprKind::Spawn(Box::new(lower_expr(inner, ctx))),

        // === Desugared: Paren ===
        ExprKind::Paren(inner) => lower_expr_kind(inner, ctx),

        // === Desugared: Coalesce (??) ===
        ExprKind::Coalesce { lhs, rhs } => {
            let lhs_hir = lower_expr(lhs, ctx);
            let rhs_hir = lower_expr(rhs, ctx);
            let ty = expr_type(expr.id, ctx.table);
            desugar::desugar_coalesce(lhs_hir, rhs_hir, expr.span, ty, ctx.definitions).kind
        }

        // === Desugared: OptionalField (?.) ===
        ExprKind::OptionalField { base, field } => {
            let base_hir = lower_expr(base, ctx);
            let ty = expr_type(expr.id, ctx.table);
            let inner_ty = spark_types::union::strip_null(base_hir.ty, ctx.table);
            desugar::desugar_optional_field(
                base_hir,
                field.name,
                expr.span,
                ty,
                inner_ty,
                ctx.definitions,
            )
            .kind
        }

        // === Desugared: Propagate (!) ===
        ExprKind::Propagate(inner) => {
            let inner_hir = lower_expr(inner, ctx);
            let ty = expr_type(expr.id, ctx.table);
            let inner_ty = inner_hir.ty;
            desugar_propagate_expr(
                inner_hir,
                inner_ty,
                ty,
                ctx.current_fn_ret,
                expr.span,
                ctx.table,
                ctx.definitions,
            )
        }

        // === Unified Pipeline ===
        ExprKind::Pipeline { source, stages } => lower_pipeline(
            source, stages, expr, ctx,
        ),

        ExprKind::To { expr: inner, arms } => {
            let inner_ty = expr_type(inner.id, ctx.table);
            let is_union = matches!(ctx.table.resolve(inner_ty), Ty::Union(_) | Ty::Optional(_));
            let inner_hir = lower_expr(inner, ctx);

            if is_union {
                // Union/optional: use narrowing lowering (adds passthrough arms)
                let ty = expr_type(expr.id, ctx.table);
                lower_to_narrowing(inner_hir, arms, ty, ctx, expr.span)
            } else {
                // Non-union: lower directly to HirExprKind::Match (exhaustive, no passthrough)
                HirExprKind::Match {
                    scrutinee: Box::new(inner_hir),
                    arms: arms
                        .iter()
                        .map(|arm| {
                            let (pattern, guard) = lower_pattern_with_guard(
                                &arm.pattern, ctx,
                            );
                            HirMatchArm {
                                pattern,
                                guard,
                                body: lower_expr(&arm.body, ctx),
                                span: arm.span,
                            }
                        })
                        .collect(),
                }
            }
        }

        ExprKind::PipelineApply {
            expr: inner,
            operation,
            args,
        } => {
            let inner_hir = lower_expr(inner, ctx);
            lower_pipeline_apply(inner_hir, operation, args, ctx, expr.span)
        }

        ExprKind::Partial { callee, args } => lower_standalone_partial(
            callee, args, expr, ctx,
        ),

        ExprKind::Range { start, end, .. } => {
            let _ = lower_expr(start, ctx);
            let _ = lower_expr(end, ctx);
            HirExprKind::Error
        }

        ExprKind::ListRange {
            start,
            end,
            inclusive,
            step,
        } => {
            let start_hir = lower_expr(start, ctx);
            let end_hir = lower_expr(end, ctx);
            let step_hir = step.as_ref().map(|s| {
                Box::new(lower_expr(s, ctx))
            });
            HirExprKind::ListRange {
                start: Box::new(start_hir),
                end: Box::new(end_hir),
                inclusive: *inclusive,
                step: step_hir,
            }
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
            let null_ty = table
                .lookup(&spark_types::ty::Ty::Null)
                .unwrap_or(TypeId::ERROR);
            desugar::desugar_propagate_optional(inner_hir, span, success_ty, null_ty, definitions)
                .kind
        }
        Ty::Union(_) => {
            if let Some((success_ty, error_types)) =
                spark_types::union::split_union(inner_ty, Some(result_ty), current_fn_ret, table)
            {
                desugar::desugar_propagate_union(
                    inner_hir,
                    span,
                    success_ty,
                    &error_types,
                    definitions,
                )
                .kind
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

/// Lowers a unified pipeline expression.
///
/// Handles sequential/parallel stages and fusion optimization.
/// For now, lowers each stage independently, chaining results.
#[allow(clippy::too_many_lines)]
fn lower_pipeline(
    source: &Expr,
    stages: &[spark_ast::PipelineStage],
    pipeline_expr: &Expr,
    ctx: &mut LowerCtx<'_>,
) -> HirExprKind {
    let span = pipeline_expr.span;

    // Check for fusion opportunity: [parallel Map, Reduce] → ParallelMapReduce
    if stages.len() == 2 {
        if let (
            spark_ast::PipelineStage {
                kind: PipelineStageKind::Map { callback: mapper },
                parallel: true,
                ..
            },
            spark_ast::PipelineStage {
                kind:
                    PipelineStageKind::Reduce {
                        initial,
                        callback: combiner,
                    },
                ..
            },
        ) = (&stages[0], &stages[1])
        {
            let source_hir = lower_expr(source, ctx);
            let mapper_hir = lower_expr(mapper, ctx);
            let combiner_hir = lower_expr(combiner, ctx);
            let initial_hir = lower_expr(initial, ctx);
            let elem_ty = extract_list_elem_ty(source_hir.ty, ctx.table);
            let void_ty = find_void_type(ctx.table);
            let mapped_ty = extract_fn_ret_ty(mapper_hir.ty, ctx.table, void_ty);
            let result_ty = extract_fn_ret_ty(combiner_hir.ty, ctx.table, void_ty);
            return HirExprKind::ParallelMapReduce {
                list: Box::new(source_hir),
                mapper: Box::new(mapper_hir),
                combiner: Box::new(combiner_hir),
                initial: Box::new(initial_hir),
                elem_ty,
                mapped_ty,
                result_ty,
            };
        }
    }

    // General case: lower each stage independently
    // For single-stage pipelines, emit directly; for multi-stage, chain via let bindings
    if stages.len() == 1 {
        let source_hir = lower_expr(source, ctx);
        return lower_single_pipeline_stage(&stages[0], source_hir, span, ctx);
    }

    // Multi-stage: build a block that chains stages via let bindings
    let mut stmts = Vec::new();
    let mut current_hir = lower_expr(source, ctx);

    for (i, stage) in stages.iter().enumerate() {
        if i < stages.len() - 1 {
            // Intermediate stage: wrap in a let binding
            let stage_result_ty = infer_stage_output_ty(stage, current_hir.ty, ctx.table);
            let temp_id = ctx.definitions.next_synthetic_id(stage.span);
            let stage_hir_kind = lower_single_pipeline_stage(
                stage, current_hir, stage.span, ctx,
            );
            let stage_hir = HirExpr::new(stage_hir_kind, stage_result_ty, stage.span);
            stmts.push(HirStmt::Let {
                def_id: temp_id,
                value: stage_hir,
                mutable: false,
            });
            current_hir = HirExpr::new(HirExprKind::Local(temp_id), stage_result_ty, stage.span);
        } else {
            // Last stage: produce the final expression
            let final_kind = lower_single_pipeline_stage(
                stage, current_hir, span, ctx,
            );
            let result_ty = expr_type(pipeline_expr.id, ctx.table);
            let trailing = HirExpr::new(final_kind, result_ty, span);
            return HirExprKind::Block {
                stmts,
                expr: Some(Box::new(trailing)),
            };
        }
    }

    // Should not reach here, but just in case
    HirExprKind::Error
}

/// Infers the output type of a pipeline stage given its input type.
fn infer_stage_output_ty(
    stage: &spark_ast::PipelineStage,
    _input_ty: TypeId,
    table: &TypeTable,
) -> TypeId {
    match &stage.kind {
        PipelineStageKind::ForEach { .. } => find_void_type(table),
        PipelineStageKind::Map { callback } => {
            // Output is List[cb_ret_ty] — we look it up from the type table using the callback id
            let cb_ty = table.get_expr_type(callback.id).unwrap_or(TypeId::ERROR);
            let void_ty = find_void_type(table);
            let ret_ty = extract_fn_ret_ty(cb_ty, table, void_ty);
            table.lookup(&Ty::List(ret_ty)).unwrap_or(TypeId::ERROR)
        }
        PipelineStageKind::Reduce { callback, .. } => {
            // Output is the return type of the callback
            let cb_ty = table.get_expr_type(callback.id).unwrap_or(TypeId::ERROR);
            let void_ty = find_void_type(table);
            extract_fn_ret_ty(cb_ty, table, void_ty)
        }
        PipelineStageKind::Filter { .. } => {
            // Output is List[elem_ty] — same element type as input
            let elem_ty = extract_list_elem_ty(_input_ty, table);
            table.lookup(&Ty::List(elem_ty)).unwrap_or(TypeId::ERROR)
        }
        PipelineStageKind::Find { .. } => {
            // Output is Optional(elem_ty) — first match or Null.
            // The type checker already interned this Optional type.
            let elem_ty = extract_list_elem_ty(_input_ty, table);
            table.lookup(&Ty::Optional(elem_ty)).unwrap_or(TypeId::ERROR)
        }
    }
}

/// Lowers a single pipeline stage to HIR, given the already-lowered source.
fn lower_single_pipeline_stage(
    stage: &spark_ast::PipelineStage,
    source_hir: HirExpr,
    span: Span,
    ctx: &mut LowerCtx<'_>,
) -> HirExprKind {
    let table = ctx.table;
    let int_ty = find_int_type(table);
    let bool_ty = find_bool_type(table);
    let void_ty = find_void_type(table);

    // Iterable: if source is a struct binding Iterable, wrap with .toList()
    let source_hir = maybe_wrap_iterable(source_hir, table);

    match &stage.kind {
        PipelineStageKind::ForEach { callback } => {
            let callback_hir = lower_partial_or_plain(callback, ctx, void_ty);
            let elem_ty = extract_list_elem_ty(source_hir.ty, table);
            if stage.parallel {
                HirExprKind::ParallelForEach {
                    list: Box::new(source_hir),
                    callback: Box::new(callback_hir),
                    elem_ty,
                }
            } else {
                let cb_ret_ty = extract_fn_ret_ty(callback_hir.ty, table, void_ty);
                desugar::desugar_each(
                    source_hir,
                    callback_hir,
                    elem_ty,
                    cb_ret_ty,
                    int_ty,
                    bool_ty,
                    void_ty,
                    span,
                    ctx.definitions,
                )
                .kind
            }
        }
        PipelineStageKind::Map { callback } => {
            let callback_hir = lower_partial_or_plain(callback, ctx, void_ty);
            let elem_ty = extract_list_elem_ty(source_hir.ty, table);
            let cb_ret_ty = extract_fn_ret_ty(callback_hir.ty, table, void_ty);
            if stage.parallel {
                HirExprKind::ParallelMap {
                    list: Box::new(source_hir),
                    callback: Box::new(callback_hir),
                    elem_ty,
                    result_elem_ty: cb_ret_ty,
                }
            } else {
                let result_list_ty = table.lookup(&Ty::List(cb_ret_ty)).unwrap_or(TypeId::ERROR);
                desugar::desugar_map(
                    source_hir,
                    callback_hir,
                    elem_ty,
                    cb_ret_ty,
                    result_list_ty,
                    int_ty,
                    bool_ty,
                    void_ty,
                    span,
                    ctx.definitions,
                )
                .kind
            }
        }
        PipelineStageKind::Reduce { initial, callback } => {
            let callback_hir = lower_expr(callback, ctx);
            let initial_hir = lower_expr(initial, ctx);
            let elem_ty = extract_list_elem_ty(source_hir.ty, table);
            let acc_ty = initial_hir.ty;
            let cb_ret_ty = extract_fn_ret_ty(callback_hir.ty, table, void_ty);
            // Sequential reduce: desugar to accumulator loop
            desugar::desugar_reduce(
                source_hir,
                callback_hir,
                initial_hir,
                elem_ty,
                acc_ty,
                cb_ret_ty,
                int_ty,
                bool_ty,
                void_ty,
                span,
                ctx.definitions,
            )
            .kind
        }
        PipelineStageKind::Filter { callback } => {
            let callback_hir = lower_partial_or_plain(callback, ctx, void_ty);
            let elem_ty = extract_list_elem_ty(source_hir.ty, table);
            let result_list_ty = table.lookup(&Ty::List(elem_ty)).unwrap_or(TypeId::ERROR);
            desugar::desugar_filter(
                source_hir,
                callback_hir,
                elem_ty,
                result_list_ty,
                int_ty,
                bool_ty,
                void_ty,
                span,
                ctx.definitions,
            )
            .kind
        }
        PipelineStageKind::Find { callback } => {
            let callback_hir = lower_partial_or_plain(callback, ctx, void_ty);
            let elem_ty = extract_list_elem_ty(source_hir.ty, table);
            let optional_ty = table.lookup(&Ty::Optional(elem_ty)).unwrap_or(TypeId::ERROR);
            desugar::desugar_find(
                source_hir,
                callback_hir,
                elem_ty,
                optional_ty,
                int_ty,
                bool_ty,
                void_ty,
                span,
                ctx.definitions,
            )
            .kind
        }
    }
}

/// Lowers a `to` narrowing expression into a match.
fn lower_to_narrowing(
    inner_hir: HirExpr,
    arms: &[spark_ast::MatchArm],
    ty: TypeId,
    ctx: &mut LowerCtx<'_>,
    span: Span,
) -> HirExprKind {
    let mut hir_arms: Vec<HirMatchArm> = Vec::with_capacity(arms.len() + 1);

    // Collect the union members for ident-as-type-narrow resolution.
    let union_members: Vec<TypeId> = match ctx.table.resolve(inner_hir.ty) {
        Ty::Union(members) => members.clone(),
        _ => Vec::new(),
    };

    for arm in arms {
        let pattern = lower_to_pattern(
            &arm.pattern,
            ctx.definitions,
            ctx.resolutions,
            ctx.table,
            ctx.builtins,
            &union_members,
        );
        let body = lower_expr(&arm.body, ctx);
        hir_arms.push(HirMatchArm {
            pattern,
            guard: None,
            body,
            span: arm.span,
        });
    }

    // Add passthrough arms for unhandled union members using TypeNarrow
    // so the MIR pattern compiler extracts payloads properly.
    if !union_members.is_empty() {
        let members = &union_members[..];
        // Collect types already handled by explicit arms
        let handled: Vec<TypeId> = hir_arms
            .iter()
            .filter_map(|arm| match &arm.pattern {
                HirPattern::TypeNarrow { ty, .. } => Some(*ty),
                _ => None,
            })
            .collect();

        for &member_ty in members {
            if !handled.contains(&member_ty) {
                let pass_id = ctx.definitions.next_synthetic_id(span);
                hir_arms.push(HirMatchArm {
                    pattern: HirPattern::TypeNarrow {
                        def_id: pass_id,
                        ty: member_ty,
                    },
                    guard: None,
                    body: HirExpr::new(HirExprKind::Local(pass_id), member_ty, span),
                    span,
                });
            }
        }
    } else {
        // Non-union: fall back to binding passthrough
        let pass_id = ctx.definitions.next_synthetic_id(span);
        hir_arms.push(HirMatchArm {
            pattern: HirPattern::Binding { def_id: pass_id },
            guard: None,
            body: HirExpr::new(HirExprKind::Local(pass_id), ty, span),
            span,
        });
    }

    HirExprKind::Match {
        scrutinee: Box::new(inner_hir),
        arms: hir_arms,
    }
}

/// Lowers a pipeline apply expression (`expr to func{args}`).
fn lower_pipeline_apply(
    inner_hir: HirExpr,
    operation: &spark_ast::Ident,
    args: &[spark_ast::PartialArg],
    ctx: &mut LowerCtx<'_>,
    _span: Span,
) -> HirExprKind {
    // Desugar `expr to func{a: v}` → `func(expr, v)`
    let func_def_id = ctx.resolutions
        .and_then(|r| r.get_ident(operation.span))
        .unwrap_or(spark_resolve::DefId::DUMMY);
    let func_ty = ctx.table.get_def_type(func_def_id).unwrap_or(TypeId::ERROR);

    let callee = HirExpr::new(HirExprKind::Local(func_def_id), func_ty, operation.span);

    let param_types = extract_param_types(func_ty, ctx.table);
    let mut call_args = vec![inner_hir];
    for (i, arg) in args.iter().enumerate() {
        let mut arg_hir = lower_expr(&arg.value, ctx);
        if let Some(&param_ty) = param_types.get(i + 1) {
            arg_hir = coerce_ref(arg_hir, param_ty, arg.value.span, ctx.table);
        }
        call_args.push(arg_hir);
    }

    HirExprKind::Call {
        callee: Box::new(callee),
        args: call_args,
    }
}

/// Lowers a pattern from a `to` expression handler arm.
///
/// When `union_members` is non-empty and the pattern is a bare identifier
/// that matches a union member type name, emits `HirPattern::TypeNarrow`
/// instead of `HirPattern::Binding` so the MIR correctly tests the discriminant
/// and extracts the payload.
fn lower_to_pattern(
    pattern: &spark_ast::Pattern,
    definitions: &mut DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    table: &TypeTable,
    builtins: &BuiltinTypes,
    union_members: &[TypeId],
) -> HirPattern {
    match &pattern.kind {
        spark_ast::PatternKind::TypeNarrowing { name, ty } => {
            let def_id = resolutions
                .and_then(|res| res.get_binding(name.span))
                .unwrap_or_else(|| definitions.next_synthetic_id(pattern.span));
            let narrowed_ty = table.get_expr_type(ty.id).unwrap_or(TypeId::ERROR);
            HirPattern::TypeNarrow {
                def_id,
                ty: narrowed_ty,
            }
        }
        spark_ast::PatternKind::Variant { path, fields } => {
            let def_id = resolutions
                .and_then(|res| res.get_path(path.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            HirPattern::Variant {
                def_id,
                fields: fields
                    .iter()
                    .map(|f| lower_to_pattern(f, definitions, resolutions, table, builtins, union_members))
                    .collect(),
            }
        }
        spark_ast::PatternKind::Ident { name, .. } => {
            // In a union `to` expression, a bare type name (e.g., `Int -> ...`)
            // should be treated as type narrowing, not a variable binding.
            if !union_members.is_empty() {
                if let Some(member_ty) =
                    resolve_ident_as_union_member(name.name, union_members, builtins, table, definitions)
                {
                    let def_id = definitions.next_synthetic_id(pattern.span);
                    return HirPattern::TypeNarrow {
                        def_id,
                        ty: member_ty,
                    };
                }
            }
            let def_id = resolutions
                .and_then(|res| res.get_binding(name.span))
                .unwrap_or(spark_resolve::DefId::DUMMY);
            HirPattern::Binding { def_id }
        }
        spark_ast::PatternKind::Wildcard => HirPattern::Wildcard,
        _ => crate::lower_pattern::lower_pattern(pattern, resolutions, table),
    }
}

/// Resolves an identifier name as a union member type.
///
/// Uses `BuiltinTypes::lookup` for builtin types (handles aliases like
/// `Int` → `Int64` automatically) and falls back to definition-name
/// matching for user-defined structs and enums.
fn resolve_ident_as_union_member(
    name: spark_intern::Symbol,
    members: &[TypeId],
    builtins: &BuiltinTypes,
    table: &TypeTable,
    definitions: &DefinitionTable,
) -> Option<TypeId> {
    if let Some(builtin_ty) = builtins.lookup(name) {
        if members.contains(&builtin_ty) {
            return Some(builtin_ty);
        }
    }
    for &member in members {
        match table.resolve(member) {
            Ty::Struct(def_id) | Ty::Enum(def_id) => {
                if let Some(info) = definitions.get(*def_id) {
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

/// Lowers a literal expression, producing either a simple `HirExprKind::Literal`
/// or a `HirExprKind::StringInterp` for interpolated strings.
fn lower_literal_expr(
    kind: &LiteralKind,
    expr: &Expr,
    ctx: &mut LowerCtx<'_>,
) -> HirExprKind {
    match kind {
        LiteralKind::String(s) if !s.is_simple() => {
            // Interpolated string: produce StringInterp with parts.
            let string_ty = expr_type(expr.id, ctx.table);
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
                        let lowered = lower_expr(sub_expr, ctx);
                        // Displayable: wrap struct values in toString()
                        Some(maybe_wrap_displayable(lowered, ctx.table))
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

/// Looks up the type for an expression by its `ExprId`.
fn expr_type(id: spark_ast::ExprId, table: &TypeTable) -> TypeId {
    table.get_expr_type(id).unwrap_or(TypeId::ERROR)
}

/// If the expression has type `Ref[T]`, wraps it in a `Deref` to produce `T`.
pub(crate) fn auto_deref_ref(expr: HirExpr, table: &TypeTable) -> HirExpr {
    let stripped = strip_mutable_ty(expr.ty, table);
    if let Ty::Ref { inner, .. } = table.resolve(stripped) {
        let inner_ty = *inner;
        let span = expr.span;
        HirExpr::new(HirExprKind::Deref(Box::new(expr)), inner_ty, span)
    } else {
        expr
    }
}

/// Lowers a pipeline callback that may be a `Partial` (e.g. `func{b: 3}`).
///
/// For plain callbacks, delegates to `lower_expr`. For `Partial`, synthesizes a
/// 1-parameter lambda that calls the full function with the element as the first
/// argument and the bound args filling the rest:
/// `func{n: 10}` becomes `fn(elem) { func(elem, 10) }`.
fn lower_partial_or_plain(
    callback: &Expr,
    ctx: &mut LowerCtx<'_>,
    void_ty: TypeId,
) -> HirExpr {
    if let ExprKind::Partial { callee, args } = &callback.kind {
        let callee_hir = lower_expr(callee, ctx);
        let param_types = extract_param_types(callee_hir.ty, ctx.table);
        let ret_ty = extract_fn_ret_ty(callee_hir.ty, ctx.table, void_ty);

        // The type checker stored a reduced 1-param function type on the callback
        let lambda_ty = ctx.table.get_expr_type(callback.id).unwrap_or(TypeId::ERROR);
        let elem_ty = if let Ty::Function { params, .. } = ctx.table.resolve(lambda_ty) {
            params.first().copied().unwrap_or(TypeId::ERROR)
        } else {
            TypeId::ERROR
        };

        let param_id = ctx.definitions.next_synthetic_id(callback.span);
        let param = HirParam {
            def_id: param_id,
            ty: elem_ty,
            span: callback.span,
        };

        let mut call_args = vec![HirExpr::new(
            HirExprKind::Local(param_id),
            elem_ty,
            callback.span,
        )];
        for (i, arg) in args.iter().enumerate() {
            let mut arg_hir = lower_expr(&arg.value, ctx);
            if let Some(&pty) = param_types.get(i + 1) {
                arg_hir = coerce_ref(arg_hir, pty, arg.value.span, ctx.table);
            }
            call_args.push(arg_hir);
        }

        let body = HirExpr::new(
            HirExprKind::Call {
                callee: Box::new(callee_hir),
                args: call_args,
            },
            ret_ty,
            callback.span,
        );

        HirExpr::new(
            HirExprKind::Lambda {
                params: vec![param],
                body: Box::new(body),
            },
            lambda_ty,
            callback.span,
        )
    } else {
        lower_expr(callback, ctx)
    }
}

/// Lowers a standalone partial application (`func{a: v1, c: v3}`) into a lambda.
///
/// Synthesizes `fn(unbound_params...) { func(all_args_in_order...) }` where
/// bound parameters use the pre-evaluated values and unbound parameters
/// become lambda parameters.
fn lower_standalone_partial(
    callee: &Expr,
    args: &[spark_ast::PartialArg],
    partial_expr: &Expr,
    ctx: &mut LowerCtx<'_>,
) -> HirExprKind {
    let callee_hir = lower_expr(callee, ctx);
    let full_param_types = extract_param_types(callee_hir.ty, ctx.table);
    let void_ty = find_void_type(ctx.table);
    let ret_ty = extract_fn_ret_ty(callee_hir.ty, ctx.table, void_ty);

    // The type checker stored the reduced function type on this expression
    let lambda_ty = ctx.table
        .get_expr_type(partial_expr.id)
        .unwrap_or(TypeId::ERROR);
    if lambda_ty.is_error() {
        return HirExprKind::Error;
    }

    // Look up parameter names to map named args to positions
    let callee_def_id = find_callee_def_id(callee.span, ctx.resolutions);
    let param_names = callee_def_id.and_then(|did| ctx.table.get_fn_param_names(did));
    let Some(param_names) = param_names else {
        // Type checker already reported this error
        return HirExprKind::Error;
    };

    // Build lambda params (unbound) and call args (all in original order)
    let mut lambda_params = Vec::new();
    let mut call_args = Vec::new();

    for (i, &name) in param_names.iter().enumerate() {
        let param_ty = full_param_types.get(i).copied().unwrap_or(TypeId::ERROR);

        // Check if this parameter is bound by one of the partial args
        let bound_arg = args.iter().find(|a| a.name.name == name);

        if let Some(bound) = bound_arg {
            let mut arg_hir = lower_expr(&bound.value, ctx);
            arg_hir = coerce_ref(arg_hir, param_ty, bound.value.span, ctx.table);
            call_args.push(arg_hir);
        } else {
            // Unbound parameter — becomes a lambda parameter
            let param_id = ctx.definitions.next_synthetic_id(partial_expr.span);
            lambda_params.push(HirParam {
                def_id: param_id,
                ty: param_ty,
                span: partial_expr.span,
            });
            call_args.push(HirExpr::new(
                HirExprKind::Local(param_id),
                param_ty,
                partial_expr.span,
            ));
        }
    }

    let body = HirExpr::new(
        HirExprKind::Call {
            callee: Box::new(callee_hir),
            args: call_args,
        },
        ret_ty,
        partial_expr.span,
    );

    HirExprKind::Lambda {
        params: lambda_params,
        body: Box::new(body),
    }
}

/// Extracts parameter types from a function type.
fn extract_param_types(fn_ty: TypeId, table: &TypeTable) -> Vec<TypeId> {
    if let Ty::Function { params, .. } = table.resolve(fn_ty) {
        params.clone()
    } else {
        vec![]
    }
}

/// Strips `Mutable` wrapper from a type.
fn strip_mutable_ty(ty: TypeId, table: &TypeTable) -> TypeId {
    if let Ty::Mutable(inner) = table.resolve(ty) {
        *inner
    } else {
        ty
    }
}

/// Tries to find the `DefId` for a callee expression.
///
/// Mirrors `find_callee_def_id` in `spark_types::check_expr`.
fn find_callee_def_id(
    callee_span: Span,
    resolutions: Option<&ResolutionMap>,
) -> Option<spark_resolve::DefId> {
    let res = resolutions?;
    res.get_path(callee_span)
        .or_else(|| res.get_ident(callee_span))
}

/// Looks up the `Int` (i64) type in the table.
fn find_int_type(table: &TypeTable) -> TypeId {
    table
        .lookup(&Ty::Int(spark_types::ty::IntWidth::I64))
        .unwrap_or(TypeId::ERROR)
}

/// Looks up the `Bool` type in the table.
fn find_bool_type(table: &TypeTable) -> TypeId {
    table.lookup(&Ty::Bool).unwrap_or(TypeId::ERROR)
}

/// Looks up the `Void` type in the table.
fn find_void_type(table: &TypeTable) -> TypeId {
    table.lookup(&Ty::Void).unwrap_or(TypeId::ERROR)
}

/// Extracts the element type from a `List[T]`, stripping `Mutable` if needed.
fn extract_list_elem_ty(ty: TypeId, table: &TypeTable) -> TypeId {
    let inner = strip_mutable_ty(ty, table);
    match table.resolve(inner) {
        Ty::List(elem) => *elem,
        _ => TypeId::ERROR,
    }
}

/// Extracts the return type from a `Ty::Function`, or returns `default`.
fn extract_fn_ret_ty(ty: TypeId, table: &TypeTable, default: TypeId) -> TypeId {
    match table.resolve(ty) {
        Ty::Function { ret, .. } => *ret,
        _ => default,
    }
}

/// Inserts `Borrow` or `Deref` coercion if the argument type needs to be
/// converted to match the parameter type.
pub(crate) fn coerce_ref(mut arg: HirExpr, param_ty: TypeId, span: Span, table: &TypeTable) -> HirExpr {
    let arg_inner = strip_mutable_ty(arg.ty, table);
    let param_inner = strip_mutable_ty(param_ty, table);
    let arg_resolved = table.resolve(arg_inner);
    let param_resolved = table.resolve(param_inner);

    // Auto-borrow: param is Ref[T], arg is T
    if let Ty::Ref { mutable, .. } = param_resolved {
        if !matches!(arg_resolved, Ty::Ref { .. }) && !arg.ty.is_error() {
            let mutable = *mutable;
            arg = HirExpr::new(
                HirExprKind::Borrow {
                    operand: Box::new(arg),
                    mutable,
                },
                param_ty,
                span,
            );
        }
    }

    // Auto-deref: param is T (not Ref), arg is Ref[T]
    let arg_inner2 = strip_mutable_ty(arg.ty, table);
    if !matches!(param_resolved, Ty::Ref { .. }) {
        if let Ty::Ref { inner, .. } = table.resolve(arg_inner2) {
            let inner_ty = *inner;
            arg = HirExpr::new(HirExprKind::Deref(Box::new(arg)), inner_ty, span);
        }
    }

    arg
}

// ============================================================================
// Standard protocol helpers
// ============================================================================

/// Returns the struct `DefId` if `ty` is a struct type, otherwise `None`.
fn struct_def_id(ty: TypeId, table: &TypeTable) -> Option<spark_resolve::DefId> {
    let inner = strip_mutable_ty(ty, table);
    match table.resolve(inner) {
        Ty::Struct(def_id) => Some(*def_id),
        _ => None,
    }
}

/// Returns `true` if the callee expression is a call to `print`.
fn is_print_call(
    callee: &Expr,
    resolutions: Option<&ResolutionMap>,
    definitions: &DefinitionTable,
) -> bool {
    let def_id = find_callee_def_id(callee.span, resolutions);
    def_id.map_or(false, |id| {
        definitions.get(id).map_or(false, |info| {
            info.name == kw::PRINT && matches!(info.kind, DefinitionKind::BuiltinFunction)
        })
    })
}

/// Finds the DefId of a binding method by struct DefId and method name.
fn find_binding_method(
    struct_def: spark_resolve::DefId,
    method_name: spark_intern::Symbol,
    table: &TypeTable,
) -> Option<spark_resolve::DefId> {
    table
        .get_type_methods(struct_def)
        .and_then(|methods| methods.iter().find(|(name, _)| *name == method_name))
        .map(|(_, def_id)| *def_id)
}

/// If `expr` is a struct that binds `Displayable`, wraps it with a
/// call to the binding's `toString(receiver)` method. Otherwise returns `expr` unchanged.
fn maybe_wrap_displayable(expr: HirExpr, table: &TypeTable) -> HirExpr {
    let Some(displayable_id) = table.well_known.displayable else {
        return expr;
    };
    let Some(sdef) = struct_def_id(expr.ty, table) else {
        return expr;
    };
    if !table.type_binds_interface(sdef, displayable_id) {
        return expr;
    }
    let Some(method_def) = find_binding_method(sdef, kw::TO_STRING, table) else {
        return expr;
    };
    let string_ty = table.lookup(&Ty::String).unwrap_or(TypeId::ERROR);
    let fn_ty = table.get_def_type(method_def).unwrap_or(TypeId::ERROR);
    let span = expr.span;
    let callee = HirExpr::new(HirExprKind::Local(method_def), fn_ty, span);
    HirExpr::new(
        HirExprKind::Call {
            callee: Box::new(callee),
            args: vec![expr],
        },
        string_ty,
        span,
    )
}

/// If `source` is a struct that binds `Iterable[E]`, wraps it with a
/// call to the binding's `toList(receiver)` method. Otherwise returns `source` unchanged.
fn maybe_wrap_iterable(source: HirExpr, table: &TypeTable) -> HirExpr {
    let Some(iterable_id) = table.well_known.iterable else {
        return source;
    };
    let Some(sdef) = struct_def_id(source.ty, table) else {
        return source;
    };
    if !table.type_binds_interface(sdef, iterable_id) {
        return source;
    }
    let Some(method_def) = find_binding_method(sdef, kw::TO_LIST, table) else {
        return source;
    };
    let fn_ty = table.get_def_type(method_def).unwrap_or(TypeId::ERROR);
    // Extract the return type (List[E]) from the method's function type
    let list_ty = match table.resolve(fn_ty) {
        Ty::Function { ret, .. } => *ret,
        _ => TypeId::ERROR,
    };
    let span = source.span;
    let callee = HirExpr::new(HirExprKind::Local(method_def), fn_ty, span);
    HirExpr::new(
        HirExprKind::Call {
            callee: Box::new(callee),
            args: vec![source],
        },
        list_ty,
        span,
    )
}

/// Returns `true` if the lhs operand is a struct binding `Equatable`.
fn should_desugar_equatable(lhs: &HirExpr, table: &TypeTable) -> bool {
    let Some(equatable_id) = table.well_known.equatable else {
        return false;
    };
    let Some(sdef) = struct_def_id(lhs.ty, table) else {
        return false;
    };
    table.type_binds_interface(sdef, equatable_id)
}

/// Desugars `a == b` to `equals(a, b)` and `a != b` to `!equals(a, b)`.
/// Caller must verify `should_desugar_equatable` first.
fn desugar_equatable(op: BinaryOp, lhs: HirExpr, rhs: HirExpr, table: &TypeTable) -> HirExprKind {
    let bool_ty = find_bool_type(table);
    let span = lhs.span;
    let Some(sdef) = struct_def_id(lhs.ty, table) else {
        return HirExprKind::BinaryOp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        };
    };
    let Some(method_def) = find_binding_method(sdef, kw::EQUALS, table) else {
        return HirExprKind::BinaryOp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        };
    };
    let fn_ty = table.get_def_type(method_def).unwrap_or(TypeId::ERROR);
    let callee = HirExpr::new(HirExprKind::Local(method_def), fn_ty, span);
    let equals_call = HirExpr::new(
        HirExprKind::Call {
            callee: Box::new(callee),
            args: vec![lhs, rhs],
        },
        bool_ty,
        span,
    );
    if op == BinaryOp::Ne {
        HirExprKind::UnaryOp {
            op: spark_ast::UnaryOp::Not,
            operand: Box::new(equals_call),
        }
    } else {
        equals_call.kind
    }
}

/// Returns `true` if the lhs operand is a struct binding `Comparable`.
fn should_desugar_comparable(lhs: &HirExpr, table: &TypeTable) -> bool {
    let Some(comparable_id) = table.well_known.comparable else {
        return false;
    };
    let Some(sdef) = struct_def_id(lhs.ty, table) else {
        return false;
    };
    table.type_binds_interface(sdef, comparable_id)
}

/// Desugars `a < b` to `compareTo(a, b) < 0`, etc.
/// Caller must verify `should_desugar_comparable` first.
fn desugar_comparable(op: BinaryOp, lhs: HirExpr, rhs: HirExpr, table: &TypeTable) -> HirExprKind {
    let int_ty = find_int_type(table);
    let span = lhs.span;
    let Some(sdef) = struct_def_id(lhs.ty, table) else {
        return HirExprKind::BinaryOp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        };
    };
    let Some(method_def) = find_binding_method(sdef, kw::COMPARE_TO, table) else {
        return HirExprKind::BinaryOp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        };
    };
    let fn_ty = table.get_def_type(method_def).unwrap_or(TypeId::ERROR);
    let callee = HirExpr::new(HirExprKind::Local(method_def), fn_ty, span);
    let cmp_call = HirExpr::new(
        HirExprKind::Call {
            callee: Box::new(callee),
            args: vec![lhs, rhs],
        },
        int_ty,
        span,
    );
    let zero = HirExpr::new(HirExprKind::Literal(HirLiteral::Int(0)), int_ty, span);
    HirExprKind::BinaryOp {
        op,
        lhs: Box::new(cmp_call),
        rhs: Box::new(zero),
    }
}
