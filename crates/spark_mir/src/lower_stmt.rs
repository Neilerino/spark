// TAG [SPARK-154]

//! Statement lowering: HIR statements → MIR statements.

use spark_diagnostics::{Diagnostic, Label};
use spark_hir::{HirExpr, HirStmt};
use spark_resolve::DefinitionTable;
use spark_types::ty::Ty;
use spark_types::{TypeId, TypeTable};

use crate::builder::{FunctionBuilder, MirBuilder};
use crate::lower_expr::{
    emit_assign_with_union_wrap, lower_expr, lower_place, stage_collection_value,
};
use crate::mir::{Local, Operand, Place, Rvalue, Statement, Terminator};

/// Strips `Mutable(T)` wrappers, returning the inner type.
fn strip_mutable(ty: TypeId, table: &TypeTable) -> TypeId {
    match table.resolve(ty) {
        Ty::Mutable(inner) => strip_mutable(*inner, table),
        _ => ty,
    }
}

/// If `def_id` has a declared Optional or Union type and `value_ty` is a
/// member, returns the declared type. This ensures the MIR local gets the
/// correct tagged-union layout for discriminant checks.
fn declared_union_type(
    def_id: spark_resolve::DefId,
    value_ty: TypeId,
    table: &TypeTable,
) -> Option<TypeId> {
    let dt = strip_mutable(table.get_def_type(def_id)?, table);
    match table.resolve(dt) {
        Ty::Optional(inner)
            if *inner == value_ty || matches!(table.resolve(value_ty), Ty::Null) =>
        {
            Some(dt)
        }
        Ty::Union(members) if members.contains(&value_ty) => Some(dt),
        _ => None,
    }
}

/// Lowers a single HIR statement, emitting MIR into the current block.
pub fn lower_stmt(
    stmt: &HirStmt,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) {
    match stmt {
        HirStmt::Let {
            def_id,
            mutable,
            value,
        } => {
            let val_local = lower_expr(value, fb, mir, type_table, defs);

            // For Optional bindings (`x: Int? = 42`), use the declared
            // Optional type so the local gets the correct layout for `is`
            // discriminant checks. Falls back to the RHS expression type.
            let effective_ty =
                declared_union_type(*def_id, value.ty, type_table).unwrap_or(value.ty);

            // Allocate a local for this binding
            let name = defs.get(*def_id).map(|info| info.name);
            let local = match name {
                Some(sym) => fb.fresh_local(effective_ty, sym, *mutable, value.span),
                None => fb.fresh_temp(effective_ty, value.span),
            };
            fb.register_def(*def_id, local);

            // If the effective type matches the value type, use a simple
            // copy/move. Otherwise (Optional wrapping), use union wrapping.
            if effective_ty == value.ty {
                // Use Move for temporaries (avoids RC leak: Copy would
                // increment the refcount with no corresponding drop for
                // the temp).  Use Copy for named bindings because
                // lower_expr(Local) returns the *original* local.
                let operand = if fb.is_def_local(val_local) {
                    Operand::Copy(Place::from_local(val_local))
                } else {
                    Operand::Move(Place::from_local(val_local))
                };
                fb.push_stmt(Statement::Assign {
                    place: Place::from_local(local),
                    rvalue: Rvalue::Use(operand),
                    span: value.span,
                });
            } else {
                emit_assign_with_union_wrap(fb, local, val_local, type_table, value.span);
            }

            // Register for drop if the type is reference-counted.
            if spark_drop::needs_drop(effective_ty, type_table) {
                fb.register_drop(local);
            }
        }

        HirStmt::Assign { target, value } => {
            lower_assignment(target, value, fb, mir, type_table, defs);
        }

        HirStmt::IndexedAssign {
            root,
            indices,
            slot_ty,
            value,
        } => {
            fb.push_scope();
            if let Some(staged) =
                lower_store_operand(value, Some(*slot_ty), fb, mir, type_table, defs)
            {
                let indices: Option<Vec<_>> = indices
                    .iter()
                    .map(|index| lower_store_operand(index, None, fb, mir, type_table, defs))
                    .collect();
                if let Some(indices) = indices {
                    let root = lower_expr(root, fb, mir, type_table, defs);
                    fb.push_stmt(Statement::IndexedAssign {
                        root: Place::from_local(root),
                        indices,
                        value: staged,
                        span: value.span,
                    });
                    fb.pop_finished_scope();
                    return;
                }
            }
            if fb.is_terminated() {
                fb.pop_finished_scope();
            } else {
                fb.pop_scope_drops(None, value.span);
            }
        }

        HirStmt::Expr(expr) => {
            // Evaluate for side effects; discard result.
            let result = lower_expr(expr, fb, mir, type_table, defs);
            // If the discarded result is RC'd, drop it immediately.
            if !fb.is_def_local(result) && spark_drop::needs_drop(expr.ty, type_table) {
                fb.push_stmt(Statement::Drop {
                    place: Place::from_local(result),
                    span: expr.span,
                });
            }
        }
    }
}

fn lower_assignment(
    target: &HirExpr,
    value: &HirExpr,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) {
    // TAG [SPARK-129]: stage the new owner before releasing an aliased destination.
    fb.push_scope();
    let val_local = lower_expr(value, fb, mir, type_table, defs);
    if fb.is_terminated() || matches!(type_table.resolve(value.ty), Ty::Never) {
        if !fb.is_terminated() {
            fb.terminate(Terminator::Goto {
                target: fb.current_block(),
            });
        }
        fb.pop_finished_scope();
        return;
    }
    let place = lower_place(target, fb, mir, type_table, defs);
    if fb.is_terminated() {
        fb.pop_finished_scope();
        return;
    }
    let dest_ty = if place.projections.is_empty() {
        if place.local == val_local {
            fb.pop_finished_scope();
            return;
        }
        fb.local_type(place.local)
    } else {
        target.ty
    };
    let Some(staged) = stage_collection_value(fb, val_local, dest_ty, value.span, type_table)
    else {
        mir.diagnostics.push(
            Diagnostic::error("assignment has incompatible storage after lowering").with_label(
                Label::primary(value.span, "value does not match the destination"),
            ),
        );
        fb.pop_scope_drops(None, value.span);
        return;
    };
    if spark_drop::needs_drop(dest_ty, type_table) {
        fb.push_stmt(Statement::Drop {
            place: place.clone(),
            span: value.span,
        });
    }
    fb.push_stmt(Statement::Assign {
        place,
        rvalue: Rvalue::Use(Operand::Move(Place::from_local(staged))),
        span: value.span,
    });
    fb.pop_finished_scope();
}

fn lower_store_operand(
    expr: &HirExpr,
    slot_ty: Option<TypeId>,
    fb: &mut FunctionBuilder,
    mir: &mut MirBuilder<'_>,
    type_table: &TypeTable,
    defs: &DefinitionTable,
) -> Option<Local> {
    let source = lower_expr(expr, fb, mir, type_table, defs);
    if fb.is_terminated() {
        return None;
    }
    if matches!(type_table.resolve(expr.ty), Ty::Never) {
        fb.terminate(Terminator::Goto {
            target: fb.current_block(),
        });
        return None;
    }
    let slot_ty = slot_ty.unwrap_or_else(|| strip_mutable(fb.local_type(source), type_table));
    let staged = stage_collection_value(fb, source, slot_ty, expr.span, type_table);
    if staged.is_none() {
        mir.diagnostics.push(
            Diagnostic::error("indexed assignment operand has incompatible storage after lowering")
                .with_label(Label::primary(
                    expr.span,
                    "operand does not match the collection slot",
                )),
        );
    }
    staged
}
