//! Materializes owned list snapshots of hash collections.

use cranelift_codegen::ir::{types, InstBuilder, StackSlotData, StackSlotKind, Value};
use cranelift_module::{FuncId, Module};
use spark_mir::Local;
use spark_types::Ty;

use super::context::{EmitCtx, FunctionContext};
use super::{emit_rvalue, types as clif_types};

pub(super) fn emit_hash_snapshot<M: Module>(
    ctx: &mut FunctionContext<'_>,
    scan: FuncId,
    receiver: Value,
    destination: Local,
    ecx: &EmitCtx<'_>,
    module: &mut M,
) -> Value {
    let table = ecx.table;
    let (_, Ty::List(element)) = clif_types::unwrap_mutable(ctx.local_type_id(destination), table)
    else {
        unreachable!("hash snapshot returns a list");
    };
    let element = *element;
    let descriptor = emit_rvalue::emit_value_desc(ctx, element, ecx, module);
    let new = module.declare_func_in_func(ctx.stubs.list_new, ctx.builder.func);
    let push = module.declare_func_in_func(ctx.stubs.list_push, ctx.builder.func);
    let next = module.declare_func_in_func(scan, ctx.builder.func);
    let call = ctx.builder.ins().call(new, &[descriptor]);
    let list = ctx.builder.inst_results(call)[0];

    let layout = ctx.layouts.layout_of(element, table);
    let slot = ctx.builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        layout.size,
        u8::try_from(layout.align.trailing_zeros()).unwrap_or(0),
    ));
    let address = ctx.builder.ins().stack_addr(ctx.ptr_type, slot, 0);
    let Ty::Tuple(fields) = table.resolve(element) else {
        unreachable!("map snapshot has tuple elements");
    };
    let offset = ctx.layouts.tuple_field_offset(fields, 1, table);
    let value = ctx.builder.ins().iadd_imm(address, i64::from(offset));

    let scan_block = ctx.builder.create_block();
    let push_block = ctx.builder.create_block();
    let done = ctx.builder.create_block();
    let cursor = ctx.builder.append_block_param(scan_block, types::I64);
    let start = ctx.builder.ins().iconst(types::I64, 0);
    ctx.builder.ins().jump(scan_block, &[start]);

    ctx.builder.switch_to_block(scan_block);
    let call = ctx
        .builder
        .ins()
        .call(next, &[receiver, cursor, address, value]);
    let cursor = ctx.builder.inst_results(call)[0];
    ctx.builder.ins().brif(cursor, push_block, &[], done, &[]);
    ctx.builder.seal_block(push_block);
    ctx.builder.switch_to_block(push_block);
    // The scanner retains the fields. List insertion takes their ownership.
    ctx.builder.ins().call(push, &[list, address]);
    ctx.builder.ins().jump(scan_block, &[cursor]);
    ctx.builder.seal_block(scan_block);
    ctx.builder.seal_block(done);
    ctx.builder.switch_to_block(done);
    list
}
