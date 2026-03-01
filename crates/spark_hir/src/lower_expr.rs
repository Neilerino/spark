//! Expression lowering.
//!
//! Converts AST [`Expr`] nodes into HIR [`HirExpr`] nodes, resolving
//! identifiers to [`DefId`]s and looking up types from the [`TypeTable`].

use spark_ast::{Expr, ExprKind, LiteralKind, PipelineStageKind, StringPart};
use spark_diagnostics::Diagnostics;
use spark_resolve::{DefinitionTable, ResolutionMap};
use spark_span::Span;
use spark_types::ty::Ty;
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
    let ty = expr_type(expr.id, table);
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
        .map_or_else(|| expr_type(block.id, table), |e| e.ty);

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
        ExprKind::Binary { op, lhs, rhs } => {
            let lhs_hir = auto_deref_ref(lower_expr(lhs, table, definitions, resolutions, diagnostics, current_fn_ret), table);
            let rhs_hir = auto_deref_ref(lower_expr(rhs, table, definitions, resolutions, diagnostics, current_fn_ret), table);
            HirExprKind::BinaryOp {
                op: *op,
                lhs: Box::new(lhs_hir),
                rhs: Box::new(rhs_hir),
            }
        }

        ExprKind::Unary { op, operand } => {
            let operand_hir = auto_deref_ref(lower_expr(operand, table, definitions, resolutions, diagnostics, current_fn_ret), table);
            HirExprKind::UnaryOp {
                op: *op,
                operand: Box::new(operand_hir),
            }
        }

        // === Access ===
        ExprKind::Field { base, field } => {
            let mut base_hir = lower_expr(base, table, definitions, resolutions, diagnostics, current_fn_ret);
            // Auto-deref through Ref (and Mutable(Ref))
            let base_ty = strip_mutable_ty(base_hir.ty, table);
            if let Ty::Ref { inner, .. } = table.resolve(base_ty) {
                let inner_ty = *inner;
                base_hir = HirExpr::new(
                    HirExprKind::Deref(Box::new(base_hir)),
                    inner_ty, base.span,
                );
            }
            HirExprKind::FieldAccess {
                base: Box::new(base_hir),
                field: field.name,
            }
        }

        ExprKind::Index { base, index } => HirExprKind::Index {
            base: Box::new(lower_expr(base, table, definitions, resolutions, diagnostics, current_fn_ret)),
            index: Box::new(lower_expr(index, table, definitions, resolutions, diagnostics, current_fn_ret)),
        },

        // === Calls ===
        ExprKind::Call { callee, args } => {
            let callee_hir = lower_expr(callee, table, definitions, resolutions, diagnostics, current_fn_ret);
            let param_types = extract_param_types(callee_hir.ty, table);
            let lowered_args: Vec<HirExpr> = args.iter().enumerate().map(|(i, a)| {
                let mut arg = lower_expr(&a.value, table, definitions, resolutions, diagnostics, current_fn_ret);
                if let Some(&param_ty) = param_types.get(i) {
                    arg = coerce_ref(arg, param_ty, a.value.span, table);
                }
                arg
            }).collect();
            HirExprKind::Call {
                callee: Box::new(callee_hir),
                args: lowered_args,
            }
        }

        ExprKind::MethodCall { receiver, method, args } => {
            let mut recv_hir = lower_expr(receiver, table, definitions, resolutions, diagnostics, current_fn_ret);
            // Auto-deref through Ref (and Mutable(Ref))
            let recv_ty = strip_mutable_ty(recv_hir.ty, table);
            if let Ty::Ref { inner, .. } = table.resolve(recv_ty) {
                let inner_ty = *inner;
                recv_hir = HirExpr::new(
                    HirExprKind::Deref(Box::new(recv_hir)),
                    inner_ty, receiver.span,
                );
            }
            HirExprKind::MethodCall {
                receiver: Box::new(recv_hir),
                method: method.name,
                args: args.iter().map(|a| lower_expr(&a.value, table, definitions, resolutions, diagnostics, current_fn_ret)).collect(),
            }
        }

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
            value.as_ref().map(|v| {
                let mut lowered = lower_expr(v, table, definitions, resolutions, diagnostics, current_fn_ret);
                if let Some(ret_ty) = current_fn_ret {
                    let ret_stripped = strip_mutable_ty(ret_ty, table);
                    // Auto-borrow: if function returns Ref[T] and value is T,
                    // wrap in Borrow so the borrow checker can detect dangling refs.
                    if let Ty::Ref { mutable: ref_mut, .. } = table.resolve(ret_stripped) {
                        let val_stripped = strip_mutable_ty(lowered.ty, table);
                        if !matches!(table.resolve(val_stripped), Ty::Ref { .. })
                            && !lowered.ty.is_error()
                        {
                            lowered = HirExpr::new(
                                HirExprKind::Borrow { operand: Box::new(lowered), mutable: *ref_mut },
                                ret_ty, v.span,
                            );
                        }
                    }
                    // Auto-deref: if function returns T and value is Ref[T],
                    // wrap in Deref.
                    if !matches!(table.resolve(ret_stripped), Ty::Ref { .. }) {
                        let val_stripped = strip_mutable_ty(lowered.ty, table);
                        if let Ty::Ref { inner, .. } = table.resolve(val_stripped) {
                            let inner_ty = *inner;
                            lowered = HirExpr::new(
                                HirExprKind::Deref(Box::new(lowered)),
                                inner_ty, v.span,
                            );
                        }
                    }
                }
                Box::new(lowered)
            }),
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
            let ty = expr_type(expr.id, table);
            desugar::desugar_coalesce(lhs_hir, rhs_hir, expr.span, ty, definitions).kind
        }

        // === Desugared: OptionalField (?.) ===
        ExprKind::OptionalField { base, field } => {
            let base_hir = lower_expr(base, table, definitions, resolutions, diagnostics, current_fn_ret);
            let ty = expr_type(expr.id, table);
            let inner_ty = spark_types::union::strip_null(base_hir.ty, table);
            desugar::desugar_optional_field(base_hir, field.name, expr.span, ty, inner_ty, definitions).kind
        }

        // === Desugared: Propagate (!) ===
        ExprKind::Propagate(inner) => {
            let inner_hir = lower_expr(inner, table, definitions, resolutions, diagnostics, current_fn_ret);
            let ty = expr_type(expr.id, table);
            let inner_ty = inner_hir.ty;
            desugar_propagate_expr(inner_hir, inner_ty, ty, current_fn_ret, expr.span, table, definitions)
        }

        // === Unified Pipeline ===
        ExprKind::Pipeline { source, stages } => {
            lower_pipeline(source, stages, expr, table, definitions, resolutions, diagnostics, current_fn_ret)
        }

        ExprKind::To { expr: inner, arms } => {
            let inner_hir = lower_expr(inner, table, definitions, resolutions, diagnostics, current_fn_ret);
            let ty = expr_type(expr.id, table);
            lower_to_narrowing(inner_hir, arms, ty, table, definitions, resolutions, diagnostics, current_fn_ret, expr.span)
        }

        ExprKind::PipelineApply { expr: inner, operation, args } => {
            let inner_hir = lower_expr(inner, table, definitions, resolutions, diagnostics, current_fn_ret);
            lower_pipeline_apply(inner_hir, operation, args, table, definitions, resolutions, diagnostics, current_fn_ret, expr.span)
        }

        ExprKind::Partial { callee, args } => {
            let _ = lower_expr(callee, table, definitions, resolutions, diagnostics, current_fn_ret);
            for arg in args {
                let _ = lower_expr(&arg.value, table, definitions, resolutions, diagnostics, current_fn_ret);
            }
            HirExprKind::Error
        }

        ExprKind::Range { start, end, .. } => {
            let _ = lower_expr(start, table, definitions, resolutions, diagnostics, current_fn_ret);
            let _ = lower_expr(end, table, definitions, resolutions, diagnostics, current_fn_ret);
            HirExprKind::Error
        }

        ExprKind::ListRange { start, end, inclusive, step } => {
            let start_hir = lower_expr(start, table, definitions, resolutions, diagnostics, current_fn_ret);
            let end_hir = lower_expr(end, table, definitions, resolutions, diagnostics, current_fn_ret);
            let step_hir = step.as_ref().map(|s| {
                Box::new(lower_expr(s, table, definitions, resolutions, diagnostics, current_fn_ret))
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

/// Lowers a unified pipeline expression.
///
/// Handles sequential/parallel stages and fusion optimization.
/// For now, lowers each stage independently, chaining results.
#[allow(clippy::too_many_arguments)]
fn lower_pipeline(
    source: &Expr,
    stages: &[spark_ast::PipelineStage],
    pipeline_expr: &Expr,
    table: &TypeTable,
    definitions: &mut DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut Diagnostics,
    current_fn_ret: Option<TypeId>,
) -> HirExprKind {
    let span = pipeline_expr.span;

    // Check for fusion opportunity: [parallel Map, Reduce] → ParallelMapReduce
    if stages.len() == 2 {
        if let (
            spark_ast::PipelineStage { kind: PipelineStageKind::Map { callback: mapper }, parallel: true, .. },
            spark_ast::PipelineStage { kind: PipelineStageKind::Reduce { initial, callback: combiner }, .. },
        ) = (&stages[0], &stages[1])
        {
            let source_hir = lower_expr(source, table, definitions, resolutions, diagnostics, current_fn_ret);
            let mapper_hir = lower_expr(mapper, table, definitions, resolutions, diagnostics, current_fn_ret);
            let combiner_hir = lower_expr(combiner, table, definitions, resolutions, diagnostics, current_fn_ret);
            let initial_hir = lower_expr(initial, table, definitions, resolutions, diagnostics, current_fn_ret);
            let elem_ty = extract_list_elem_ty(source_hir.ty, table);
            let void_ty = find_void_type(table);
            let mapped_ty = extract_fn_ret_ty(mapper_hir.ty, table, void_ty);
            let result_ty = extract_fn_ret_ty(combiner_hir.ty, table, void_ty);
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
        let source_hir = lower_expr(source, table, definitions, resolutions, diagnostics, current_fn_ret);
        return lower_single_pipeline_stage(&stages[0], source_hir, span, table, definitions, resolutions, diagnostics, current_fn_ret);
    }

    // Multi-stage: build a block that chains stages via let bindings
    let mut stmts = Vec::new();
    let mut current_hir = lower_expr(source, table, definitions, resolutions, diagnostics, current_fn_ret);

    for (i, stage) in stages.iter().enumerate() {
        if i < stages.len() - 1 {
            // Intermediate stage: wrap in a let binding
            let stage_result_ty = infer_stage_output_ty(stage, current_hir.ty, table);
            let temp_id = definitions.next_synthetic_id(stage.span);
            let stage_hir_kind = lower_single_pipeline_stage(stage, current_hir, stage.span, table, definitions, resolutions, diagnostics, current_fn_ret);
            let stage_hir = HirExpr::new(stage_hir_kind, stage_result_ty, stage.span);
            stmts.push(HirStmt::Let {
                def_id: temp_id,
                value: stage_hir,
                mutable: false,
            });
            current_hir = HirExpr::new(HirExprKind::Local(temp_id), stage_result_ty, stage.span);
        } else {
            // Last stage: produce the final expression
            let final_kind = lower_single_pipeline_stage(stage, current_hir, span, table, definitions, resolutions, diagnostics, current_fn_ret);
            let result_ty = expr_type(pipeline_expr.id, table);
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
    }
}

/// Lowers a single pipeline stage to HIR, given the already-lowered source.
#[allow(clippy::too_many_arguments)]
fn lower_single_pipeline_stage(
    stage: &spark_ast::PipelineStage,
    source_hir: HirExpr,
    span: Span,
    table: &TypeTable,
    definitions: &mut DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut Diagnostics,
    current_fn_ret: Option<TypeId>,
) -> HirExprKind {
    let int_ty = find_int_type(table);
    let bool_ty = find_bool_type(table);
    let void_ty = find_void_type(table);

    match &stage.kind {
        PipelineStageKind::ForEach { callback } => {
            let callback_hir = lower_partial_or_plain(
                callback, table, definitions, resolutions, diagnostics, current_fn_ret, void_ty,
            );
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
                    source_hir, callback_hir, elem_ty, cb_ret_ty,
                    int_ty, bool_ty, void_ty, span, definitions,
                ).kind
            }
        }
        PipelineStageKind::Map { callback } => {
            let callback_hir = lower_partial_or_plain(
                callback, table, definitions, resolutions, diagnostics, current_fn_ret, void_ty,
            );
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
                desugar::desugar_each_via(
                    source_hir, callback_hir, elem_ty, cb_ret_ty,
                    result_list_ty, int_ty, bool_ty, void_ty,
                    span, definitions,
                ).kind
            }
        }
        PipelineStageKind::Reduce { initial, callback } => {
            let callback_hir = lower_expr(callback, table, definitions, resolutions, diagnostics, current_fn_ret);
            let initial_hir = lower_expr(initial, table, definitions, resolutions, diagnostics, current_fn_ret);
            let elem_ty = extract_list_elem_ty(source_hir.ty, table);
            let acc_ty = initial_hir.ty;
            let cb_ret_ty = extract_fn_ret_ty(callback_hir.ty, table, void_ty);
            // Sequential reduce: desugar to accumulator loop
            desugar::desugar_reduce(
                source_hir, callback_hir, initial_hir, elem_ty, acc_ty, cb_ret_ty,
                int_ty, bool_ty, void_ty, span, definitions,
            ).kind
        }
    }
}

/// Lowers a `to` narrowing expression into a match.
#[allow(clippy::too_many_arguments)]
fn lower_to_narrowing(
    inner_hir: HirExpr,
    arms: &[spark_ast::MatchArm],
    ty: TypeId,
    table: &TypeTable,
    definitions: &mut DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut Diagnostics,
    current_fn_ret: Option<TypeId>,
    span: Span,
) -> HirExprKind {
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

    // Add passthrough arms for unhandled union members using TypeNarrow
    // so the MIR pattern compiler extracts payloads properly.
    let inner_resolved = table.resolve(inner_hir.ty);
    if let Ty::Union(members) = inner_resolved {
        let members = members.clone();
        // Collect types already handled by explicit arms
        let handled: Vec<TypeId> = hir_arms
            .iter()
            .filter_map(|arm| match &arm.pattern {
                HirPattern::TypeNarrow { ty, .. } => Some(*ty),
                _ => None,
            })
            .collect();

        for &member_ty in &members {
            if !handled.contains(&member_ty) {
                let pass_id = definitions.next_synthetic_id(span);
                hir_arms.push(HirMatchArm {
                    pattern: HirPattern::TypeNarrow { def_id: pass_id, ty: member_ty },
                    guard: None,
                    body: HirExpr::new(HirExprKind::Local(pass_id), member_ty, span),
                    span,
                });
            }
        }
    } else {
        // Non-union: fall back to binding passthrough
        let pass_id = definitions.next_synthetic_id(span);
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

/// Lowers a pipeline apply expression (`expr via func{args}` or `expr to func{args}`).
#[allow(clippy::too_many_arguments)]
fn lower_pipeline_apply(
    inner_hir: HirExpr,
    operation: &spark_ast::Ident,
    args: &[spark_ast::PartialArg],
    table: &TypeTable,
    definitions: &mut DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut Diagnostics,
    current_fn_ret: Option<TypeId>,
    _span: Span,
) -> HirExprKind {
    // Desugar `expr via func{a: v}` → `func(expr, v)`
    let func_def_id = resolutions
        .and_then(|r| r.get_ident(operation.span))
        .unwrap_or(spark_resolve::DefId::DUMMY);
    let func_ty = table.get_def_type(func_def_id).unwrap_or(TypeId::ERROR);

    let callee = HirExpr::new(HirExprKind::Local(func_def_id), func_ty, operation.span);

    let param_types = extract_param_types(func_ty, table);
    let mut call_args = vec![inner_hir];
    for (i, arg) in args.iter().enumerate() {
        let mut arg_hir = lower_expr(&arg.value, table, definitions, resolutions, diagnostics, current_fn_ret);
        if let Some(&param_ty) = param_types.get(i + 1) {
            arg_hir = coerce_ref(arg_hir, param_ty, arg.value.span, table);
        }
        call_args.push(arg_hir);
    }

    HirExprKind::Call {
        callee: Box::new(callee),
        args: call_args,
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
            let narrowed_ty = table.get_expr_type(ty.id).unwrap_or(TypeId::ERROR);
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
        _ => crate::lower_pattern::lower_pattern(pattern, resolutions, table),
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
            let string_ty = expr_type(expr.id, table);
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
#[allow(clippy::too_many_arguments)]
fn lower_partial_or_plain(
    callback: &Expr,
    table: &TypeTable,
    definitions: &mut DefinitionTable,
    resolutions: Option<&ResolutionMap>,
    diagnostics: &mut Diagnostics,
    current_fn_ret: Option<TypeId>,
    void_ty: TypeId,
) -> HirExpr {
    if let ExprKind::Partial { callee, args } = &callback.kind {
        let callee_hir = lower_expr(callee, table, definitions, resolutions, diagnostics, current_fn_ret);
        let param_types = extract_param_types(callee_hir.ty, table);
        let ret_ty = extract_fn_ret_ty(callee_hir.ty, table, void_ty);

        // The type checker stored a reduced 1-param function type on the callback
        let lambda_ty = table.get_expr_type(callback.id).unwrap_or(TypeId::ERROR);
        let elem_ty = if let Ty::Function { params, .. } = table.resolve(lambda_ty) {
            params.first().copied().unwrap_or(TypeId::ERROR)
        } else {
            TypeId::ERROR
        };

        let param_id = definitions.next_synthetic_id(callback.span);
        let param = HirParam { def_id: param_id, ty: elem_ty, span: callback.span };

        let mut call_args = vec![HirExpr::new(HirExprKind::Local(param_id), elem_ty, callback.span)];
        for (i, arg) in args.iter().enumerate() {
            let mut arg_hir = lower_expr(&arg.value, table, definitions, resolutions, diagnostics, current_fn_ret);
            if let Some(&pty) = param_types.get(i + 1) {
                arg_hir = coerce_ref(arg_hir, pty, arg.value.span, table);
            }
            call_args.push(arg_hir);
        }

        let body = HirExpr::new(
            HirExprKind::Call { callee: Box::new(callee_hir), args: call_args },
            ret_ty,
            callback.span,
        );

        HirExpr::new(
            HirExprKind::Lambda { params: vec![param], body: Box::new(body) },
            lambda_ty,
            callback.span,
        )
    } else {
        lower_expr(callback, table, definitions, resolutions, diagnostics, current_fn_ret)
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
fn coerce_ref(mut arg: HirExpr, param_ty: TypeId, span: Span, table: &TypeTable) -> HirExpr {
    let arg_inner = strip_mutable_ty(arg.ty, table);
    let param_inner = strip_mutable_ty(param_ty, table);
    let arg_resolved = table.resolve(arg_inner);
    let param_resolved = table.resolve(param_inner);

    // Auto-borrow: param is Ref[T], arg is T
    if let Ty::Ref { mutable, .. } = param_resolved {
        if !matches!(arg_resolved, Ty::Ref { .. }) && !arg.ty.is_error() {
            let mutable = *mutable;
            arg = HirExpr::new(
                HirExprKind::Borrow { operand: Box::new(arg), mutable },
                param_ty, span,
            );
        }
    }

    // Auto-deref: param is T (not Ref), arg is Ref[T]
    let arg_inner2 = strip_mutable_ty(arg.ty, table);
    if !matches!(param_resolved, Ty::Ref { .. }) {
        if let Ty::Ref { inner, .. } = table.resolve(arg_inner2) {
            let inner_ty = *inner;
            arg = HirExpr::new(
                HirExprKind::Deref(Box::new(arg)),
                inner_ty, span,
            );
        }
    }

    arg
}
