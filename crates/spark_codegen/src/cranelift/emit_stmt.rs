//! Statement lowering: MIR statements → Cranelift IR.
//!
//! Handles `Assign`, `Drop`, and `Nop` statements.

use cranelift_codegen::ir::{types, InstBuilder, MemFlags};
use cranelift_module::{FuncId, Module};
use rustc_hash::FxHashMap;
use spark_diagnostics::Diagnostics;
use spark_mir::Statement;
use spark_resolve::DefinitionTable;
use spark_types::{Ty, TypeTable};

use super::context::{FunctionContext, LocalStorage};
use super::emit_rvalue;
use super::types as clif_types;

/// Emits a single MIR statement.
#[allow(clippy::too_many_arguments)]
pub fn emit_statement<M: Module>(
    ctx: &mut FunctionContext<'_>,
    stmt: &Statement,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
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
                thunk_ids,
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
                    let dest_addr = emit_rvalue::emit_place_addr(ctx, place, table, module);
                    let place_ty = emit_rvalue::resolve_place_type(ctx, place, table);
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
    let (_, ty) = clif_types::unwrap_mutable(ty_id, table);

    // Only droppable types need a dec call.
    if !spark_drop::needs_drop(ty_id, table) {
        return;
    }

    let ptr_val = ctx.read_local(local);

    match ty {
        Ty::String => {
            let func_ref = module.declare_func_in_func(ctx.stubs.string_rc_dec, ctx.builder.func);
            ctx.builder.ins().call(func_ref, &[ptr_val]);
        }
        Ty::List(elem) => {
            let elem_rc_flag = i64::from(spark_drop::needs_rc(*elem, table));
            let elem_rc_val = ctx.builder.ins().iconst(types::I8, elem_rc_flag);
            let func_ref = module.declare_func_in_func(ctx.stubs.list_rc_dec, ctx.builder.func);
            ctx.builder.ins().call(func_ref, &[ptr_val, elem_rc_val]);
        }
        Ty::Map(k, v) => {
            let key_rc_flag = i64::from(spark_drop::needs_rc(*k, table));
            let val_rc_flag = i64::from(spark_drop::needs_rc(*v, table));
            let key_rc_val = ctx.builder.ins().iconst(types::I8, key_rc_flag);
            let val_rc_val = ctx.builder.ins().iconst(types::I8, val_rc_flag);
            let func_ref = module.declare_func_in_func(ctx.stubs.map_rc_dec, ctx.builder.func);
            ctx.builder
                .ins()
                .call(func_ref, &[ptr_val, key_rc_val, val_rc_val]);
        }
        Ty::Set(elem) => {
            let elem_rc_flag = i64::from(spark_drop::needs_rc(*elem, table));
            let elem_rc_val = ctx.builder.ins().iconst(types::I8, elem_rc_flag);
            let func_ref = module.declare_func_in_func(ctx.stubs.set_rc_dec, ctx.builder.func);
            ctx.builder.ins().call(func_ref, &[ptr_val, elem_rc_val]);
        }
        Ty::Function { .. } => {
            // Function fat pointer: (fn_ptr, env_ptr).
            // Load env_ptr from the second slot and call __spark_env_rc_dec.
            // ptr_val is the address of the fat pointer.
            let env_ptr = ctx.builder.ins().load(
                ctx.ptr_type,
                MemFlags::new(),
                ptr_val,
                ctx.ptr_type.bytes() as i32,
            );
            let func_ref = module.declare_func_in_func(ctx.stubs.env_rc_dec, ctx.builder.func);
            ctx.builder.ins().call(func_ref, &[env_ptr]);
        }
        _ => {
            // Non-droppable type — no-op (shouldn't reach here due to early return).
        }
    }
}
