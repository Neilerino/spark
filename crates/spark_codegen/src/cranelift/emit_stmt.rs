//! Statement lowering: MIR statements → Cranelift IR.

use cranelift_codegen::ir::{types, InstBuilder, MemFlags};
use cranelift_module::Module;
use spark_diagnostics::Diagnostics;
use spark_mir::{Local, Operand, Place, Statement};
use spark_types::ty::Ty;
use spark_types::TypeTable;

use super::clif_types;
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
                && matches!(
                    rvalue,
                    spark_mir::Rvalue::Use(
                        spark_mir::Operand::Constant(spark_mir::Constant::Null,)
                    )
                )
            {
                if let LocalStorage::StackSlot(slot, size) = ctx.local_storage(dest_local) {
                    let dest_addr = ctx.builder.ins().stack_addr(ctx.ptr_type, slot, 0);
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

            if let Some(val) =
                emit_rvalue::emit_rvalue(ctx, rvalue, dest_local, ecx, module, diagnostics)
            {
                if place.projections.is_empty() {
                    // Check if the destination is a stack slot (aggregate).
                    // For aggregates, `val` is a pointer to the source data
                    // and we need to memcpy it into the destination slot.
                    if let LocalStorage::StackSlot(slot, size) = ctx.local_storage(dest_local) {
                        let dest_addr = ctx.builder.ins().stack_addr(ctx.ptr_type, slot, 0);
                        let config = module.target_config();
                        ctx.builder.emit_small_memory_copy(
                            config,
                            dest_addr,
                            val,
                            u64::from(size),
                            1,    // dest align
                            1,    // src align
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

        Statement::IndexedAssign {
            root,
            indices,
            value,
            ..
        } => {
            emit_indexed_assign(ctx, *root, indices, *value, ecx, module);
        }

        Statement::Drop { place, span: _ } => {
            emit_drop(ctx, place, table, module);
        }

        Statement::Nop => {}
    }
}

fn emit_indexed_assign<M: Module>(
    ctx: &mut FunctionContext<'_>,
    root: Local,
    indices: &[Local],
    value: Local,
    ecx: &EmitCtx<'_>,
    module: &mut M,
) {
    let original = ctx.read_local(root);
    let indices: Vec<_> = indices
        .iter()
        .map(|&local| {
            let index = ctx.read_local(local);
            let (_, Ty::Int(width)) =
                clif_types::unwrap_mutable(ctx.local_type_id(local), ecx.table)
            else {
                unreachable!("list write requires an integer index");
            };
            let signed = crate::abi::int_is_signed(*width);
            let index = if ctx.builder.func.dfg.value_type(index) == types::I64 {
                index
            } else if signed {
                ctx.builder.ins().sextend(types::I64, index)
            } else {
                ctx.builder.ins().uextend(types::I64, index)
            };
            let signed = ctx.builder.ins().iconst(types::I8, i64::from(signed));
            (index, signed)
        })
        .collect();

    let checked_slot = module.declare_func_in_func(ctx.stubs.list_write_slot, ctx.builder.func);
    let mut list = original;
    for (offset, &(index, signed)) in indices.iter().enumerate() {
        let call = ctx.builder.ins().call(checked_slot, &[list, index, signed]);
        if offset + 1 < indices.len() {
            list = ctx.load(ctx.ptr_type, ctx.builder.inst_results(call)[0], 0);
        }
    }

    let cow = module.declare_func_in_func(ctx.stubs.list_cow, ctx.builder.func);
    let mut owner = None;
    list = original;
    for (offset, &(index, signed)) in indices.iter().enumerate() {
        let call = ctx.builder.ins().call(cow, &[list]);
        list = ctx.builder.inst_results(call)[0];
        if let Some(slot) = owner {
            ctx.builder.ins().store(MemFlags::new(), list, slot, 0);
        } else {
            ctx.write_local(root, list);
        }

        let call = ctx.builder.ins().call(checked_slot, &[list, index, signed]);
        let slot = ctx.builder.inst_results(call)[0];
        if offset + 1 == indices.len() {
            let operand = Operand::Move(Place::from_local(value));
            let value_ty = ctx.local_type_id(value);
            let address = emit_rvalue::emit_operand_addr(ctx, &operand, value_ty, ecx, module)
                .unwrap_or_else(|| ctx.builder.ins().iconst(ctx.ptr_type, 0));
            let replace = module.declare_func_in_func(ctx.stubs.list_replace, ctx.builder.func);
            ctx.builder.ins().call(replace, &[list, slot, address]);
        } else {
            owner = Some(slot);
            list = ctx.load(ctx.ptr_type, slot, 0);
        }
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
