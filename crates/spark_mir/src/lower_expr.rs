//! Expression lowering: HIR expressions → MIR locals.
//!
//! Every expression is lowered into a [`Local`] that holds its result.
//! Control flow expressions (if, loop, match) create new basic blocks.

use spark_diagnostics::{Diagnostic, Label};
use spark_hir::{HirExpr, HirExprKind, HirLiteral};
use spark_intern::Symbol;
use spark_resolve::{DefinitionKind, DefinitionTable};
use spark_types::{Ty, TypeId, TypeTable};

use crate::builder::{FunctionBuilder, LoopContext, MirBuilder};
use crate::lower_pattern::lower_match;
use crate::lower_stmt::lower_stmt;
use crate::mir::{
    AggregateKind, Constant, Local, Operand, Place, Projection, Rvalue, Terminator,
};

/// Lowers an HIR expression, returning the [`Local`] holding the result.
#[allow(clippy::too_many_lines)]
pub fn lower_expr(
    expr: &HirExpr,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) -> Local {
    match &expr.kind {
        // ── Values ───────────────────────────────────────────────
        HirExprKind::Literal(lit) => lower_literal(lit, expr, fb),

        HirExprKind::Local(def_id) => fb.lookup_def(*def_id).unwrap_or_else(|| {
            // Could be a function reference
            let temp = fb.fresh_temp(expr.ty, expr.span);
            fb.push_assign(
                Place::from_local(temp),
                Rvalue::Use(Operand::Constant(Constant::FunctionRef(*def_id))),
                expr.span,
            );
            temp
        }),

        // ── Operators ────────────────────────────────────────────
        HirExprKind::BinaryOp { op, lhs, rhs } => {
            let lhs_local = lower_expr(lhs, fb, mir, type_table, defs);
            let rhs_local = lower_expr(rhs, fb, mir, type_table, defs);
            let temp = fb.fresh_temp(expr.ty, expr.span);
            fb.push_assign(
                Place::from_local(temp),
                Rvalue::BinaryOp(
                    *op,
                    Operand::Copy(Place::from_local(lhs_local)),
                    Operand::Copy(Place::from_local(rhs_local)),
                ),
                expr.span,
            );
            temp
        }

        HirExprKind::UnaryOp { op, operand } => {
            let operand_local = lower_expr(operand, fb, mir, type_table, defs);
            let temp = fb.fresh_temp(expr.ty, expr.span);
            fb.push_assign(
                Place::from_local(temp),
                Rvalue::UnaryOp(*op, Operand::Copy(Place::from_local(operand_local))),
                expr.span,
            );
            temp
        }

        // ── Access ───────────────────────────────────────────────
        HirExprKind::FieldAccess { base, field } => {
            let base_local = lower_expr(base, fb, mir, type_table, defs);
            let temp = fb.fresh_temp(expr.ty, expr.span);
            let mut place = Place::from_local(base_local);
            place.projections.push(Projection::Field(*field));
            fb.push_assign(
                Place::from_local(temp),
                Rvalue::Use(Operand::Copy(place)),
                expr.span,
            );
            temp
        }

        HirExprKind::Index { base, index } => {
            let base_local = lower_expr(base, fb, mir, type_table, defs);
            let index_local = lower_expr(index, fb, mir, type_table, defs);
            let temp = fb.fresh_temp(expr.ty, expr.span);
            let mut place = Place::from_local(base_local);
            place.projections.push(Projection::Index(index_local));
            fb.push_assign(
                Place::from_local(temp),
                Rvalue::Use(Operand::Copy(place)),
                expr.span,
            );
            temp
        }

        // ── Calls ────────────────────────────────────────────────
        HirExprKind::Call { callee, args } => {
            lower_call(callee, args, expr, fb, mir, type_table, defs)
        }

        HirExprKind::MethodCall {
            receiver,
            method,
            args,
        } => lower_method_call(receiver, *method, args, expr, fb, mir, type_table, defs),

        // ── Constructors ─────────────────────────────────────────
        HirExprKind::StructInit { def_id, fields } => {
            let field_names: Vec<Symbol> = fields.iter().map(|(name, _)| *name).collect();
            let operands: Vec<Operand> = fields
                .iter()
                .map(|(_, val)| {
                    let local = lower_expr(val, fb, mir, type_table, defs);
                    Operand::Copy(Place::from_local(local))
                })
                .collect();
            let temp = fb.fresh_temp(expr.ty, expr.span);
            fb.push_assign(
                Place::from_local(temp),
                Rvalue::Aggregate(
                    AggregateKind::Struct {
                        def_id: *def_id,
                        fields: field_names,
                    },
                    operands,
                ),
                expr.span,
            );
            temp
        }

        HirExprKind::EnumVariant {
            def_id,
            variant,
            fields,
        } => {
            let operands: Vec<Operand> = fields
                .iter()
                .map(|val| {
                    let local = lower_expr(val, fb, mir, type_table, defs);
                    Operand::Copy(Place::from_local(local))
                })
                .collect();
            let temp = fb.fresh_temp(expr.ty, expr.span);
            fb.push_assign(
                Place::from_local(temp),
                Rvalue::Aggregate(
                    AggregateKind::EnumVariant {
                        def_id: *def_id,
                        variant: *variant,
                    },
                    operands,
                ),
                expr.span,
            );
            temp
        }

        HirExprKind::ListLit(elems) => {
            lower_collection(elems, AggregateKind::List, expr, fb, mir, type_table, defs)
        }

        HirExprKind::SetLit(elems) => {
            lower_collection(elems, AggregateKind::Set, expr, fb, mir, type_table, defs)
        }

        HirExprKind::TupleLit(elems) => {
            lower_collection(elems, AggregateKind::Tuple, expr, fb, mir, type_table, defs)
        }

        HirExprKind::MapLit(entries) => {
            // Flatten key-value pairs into operands: [k1, v1, k2, v2, ...]
            let mut operands = Vec::with_capacity(entries.len() * 2);
            for (key, val) in entries {
                let k = lower_expr(key, fb, mir, type_table, defs);
                let v = lower_expr(val, fb, mir, type_table, defs);
                operands.push(Operand::Copy(Place::from_local(k)));
                operands.push(Operand::Copy(Place::from_local(v)));
            }
            let temp = fb.fresh_temp(expr.ty, expr.span);
            fb.push_assign(
                Place::from_local(temp),
                Rvalue::Aggregate(AggregateKind::Map, operands),
                expr.span,
            );
            temp
        }

        // ── Control flow ─────────────────────────────────────────
        HirExprKind::Block { stmts, expr: tail } => {
            fb.push_scope();
            for s in stmts {
                lower_stmt(s, fb, mir, type_table, defs);
            }
            let result = if let Some(tail_expr) = tail {
                lower_expr(tail_expr, fb, mir, type_table, defs)
            } else {
                let temp = fb.fresh_temp(expr.ty, expr.span);
                fb.push_assign(
                    Place::from_local(temp),
                    Rvalue::Use(Operand::Constant(Constant::Void)),
                    expr.span,
                );
                temp
            };
            fb.pop_scope_drops(Some(result), expr.span);
            result
        }

        HirExprKind::If {
            cond,
            then_,
            else_,
        } => lower_if(cond, then_, else_.as_deref(), expr, fb, mir, type_table, defs),

        HirExprKind::Match { scrutinee, arms } => {
            lower_match(scrutinee, arms, expr, fb, mir, type_table, defs)
        }

        HirExprKind::Loop { body } => lower_loop(body, expr, fb, mir, type_table, defs),

        HirExprKind::Break(value) => {
            lower_break(value.as_deref(), expr, fb, mir, type_table, defs)
        }

        HirExprKind::Continue => {
            if let Some(ctx) = fb.current_loop() {
                let target = ctx.continue_block;
                // Drop locals in scopes above the loop before jumping back.
                fb.drops_to_loop(None, expr.span);
                fb.terminate(Terminator::Goto { target });
            }
            // Return a void temp (unreachable in practice)
            let temp = fb.fresh_temp(expr.ty, expr.span);
            fb.push_assign(
                Place::from_local(temp),
                Rvalue::Use(Operand::Constant(Constant::Void)),
                expr.span,
            );
            temp
        }

        HirExprKind::Return(value) => {
            lower_return(value.as_deref(), expr, fb, mir, type_table, defs)
        }

        // ── Type operations ──────────────────────────────────────
        HirExprKind::As { expr: inner, target_ty } => {
            let operand_local = lower_expr(inner, fb, mir, type_table, defs);
            let temp = fb.fresh_temp(expr.ty, expr.span);
            fb.push_assign(
                Place::from_local(temp),
                Rvalue::Cast {
                    operand: Operand::Copy(Place::from_local(operand_local)),
                    target_ty: *target_ty,
                },
                expr.span,
            );
            temp
        }

        // ── References ────────────────────────────────────────────
        HirExprKind::Borrow { operand, mutable } => {
            let operand_local = lower_expr(operand, fb, mir, type_table, defs);
            let temp = fb.fresh_temp(expr.ty, expr.span);
            fb.push_assign(
                Place::from_local(temp),
                Rvalue::Ref {
                    place: Place::from_local(operand_local),
                    mutable: *mutable,
                },
                expr.span,
            );
            temp
        }

        HirExprKind::Deref(inner) => {
            let inner_local = lower_expr(inner, fb, mir, type_table, defs);
            let temp = fb.fresh_temp(expr.ty, expr.span);
            let mut place = Place::from_local(inner_local);
            place.projections.push(Projection::Deref);
            fb.push_assign(
                Place::from_local(temp),
                Rvalue::Use(Operand::Copy(place)),
                expr.span,
            );
            temp
        }

        // ── Type test ─────────────────────────────────────────────
        HirExprKind::Is { expr: inner, target_ty } => {
            lower_is_expr(inner, *target_ty, expr, fb, mir, type_table, defs)
        }

        HirExprKind::Lambda { .. } => {
            // After lambda lifting, no Lambda nodes should reach MIR.
            // If one does, it indicates a compiler bug.
            mir.diagnostics.push(
                Diagnostic::error("internal: lambda not lifted before MIR lowering")
                    .with_label(Label::primary(expr.span, "unliftable lambda")),
            );
            void_temp(fb, expr)
        }

        HirExprKind::MakeClosure {
            func_def_id,
            captures,
        } => {
            // Lower each captured variable to an operand.
            let operands: Vec<Operand> = captures
                .iter()
                .map(|(def_id, _ty)| {
                    let local = fb.lookup_def(*def_id).unwrap_or_else(|| {
                        // Should not happen — capture analysis found this DefId.
                        let temp = fb.fresh_temp(expr.ty, expr.span);
                        fb.push_assign(
                            Place::from_local(temp),
                            Rvalue::Use(Operand::Constant(Constant::Void)),
                            expr.span,
                        );
                        temp
                    });
                    Operand::Copy(Place::from_local(local))
                })
                .collect();

            let temp = fb.fresh_temp(expr.ty, expr.span);
            fb.push_assign(
                Place::from_local(temp),
                Rvalue::Aggregate(
                    AggregateKind::Closure {
                        func_def_id: *func_def_id,
                    },
                    operands,
                ),
                expr.span,
            );
            temp
        }

        HirExprKind::Await(_) => {
            emit_deferred(mir, "`await` expressions", expr);
            void_temp(fb, expr)
        }

        HirExprKind::Spawn(_) => {
            emit_deferred(mir, "`spawn` expressions", expr);
            void_temp(fb, expr)
        }

        // ── String interpolation ──────────────────────────────────
        HirExprKind::StringInterp { parts } => {
            lower_string_interp(parts, expr, fb, mir, type_table, defs)
        }

        // ── Error recovery ───────────────────────────────────────
        HirExprKind::Error => void_temp(fb, expr),
    }
}

