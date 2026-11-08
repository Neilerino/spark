//! Statement lowering: MIR statements → Cranelift IR.

use cranelift_codegen::ir::{types, FuncRef, InstBuilder, MemFlags, Value};
use cranelift_module::Module;
use spark_diagnostics::{Diagnostic, Diagnostics, Label};
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
            span,
        } => {
            if let Err(message) = emit_indexed_assign(ctx, *root, indices, *value, ecx, module) {
                diagnostics.push(Diagnostic::error(message).with_label(Label::primary(
                    *span,
                    "the value type needs a stored representation",
                )));
            }
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
) -> Result<(), &'static str> {
    let (&terminal, prefix) = indices
        .split_last()
        .expect("indexed assignment has a selector");
    let mut collection_ty = ctx.local_type_id(root);
    for _ in prefix {
        let (_, Ty::List(element)) = clif_types::unwrap_mutable(collection_ty, ecx.table) else {
            unreachable!("only lists can precede the terminal write");
        };
        collection_ty = *element;
    }
    let terminal_is_map = matches!(
        clif_types::unwrap_mutable(collection_ty, ecx.table).1,
        Ty::Map(..)
    );
    let value_ty = ctx.local_type_id(value);
    if terminal_is_map && ctx.layouts.layout_of(value_ty, ecx.table).size == 0 {
        return Err("map value type has no storage");
    }
    let prefix: Vec<_> = prefix
        .iter()
        .map(|&local| list_write_index(ctx, local))
        .collect();
    let terminal_index = (!terminal_is_map).then(|| list_write_index(ctx, terminal));

    let original = ctx.read_local(root);
    let checked_slot = module.declare_func_in_func(ctx.stubs.list_write_slot, ctx.builder.func);

    // Validate the whole list path before detaching any owner.
    let mut collection = original;
    for &index in &prefix {
        let slot = list_write_slot(ctx, checked_slot, collection, index);
        collection = ctx.load(ctx.ptr_type, slot, 0);
    }
    if let Some(index) = terminal_index {
        list_write_slot(ctx, checked_slot, collection, index);
    }

    let list_cow = module.declare_func_in_func(ctx.stubs.list_cow, ctx.builder.func);
    let mut owner = None;
    collection = original;
    for index in prefix {
        let list = detach_collection(ctx, list_cow, root, owner, collection);
        let slot = list_write_slot(ctx, checked_slot, list, index);
        owner = Some(slot);
        collection = ctx.load(ctx.ptr_type, slot, 0);
    }

    let value_operand = Operand::Move(Place::from_local(value));
    let value_address = emit_rvalue::emit_operand_addr(ctx, &value_operand, value_ty, ecx, module)
        .unwrap_or_else(|| ctx.builder.ins().iconst(ctx.ptr_type, 0));
    if let Some(index) = terminal_index {
        let list = detach_collection(ctx, list_cow, root, owner, collection);
        let slot = list_write_slot(ctx, checked_slot, list, index);
        let replace = module.declare_func_in_func(ctx.stubs.list_replace, ctx.builder.func);
        ctx.builder
            .ins()
            .call(replace, &[list, slot, value_address]);
    } else {
        let key_operand = Operand::Move(Place::from_local(terminal));
        let key_ty = ctx.local_type_id(terminal);
        let key_address = emit_rvalue::emit_operand_addr(ctx, &key_operand, key_ty, ecx, module)
            .expect("validated map key has storage");
        let map_cow = module.declare_func_in_func(ctx.stubs.map_cow, ctx.builder.func);
        let map = detach_collection(ctx, map_cow, root, owner, collection);
        let insert = module.declare_func_in_func(ctx.stubs.map_insert, ctx.builder.func);
        ctx.builder
            .ins()
            .call(insert, &[map, key_address, value_address]);
    }
    Ok(())
}

fn list_write_index(ctx: &mut FunctionContext<'_>, local: Local) -> (Value, Value) {
    let index = ctx.read_local(local);
    let (_, Ty::Int(width)) = clif_types::unwrap_mutable(ctx.local_type_id(local), ctx.table)
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
}

fn list_write_slot(
    ctx: &mut FunctionContext<'_>,
    checked_slot: FuncRef,
    list: Value,
    (index, signed): (Value, Value),
) -> Value {
    let call = ctx.builder.ins().call(checked_slot, &[list, index, signed]);
    ctx.builder.inst_results(call)[0]
}

fn detach_collection(
    ctx: &mut FunctionContext<'_>,
    cow: FuncRef,
    root: Local,
    owner: Option<Value>,
    collection: Value,
) -> Value {
    let call = ctx.builder.ins().call(cow, &[collection]);
    let detached = ctx.builder.inst_results(call)[0];
    if let Some(slot) = owner {
        ctx.builder.ins().store(MemFlags::new(), detached, slot, 0);
    } else {
        ctx.write_local(root, detached);
    }
    detached
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
