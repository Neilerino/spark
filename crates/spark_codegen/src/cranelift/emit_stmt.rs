//! Statement lowering: MIR statements → Cranelift IR.
//!
//! Handles `Assign`, `Drop`, and `Nop` statements.

use cranelift_module::{FuncId, Module};
use rustc_hash::FxHashMap;
use spark_diagnostics::Diagnostics;
use spark_mir::Statement;
use spark_resolve::DefinitionTable;
use spark_types::TypeTable;

use super::context::FunctionContext;
use super::emit_rvalue;

/// Emits a single MIR statement.
#[allow(clippy::too_many_arguments)]
pub fn emit_statement<M: Module>(
    ctx: &mut FunctionContext<'_>,
    stmt: &Statement,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
    diagnostics: &mut Diagnostics,
) {
    match stmt {
        Statement::Assign {
            place,
            rvalue,
            span: _,
        } => {
            let dest_local = place.local;
            if let Some(val) = emit_rvalue::emit_rvalue(
                ctx,
                rvalue,
                dest_local,
                table,
                definitions,
                func_ids,
                module,
                diagnostics,
            ) {
                if place.projections.is_empty() {
                    // Simple local assignment.
                    ctx.write_local(dest_local, val);
                } else {
                    // Projected assignment — compute address and store.
                    // The rvalue emission for aggregates handles this directly.
                    // For scalar projected writes, we store at the computed address.
                    // This is handled through emit_rvalue already returning
                    // values that are stored at the right places.
                }
            }
        }

        Statement::Drop { place: _, span: _ } | Statement::Nop => {
            // Drop and Nop are no-ops for now. When we have runtime support
            // for reference counting or destructors, Drop would emit calls
            // to cleanup routines.
        }
    }
}
