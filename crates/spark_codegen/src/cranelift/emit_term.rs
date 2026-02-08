//! Terminator lowering: MIR terminators → Cranelift IR.
//!
//! Handles `Goto`, `Return`, `SwitchInt`, `Call`, and `Unreachable` terminators.

use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{AbiParam, InstBuilder, Signature, TrapCode, Value};
use cranelift_module::{FuncId, Module};
use rustc_hash::FxHashMap;
use spark_diagnostics::Diagnostics;
use spark_mir::{BlockId, Operand, Place, Terminator};
use spark_resolve::DefinitionTable;
use spark_types::TypeTable;

use super::context::FunctionContext;
use super::emit_rvalue;
use crate::abi;

/// Emits a MIR terminator instruction.
#[allow(clippy::too_many_arguments)]
pub fn emit_terminator<M: Module>(
    ctx: &mut FunctionContext<'_>,
    terminator: &Terminator,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
    diagnostics: &mut Diagnostics,
) {
    match terminator {
        Terminator::Goto { target } => {
            let target_block = ctx.clif_block(*target);
            ctx.builder.ins().jump(target_block, &[]);
        }

        Terminator::Return => {
            let return_type = ctx.mir_func.return_type;
            if abi::is_zero_sized(return_type, table) {
                ctx.builder.ins().return_(&[]);
            } else {
                let ret_val = ctx.read_local(spark_mir::Local::RETURN_PLACE);
                ctx.builder.ins().return_(&[ret_val]);
            }
        }

        Terminator::SwitchInt {
            discriminant,
            targets,
            otherwise,
        } => {
            emit_switch_int(ctx, discriminant, targets, *otherwise, table, definitions, func_ids, module);
        }

        Terminator::Call {
            func,
            args,
            destination,
            target,
            span: _,
        } => {
            emit_call(ctx, func, args, destination, *target, table, definitions, func_ids, module, diagnostics);
        }

        Terminator::Unreachable => {
            ctx.builder.ins().trap(TrapCode::unwrap_user(1));
        }
    }
}

/// Emits a `SwitchInt` terminator as a chain of compare-and-branch instructions.
#[allow(clippy::too_many_arguments)]
fn emit_switch_int<M: Module>(
    ctx: &mut FunctionContext<'_>,
    discriminant: &Operand,
    targets: &[(i128, BlockId)],
    otherwise: BlockId,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
) {
    let disc_val =
        emit_rvalue::emit_operand(ctx, discriminant, table, definitions, func_ids, module)
            .unwrap_or_else(|| ctx.builder.ins().iconst(types::I8, 0));

    for (value, target) in targets {
        let val_ty = ctx.builder.func.dfg.value_type(disc_val);
        let cmp_val = ctx.builder.ins().iconst(val_ty, *value as i64);
        let cmp = ctx
            .builder
            .ins()
            .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, disc_val, cmp_val);

        let target_block = ctx.clif_block(*target);
        let next_block = ctx.builder.create_block();

        ctx.builder.ins().brif(cmp, target_block, &[], next_block, &[]);
        ctx.builder.switch_to_block(next_block);
    }

    let otherwise_block = ctx.clif_block(otherwise);
    ctx.builder.ins().jump(otherwise_block, &[]);
}

/// Emits a function `Call` terminator.
#[allow(clippy::too_many_arguments)]
fn emit_call<M: Module>(
    ctx: &mut FunctionContext<'_>,
    func: &Operand,
    args: &[Operand],
    destination: &Place,
    target: BlockId,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
    diagnostics: &mut Diagnostics,
) {
    let func_val = emit_rvalue::emit_operand(ctx, func, table, definitions, func_ids, module);

    let arg_vals: Vec<Value> = args
        .iter()
        .filter_map(|arg| emit_rvalue::emit_operand(ctx, arg, table, definitions, func_ids, module))
        .collect();

    let call_result = match func {
        spark_mir::Operand::Constant(spark_mir::Constant::FunctionRef(def_id)) => {
            emit_direct_call(ctx, *def_id, &arg_vals, func_ids, module, diagnostics)
        }
        _ => emit_indirect_call(ctx, func_val, &arg_vals, destination, table, module),
    };

    if let Some(result_val) = call_result {
        if destination.projections.is_empty() {
            ctx.write_local(destination.local, result_val);
        }
    }

    let target_block = ctx.clif_block(target);
    ctx.builder.ins().jump(target_block, &[]);
}

/// Emits a direct function call via `FunctionRef`.
fn emit_direct_call<M: Module>(
    ctx: &mut FunctionContext<'_>,
    def_id: spark_resolve::DefId,
    arg_vals: &[Value],
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
    diagnostics: &mut Diagnostics,
) -> Option<Value> {
    if let Some(&func_id) = func_ids.get(&def_id) {
        let func_ref = module.declare_func_in_func(func_id, ctx.builder.func);
        let call = ctx.builder.ins().call(func_ref, arg_vals);
        ctx.builder.inst_results(call).first().copied()
    } else {
        diagnostics.warning(format!(
            "call to unknown function DefId({})",
            def_id.as_u32()
        ));
        None
    }
}

/// Emits an indirect function call through a function pointer.
fn emit_indirect_call<M: Module>(
    ctx: &mut FunctionContext<'_>,
    func_val: Option<Value>,
    arg_vals: &[Value],
    destination: &Place,
    table: &TypeTable,
    module: &M,
) -> Option<Value> {
    let fptr = func_val?;

    let dest_ty = ctx.local_type_id(destination.local);
    let mut sig = Signature::new(module.isa().default_call_conv());
    for av in arg_vals {
        sig.params.push(AbiParam::new(
            ctx.builder.func.dfg.value_type(*av),
        ));
    }
    if let Some(ret_clif) = abi::spark_type_to_clif(dest_ty, table, ctx.ptr_type) {
        sig.returns.push(AbiParam::new(ret_clif));
    }

    let sig_ref = ctx.builder.import_signature(sig);
    let call = ctx.builder.ins().call_indirect(sig_ref, fptr, arg_vals);
    ctx.builder.inst_results(call).first().copied()
}