/// Lowers an HIR expression as a place (l-value) for assignment targets.
///
/// Most expressions aren't valid l-values; for those we fall back to
/// lowering as an expression and returning a place for the resulting local.
pub fn lower_place(
    expr: &HirExpr,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) -> Place {
    match &expr.kind {
        HirExprKind::Local(def_id) => {
            let local = fb.lookup_def(*def_id).unwrap_or_else(|| {
                lower_expr(expr, fb, mir, type_table, defs)
            });
            Place::from_local(local)
        }

        HirExprKind::FieldAccess { base, field } => {
            let mut place = lower_place(base, fb, mir, type_table, defs);
            place.projections.push(Projection::Field(*field));
            place
        }

        HirExprKind::Index { base, index } => {
            let mut place = lower_place(base, fb, mir, type_table, defs);
            let index_local = lower_expr(index, fb, mir, type_table, defs);
            place.projections.push(Projection::Index(index_local));
            place
        }

        HirExprKind::Deref(inner) => {
            let mut place = lower_place(inner, fb, mir, type_table, defs);
            place.projections.push(Projection::Deref);
            place
        }

        _ => {
            let local = lower_expr(expr, fb, mir, type_table, defs);
            Place::from_local(local)
        }
    }
}

// ── String interpolation ─────────────────────────────────────────────────

