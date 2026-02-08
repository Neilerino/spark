//! Statement lowering: HIR statements → MIR statements.

use spark_hir::HirStmt;
use spark_resolve::DefinitionTable;
use spark_types::TypeTable;

use crate::builder::{FunctionBuilder, MirBuilder};
use crate::lower_expr::{lower_expr, lower_place};
use crate::mir::{Operand, Place, Rvalue, Statement};

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

            // Allocate a local for this binding
            let name = defs.get(*def_id).map(|info| info.name);
            let local = match name {
                Some(sym) => fb.fresh_local(value.ty, sym, *mutable, value.span),
                None => fb.fresh_temp(value.ty, value.span),
            };
            fb.register_def(*def_id, local);

            fb.push_stmt(Statement::Assign {
                place: Place::from_local(local),
                rvalue: Rvalue::Use(Operand::Copy(Place::from_local(val_local))),
                span: value.span,
            });

            // Register for drop if the type is reference-counted.
            if spark_drop::needs_rc(value.ty, type_table) {
                fb.register_drop(local);
            }
        }

        HirStmt::Assign { target, value } => {
            let val_local = lower_expr(value, fb, mir, type_table, defs);
            let place = lower_place(target, fb, mir, type_table, defs);

            // If reassigning a mutable RC variable, drop the old value first.
            if place.projections.is_empty()
                && spark_drop::needs_rc(value.ty, type_table)
            {
                fb.push_stmt(Statement::Drop {
                    place: place.clone(),
                    span: value.span,
                });
            }

            fb.push_stmt(Statement::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(Place::from_local(val_local))),
                span: value.span,
            });
        }

        HirStmt::Expr(expr) => {
            // Evaluate for side effects; discard result.
            let result = lower_expr(expr, fb, mir, type_table, defs);
            // If the discarded result is RC'd, drop it immediately.
            if spark_drop::needs_rc(expr.ty, type_table) {
                fb.push_stmt(Statement::Drop {
                    place: Place::from_local(result),
                    span: expr.span,
                });
            }
        }
    }
}
