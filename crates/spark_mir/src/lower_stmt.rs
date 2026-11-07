//! Statement lowering: HIR statements → MIR statements.

use spark_hir::HirStmt;
use spark_resolve::DefinitionTable;
use spark_types::ty::Ty;
use spark_types::TypeTable;

use crate::builder::{FunctionBuilder, MirBuilder};
use crate::lower_expr::{emit_assign_with_union_wrap, lower_expr, lower_place};
use crate::mir::{Operand, Place, Rvalue, Statement};

/// Strips `Mutable(T)` wrappers, returning the inner type.
fn strip_mutable(ty: spark_types::ty::TypeId, table: &TypeTable) -> spark_types::ty::TypeId {
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
    value_ty: spark_types::ty::TypeId,
    table: &TypeTable,
) -> Option<spark_types::ty::TypeId> {
    let dt = strip_mutable(table.get_def_type(def_id)?, table);
    match table.resolve(dt) {
        Ty::Optional(inner) => {
            if *inner == value_ty || matches!(table.resolve(value_ty), Ty::Null) {
                return Some(dt);
            }
        }
        Ty::Union(members) => {
            if members.contains(&value_ty) {
                return Some(dt);
            }
        }
        _ => {}
    }
    None
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
            let val_local = lower_expr(value, fb, mir, type_table, defs);
            let place = lower_place(target, fb, mir, type_table, defs);

            // Check if the destination needs union/optional wrapping.
            // For simple local targets, compare the local's type with the
            // value's type — if they differ (e.g. assigning `Int` to `Int?!`),
            // use union wrapping just like Let bindings do.
            let dest_ty = if place.projections.is_empty() {
                Some(fb.local_type(place.local))
            } else {
                None
            };
            if let Some(dest_ty) = dest_ty {
                if place.local == val_local {
                    return;
                }
                if spark_drop::needs_drop(dest_ty, type_table) {
                    fb.push_stmt(Statement::Drop {
                        place: place.clone(),
                        span: value.span,
                    });
                }
            }
            let needs_wrap = dest_ty.is_some_and(|dt| {
                let dt_stripped = strip_mutable(dt, type_table);
                let val_ty = value.ty;
                dt_stripped != val_ty
                    && (matches!(
                        type_table.resolve(dt_stripped),
                        Ty::Optional(_) | Ty::Union(_)
                    ))
            });

            if needs_wrap {
                emit_assign_with_union_wrap(fb, place.local, val_local, type_table, value.span);
            } else {
                let operand = if fb.is_def_local(val_local) {
                    Operand::Copy(Place::from_local(val_local))
                } else {
                    Operand::Move(Place::from_local(val_local))
                };
                fb.push_stmt(Statement::Assign {
                    place,
                    rvalue: Rvalue::Use(operand),
                    span: value.span,
                });
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