/// Lowers a `StringInterp` HIR node into a `Rvalue::StringInterp`.
///
/// Each part is lowered to a local. The resulting rvalue carries all parts
/// as operands; the codegen is responsible for emitting to-string conversions
/// and the concat chain.
fn lower_string_interp(
    parts: &[HirExpr],
    expr: &HirExpr,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) -> Local {
    let operands: Vec<Operand> = parts
        .iter()
        .map(|part| {
            let local = lower_expr(part, fb, mir, type_table, defs);
            Operand::Copy(Place::from_local(local))
        })
        .collect();

    let temp = fb.fresh_temp(expr.ty, expr.span);
    fb.push_assign(
        Place::from_local(temp),
        Rvalue::StringInterp(operands),
        expr.span,
    );
    temp
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Lowers a literal into a constant operand assigned to a fresh temp.
fn lower_literal(lit: &HirLiteral, expr: &HirExpr, fb: &mut FunctionBuilder) -> Local {
    let constant = match lit {
        HirLiteral::Int(v) => Constant::Int(*v),
        HirLiteral::Float(v) => Constant::Float(*v),
        HirLiteral::Bool(v) => Constant::Bool(*v),
        HirLiteral::String(s) => Constant::String(*s),
        HirLiteral::Char(c) => Constant::Char(*c),
        HirLiteral::Null => Constant::Null,
    };
    let temp = fb.fresh_temp(expr.ty, expr.span);
    fb.push_assign(
        Place::from_local(temp),
        Rvalue::Use(Operand::Constant(constant)),
        expr.span,
    );
    temp
}

/// Lowers a function call (Call terminator + continuation block).
fn lower_call(
    callee: &HirExpr,
    args: &[HirExpr],
    expr: &HirExpr,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) -> Local {
    let func_local = lower_expr(callee, fb, mir, type_table, defs);
    let arg_operands: Vec<Operand> = args
        .iter()
        .map(|a| {
            let local = lower_expr(a, fb, mir, type_table, defs);
            Operand::Copy(Place::from_local(local))
        })
        .collect();

    let dest = fb.fresh_temp(expr.ty, expr.span);
    let cont = fb.new_block();

    fb.terminate(Terminator::Call {
        func: Operand::Copy(Place::from_local(func_local)),
        args: arg_operands,
        destination: Place::from_local(dest),
        target: cont,
        span: expr.span,
    });

    fb.switch_to(cont);
    dest
}

/// Lowers a method call. Method calls are desugared into regular calls
/// with the receiver as the first argument.
#[allow(clippy::too_many_arguments)]
fn lower_method_call(
    receiver: &HirExpr,
    method: Symbol,
    args: &[HirExpr],
    expr: &HirExpr,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) -> Local {
    let receiver_local = lower_expr(receiver, fb, mir, type_table, defs);
    let mut all_args = vec![Operand::Copy(Place::from_local(receiver_local))];
    for a in args {
        let local = lower_expr(a, fb, mir, type_table, defs);
        all_args.push(Operand::Copy(Place::from_local(local)));
    }

    // Look up the method DefId: first try builtins, then user-defined methods.
    let method_def_id = find_builtin_method(defs, method)
        .or_else(|| type_table.get_method_resolution(expr.span))
        .unwrap_or(spark_resolve::DefId::DUMMY);

    let dest = fb.fresh_temp(expr.ty, expr.span);
    let cont = fb.new_block();

    // Use a constant FunctionRef directly in the Call terminator to avoid
    // creating a temp with the wrong type (the expression type vs pointer type).
    fb.terminate(Terminator::Call {
        func: Operand::Constant(Constant::FunctionRef(method_def_id)),
        args: all_args,
        destination: Place::from_local(dest),
        target: cont,
        span: expr.span,
    });

    fb.switch_to(cont);
    dest
}

/// Lowers a collection literal (List, Set, Tuple).
fn lower_collection(
    elems: &[HirExpr],
    kind: AggregateKind,
    expr: &HirExpr,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) -> Local {
    let operands: Vec<Operand> = elems
        .iter()
        .map(|e| {
            let local = lower_expr(e, fb, mir, type_table, defs);
            Operand::Copy(Place::from_local(local))
        })
        .collect();
    let temp = fb.fresh_temp(expr.ty, expr.span);
    fb.push_assign(
        Place::from_local(temp),
        Rvalue::Aggregate(kind, operands),
        expr.span,
    );
    temp
}

/// Lowers an if-else expression.
///
/// ```text
/// current_block:
///   cond = ...
///   SwitchInt(cond) -> [true: then_bb, false: else_bb]
///
/// then_bb:
///   then_val = ...
///   result = then_val
///   Goto -> join_bb
///
/// else_bb:
///   else_val = ...
///   result = else_val
///   Goto -> join_bb
///
/// join_bb:
///   (continues)
/// ```
#[allow(clippy::too_many_arguments)]
fn lower_if(
    cond: &HirExpr,
    then_: &HirExpr,
    else_: Option<&HirExpr>,
    expr: &HirExpr,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) -> Local {
    let cond_local = lower_expr(cond, fb, mir, type_table, defs);

    let then_bb = fb.new_block();
    let else_bb = fb.new_block();
    let join_bb = fb.new_block();

    let result = fb.fresh_temp(expr.ty, expr.span);

    // Branch on condition
    fb.terminate(Terminator::SwitchInt {
        discriminant: Operand::Copy(Place::from_local(cond_local)),
        targets: vec![(1, then_bb)], // true = 1
        otherwise: else_bb,
    });

    // Then branch
    fb.switch_to(then_bb);
    let then_val = lower_expr(then_, fb, mir, type_table, defs);
    if !fb.is_terminated() {
        fb.push_assign(
            Place::from_local(result),
            Rvalue::Use(Operand::Copy(Place::from_local(then_val))),
            then_.span,
        );
        fb.terminate(Terminator::Goto { target: join_bb });
    }

    // Else branch
    fb.switch_to(else_bb);
    if let Some(else_expr) = else_ {
        let else_val = lower_expr(else_expr, fb, mir, type_table, defs);
        if !fb.is_terminated() {
            fb.push_assign(
                Place::from_local(result),
                Rvalue::Use(Operand::Copy(Place::from_local(else_val))),
                else_expr.span,
            );
            fb.terminate(Terminator::Goto { target: join_bb });
        }
    } else {
        // No else branch: result is Void
        fb.push_assign(
            Place::from_local(result),
            Rvalue::Use(Operand::Constant(Constant::Void)),
            expr.span,
        );
        fb.terminate(Terminator::Goto { target: join_bb });
    }

    fb.switch_to(join_bb);
    result
}

/// Lowers an `is` type test expression.
///
/// Emits a discriminant check for enum, union, and optional types, branching
/// to assign `Bool(true)` or `Bool(false)`. For types that can be resolved
/// at compile time (e.g. `42 is Int`), emits a constant directly.
///
/// ```text
/// current_bb:
///   disc = Discriminant(inner)
///   SwitchInt(disc) -> [tag: true_bb, otherwise: false_bb]
///
/// true_bb:
///   result = true
///   Goto -> join_bb
///
/// false_bb:
///   result = false
///   Goto -> join_bb
///
/// join_bb:
///   (continues)
/// ```
#[allow(clippy::too_many_arguments)]
fn lower_is_expr(
    inner: &HirExpr,
    target_ty: TypeId,
    expr: &HirExpr,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) -> Local {
    let inner_local = lower_expr(inner, fb, mir, type_table, defs);
    let result = fb.fresh_temp(expr.ty, expr.span);

    // If either type is Error, return false without branching
    if inner.ty.is_error() || target_ty.is_error() {
        fb.push_assign(
            Place::from_local(result),
            Rvalue::Use(Operand::Constant(Constant::Bool(false))),
            expr.span,
        );
        return result;
    }

    let inner_ty = type_table.resolve(inner.ty);

    match inner_ty {
        // Enum: check discriminant against target variant's DefId
        Ty::Enum(enum_def_id) => {
            // The target type should also be this enum (user wrote `x is Variant`)
            // Find which variant DefId matches the target_ty
            if let Some(variants) = type_table.get_enum_variants(*enum_def_id) {
                // Look for a variant whose type matches target_ty
                let target_resolved = type_table.resolve(target_ty);
                let matching_variant = if let Ty::Enum(target_def) = target_resolved {
                    // target_ty is the enum itself — always true
                    if *target_def == *enum_def_id {
                        // `x is EnumType` — always true since x is already that enum
                        fb.push_assign(
                            Place::from_local(result),
                            Rvalue::Use(Operand::Constant(Constant::Bool(true))),
                            expr.span,
                        );
                        return result;
                    }
                    // target is a specific variant — find it
                    variants.iter().find(|(_, vid)| {
                        type_table.get_def_type(*vid) == Some(target_ty)
                    })
                } else {
                    // Target might be found via variant DefId matching
                    variants.iter().find(|(_, vid)| {
                        type_table.get_def_type(*vid) == Some(target_ty)
                    })
                };

                if let Some((_, variant_def_id)) = matching_variant {
                    emit_discriminant_check(inner_local, i128::from(variant_def_id.as_u32()), result, expr, fb);
                } else {
                    // No matching variant — always false
                    fb.push_assign(
                        Place::from_local(result),
                        Rvalue::Use(Operand::Constant(Constant::Bool(false))),
                        expr.span,
                    );
                }
            } else {
                fb.push_assign(
                    Place::from_local(result),
                    Rvalue::Use(Operand::Constant(Constant::Bool(false))),
                    expr.span,
                );
            }
        }

        // Union: check discriminant against the target member's index
        Ty::Union(members) => {
            if let Some(idx) = members.iter().position(|&m| m == target_ty) {
                emit_discriminant_check(inner_local, idx as i128, result, expr, fb);
            } else {
                // Target type is not a member — always false
                fb.push_assign(
                    Place::from_local(result),
                    Rvalue::Use(Operand::Constant(Constant::Bool(false))),
                    expr.span,
                );
            }
        }

        // Optional: discriminant 0 = Null, 1 = value
        Ty::Optional(inner_type) => {
            let target_resolved = type_table.resolve(target_ty);
            if matches!(target_resolved, Ty::Null) {
                // `x is Null` — check discriminant == 0
                emit_discriminant_check(inner_local, 0, result, expr, fb);
            } else if target_ty == *inner_type {
                // `x is T` where T is the inner type — check discriminant == 1
                emit_discriminant_check(inner_local, 1, result, expr, fb);
            } else {
                // Unknown target — always false
                fb.push_assign(
                    Place::from_local(result),
                    Rvalue::Use(Operand::Constant(Constant::Bool(false))),
                    expr.span,
                );
            }
        }

        // Same type known at compile time — always true or false
        _ => {
            let same = inner.ty == target_ty;
            fb.push_assign(
                Place::from_local(result),
                Rvalue::Use(Operand::Constant(Constant::Bool(same))),
                expr.span,
            );
        }
    }

    result
}

/// Emits a discriminant check: extracts discriminant from `scrut`, compares
/// against `expected_tag`, and assigns `true` or `false` to `result`.
fn emit_discriminant_check(
    scrut: Local,
    expected_tag: i128,
    result: Local,
    expr: &HirExpr,
    fb: &mut FunctionBuilder,
) {
    let disc_temp = fb.fresh_temp(TypeId::ERROR, expr.span);
    fb.push_assign(
        Place::from_local(disc_temp),
        Rvalue::Discriminant(Place::from_local(scrut)),
        expr.span,
    );

    let true_bb = fb.new_block();
    let false_bb = fb.new_block();
    let join_bb = fb.new_block();

    fb.terminate(Terminator::SwitchInt {
        discriminant: Operand::Copy(Place::from_local(disc_temp)),
        targets: vec![(expected_tag, true_bb)],
        otherwise: false_bb,
    });

    fb.switch_to(true_bb);
    fb.push_assign(
        Place::from_local(result),
        Rvalue::Use(Operand::Constant(Constant::Bool(true))),
        expr.span,
    );
    fb.terminate(Terminator::Goto { target: join_bb });

    fb.switch_to(false_bb);
    fb.push_assign(
        Place::from_local(result),
        Rvalue::Use(Operand::Constant(Constant::Bool(false))),
        expr.span,
    );
    fb.terminate(Terminator::Goto { target: join_bb });

    fb.switch_to(join_bb);
}

/// Lowers a loop expression.
///
/// ```text
/// header_bb:
///   body = ...
///   Goto -> header_bb     (implicit continue)
///
/// break_bb:
///   result = break_value
///   (continues)
/// ```
fn lower_loop(
    body: &HirExpr,
    expr: &HirExpr,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) -> Local {
    let header_bb = fb.new_block();
    let break_bb = fb.new_block();
    let break_val = fb.fresh_temp(expr.ty, expr.span);

    // Jump from current block into header
    fb.terminate(Terminator::Goto { target: header_bb });

    let scope_depth = fb.scope_depth();
    fb.push_loop(LoopContext {
        continue_block: header_bb,
        break_block: break_bb,
        break_value: Some(break_val),
        scope_depth,
    });

    // Lower body in header block
    fb.switch_to(header_bb);
    let _ = lower_expr(body, fb, mir, type_table, defs);
    // If body didn't terminate, loop back
    if !fb.is_terminated() {
        fb.terminate(Terminator::Goto { target: header_bb });
    }

    fb.pop_loop();
    fb.switch_to(break_bb);
    break_val
}

/// Lowers a break expression.
fn lower_break(
    value: Option<&HirExpr>,
    expr: &HirExpr,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) -> Local {
    if let Some(ctx) = fb.current_loop() {
        let break_block = ctx.break_block;
        let break_local = ctx.break_value;

        if let Some(val_expr) = value {
            let val = lower_expr(val_expr, fb, mir, type_table, defs);
            if let Some(dest) = break_local {
                fb.push_assign(
                    Place::from_local(dest),
                    Rvalue::Use(Operand::Copy(Place::from_local(val))),
                    val_expr.span,
                );
            }
            // Drop locals in scopes above the loop before jumping out.
            fb.drops_to_loop(break_local, expr.span);
        } else {
            if let Some(dest) = break_local {
                fb.push_assign(
                    Place::from_local(dest),
                    Rvalue::Use(Operand::Constant(Constant::Void)),
                    expr.span,
                );
            }
            fb.drops_to_loop(None, expr.span);
        }

        fb.terminate(Terminator::Goto {
            target: break_block,
        });
    }

    // Return a void temp (code after break is unreachable)
    let temp = fb.fresh_temp(expr.ty, expr.span);
    fb.push_assign(
        Place::from_local(temp),
        Rvalue::Use(Operand::Constant(Constant::Void)),
        expr.span,
    );
    temp
}

/// Lowers a return expression.
fn lower_return(
    value: Option<&HirExpr>,
    expr: &HirExpr,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) -> Local {
    let val_local = if let Some(val_expr) = value {
        let val = lower_expr(val_expr, fb, mir, type_table, defs);
        fb.push_assign(
            Place::from_local(Local::RETURN_PLACE),
            Rvalue::Use(Operand::Copy(Place::from_local(val))),
            val_expr.span,
        );
        Some(val)
    } else {
        fb.push_assign(
            Place::from_local(Local::RETURN_PLACE),
            Rvalue::Use(Operand::Constant(Constant::Void)),
            expr.span,
        );
        None
    };
    // Drop all locals in all scopes before returning.
    fb.drops_for_return(val_local, expr.span);
    fb.terminate(Terminator::Return);

    // Start a new dead block so subsequent code (if any) doesn't corrupt
    // the current block's terminator
    let dead = fb.new_block();
    fb.switch_to(dead);

    // Return void temp (unreachable)
    let temp = fb.fresh_temp(expr.ty, expr.span);
    fb.push_assign(
        Place::from_local(temp),
        Rvalue::Use(Operand::Constant(Constant::Void)),
        expr.span,
    );
    temp
}

/// Emits a diagnostic for a deferred feature.
fn emit_deferred(mir: &mut MirBuilder<'_>, feature: &str, expr: &HirExpr) {
    mir.diagnostics.push(
        Diagnostic::warning(format!("{feature} not yet supported in MIR lowering"))
            .with_label(Label::primary(expr.span, "unsupported")),
    );
}

/// Returns a void-valued temp (for error/deferred nodes).
fn void_temp(fb: &mut FunctionBuilder, expr: &HirExpr) -> Local {
    let temp = fb.fresh_temp(expr.ty, expr.span);
    fb.push_assign(
        Place::from_local(temp),
        Rvalue::Use(Operand::Constant(Constant::Void)),
        expr.span,
    );
    temp
}

/// Finds the `DefId` of a built-in method by name.
fn find_builtin_method(defs: &DefinitionTable, name: Symbol) -> Option<spark_resolve::DefId> {
    defs.iter()
        .find(|(_, info)| matches!(info.kind, DefinitionKind::BuiltinMethod) && info.name == name)
        .map(|(id, _)| id)
}
