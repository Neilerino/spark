//! Native iteration over a pinned hash collection.

use cranelift_codegen::ir::{types, InstBuilder, StackSlotData, StackSlotKind, TrapCode, Value};
use cranelift_module::{FuncId, Module};
use spark_mir::Place;
use spark_types::TypeId;

use super::context::FunctionContext;
use super::emit_term::emit_indirect_call;
use crate::abi;

pub(super) fn emit_traversal<M: Module>(
    ctx: &mut FunctionContext<'_>,
    scan: FuncId,
    receiver: Value,
    visitor: Value,
    slot_types: &[TypeId],
    destination: &Place,
    module: &mut M,
) {
    let table = ctx.table;
    let addresses: Vec<_> = slot_types
        .iter()
        .map(|&ty| {
            let layout = ctx.layouts.layout_of(ty, table);
            let slot = ctx.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                layout.size,
                u8::try_from(layout.align.trailing_zeros()).unwrap_or(0),
            ));
            ctx.builder.ins().stack_addr(ctx.ptr_type, slot, 0)
        })
        .collect();

    let visitor_entry = ctx.load(ctx.ptr_type, visitor, 0);
    let start_block = ctx.builder.create_block();
    let invalid = ctx.builder.create_block();
    ctx.builder
        .ins()
        .brif(visitor_entry, start_block, &[], invalid, &[]);
    ctx.builder.seal_block(invalid);
    ctx.builder.switch_to_block(invalid);
    let report =
        module.declare_func_in_func(ctx.stubs.invalid_collection_visitor, ctx.builder.func);
    ctx.builder.ins().call(report, &[]);
    ctx.builder.ins().trap(TrapCode::unwrap_user(1));
    ctx.builder.seal_block(start_block);
    ctx.builder.switch_to_block(start_block);

    let scan_ref = module.declare_func_in_func(scan, ctx.builder.func);
    let scan_block = ctx.builder.create_block();
    let visit_block = ctx.builder.create_block();
    let done = ctx.builder.create_block();
    let cursor = ctx.builder.append_block_param(scan_block, types::I64);
    let start = ctx.builder.ins().iconst(types::I64, 0);
    ctx.builder.ins().jump(scan_block, &[start]);

    ctx.builder.switch_to_block(scan_block);
    let mut args = vec![receiver, cursor];
    args.extend_from_slice(&addresses);
    let call = ctx.builder.ins().call(scan_ref, &args);
    let next = ctx.builder.inst_results(call)[0];
    ctx.builder.ins().brif(next, visit_block, &[], done, &[]);
    ctx.builder.seal_block(visit_block);

    ctx.builder.switch_to_block(visit_block);
    let args: Vec<_> = addresses
        .iter()
        .zip(slot_types)
        .map(|(&address, &ty)| {
            if abi::is_scalar(ty, table) {
                let scalar = abi::spark_type_to_clif(ty, table, ctx.ptr_type)
                    .expect("traversal scalar has storage");
                ctx.load(scalar, address, 0)
            } else {
                address
            }
        })
        .collect();
    // The scan retains each output. The callback takes those owners.
    emit_indirect_call(ctx, Some(visitor), &args, destination, table, module);
    ctx.builder.ins().jump(scan_block, &[next]);
    ctx.builder.seal_block(scan_block);
    ctx.builder.seal_block(done);
    ctx.builder.switch_to_block(done);
}
