//! Statement lowering: MIR statements → Cranelift IR.
//!
//! Handles `Assign`, `Drop`, and `Nop` statements.

use cranelift_codegen::ir::{InstBuilder, MemFlags};
use cranelift_module::Module;
use spark_diagnostics::Diagnostics;
use spark_mir::Statement;
use spark_types::TypeTable;

use super::context::{EmitCtx, FunctionContext, LocalStorage};
use super::drop_glue;
use super::emit_rvalue;

/// Emits a single MIR statement.
pub fn emit_statement<M: Module>(
    ctx: &mut FunctionContext<'_>,
    stmt: &Statement,
    ecx: &EmitCtx<'_>,
    module: &mut M,
    diagnostics: &mut Diagnostics,
) {
    let table = ecx.table;

    match stmt {
        Statement::Assign {
            place,
            rvalue,
            span: _,
        } => {
            let dest_local = place.local;

            // Special case: assigning Null to an aggregate StackSlot
            // (e.g. `result: T?! = Null`). Zero-fill the slot instead of
            // trying to memcpy from address 0 (which would segfault).
            if place.projections.is_empty()
                && matches!(rvalue, spark_mir::Rvalue::Use(spark_mir::Operand::Constant(
                    spark_mir::Constant::Null,
                )))
                {
                    if let LocalStorage::StackSlot(slot, size) = ctx.local_storage(dest_local) {
                        let dest_addr =
                            ctx.builder.ins().stack_addr(ctx.ptr_type, slot, 0);
                        ctx.builder.emit_small_memset(
                            module.target_config(),
                            dest_addr,
                            0_u8,
                            u64::from(size),
                            1,
                            MemFlags::new(),
                        );
                        return;
                    }
                }

            if let Some(val) = emit_rvalue::emit_rvalue(
                ctx,
                rvalue,
                dest_local,
                ecx,
                module,
                diagnostics,
            ) {
                if place.projections.is_empty() {
                    // Check if the destination is a stack slot (aggregate).
                    // For aggregates, `val` is a pointer to the source data
                    // and we need to memcpy it into the destination slot.
                    if let LocalStorage::StackSlot(slot, size) = ctx.local_storage(dest_local) {
                        let dest_addr =
                            ctx.builder.ins().stack_addr(ctx.ptr_type, slot, 0);
                        let config = module.target_config();
                        ctx.builder.emit_small_memory_copy(
                            config,
                            dest_addr,
                            val,
                            u64::from(size),
                            1, // dest align
                            1, // src align
                            true, // non-overlapping
                            MemFlags::new(),
                        );
                    } else {
                        // Scalar local assignment.
                        ctx.write_local(dest_local, val);
                    }
                } else {
                    // Projected assignment — compute address and store.
                    let (dest_addr, place_ty) =
                        emit_rvalue::emit_place_addr(ctx, place, table, module);
                    if crate::abi::is_scalar(place_ty, table) {
                        ctx.builder.ins().store(MemFlags::new(), val, dest_addr, 0);
                    } else {
                        // Aggregate projected write — memcpy.
                        let size = ctx.layouts.layout_of(place_ty, table).size;
                        let config = module.target_config();
                        ctx.builder.emit_small_memory_copy(
                            config,
                            dest_addr,
                            val,
                            u64::from(size),
                            1,
                            1,
                            true,
                            MemFlags::new(),
                        );
                    }
                }
            }
        }

        Statement::Drop { place, span: _ } => {
            emit_drop(ctx, place, table, module);
        }

        Statement::Nop => {}
    }
}

/// Emits an RC decrement for a dropped place, dispatched by type.
fn emit_drop<M: Module>(
    ctx: &mut FunctionContext<'_>,
    place: &spark_mir::Place,
    table: &TypeTable,
    module: &mut M,
) {
    let local = place.local;
    let ty_id = ctx.local_type_id(local);

    // Only droppable types need a dec call.
    if !spark_drop::needs_drop(ty_id, table) {
        return;
    }

    let ptr_val = ctx.read_local(local);
    let ptr_type = ctx.ptr_type;
    let stubs = ctx.stubs;

    drop_glue::emit_typed_drop(
        &mut ctx.builder,
        ptr_val,
        ty_id,
        table,
        stubs,
        module,
        ptr_type,
    );
}
