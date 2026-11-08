//! Terminator lowering: MIR terminators → Cranelift IR.
//!
//! Handles `Goto`, `Return`, `SwitchInt`, `Call`, and `Unreachable` terminators.

use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{AbiParam, InstBuilder, MemFlags, Signature, TrapCode, Value};
use cranelift_module::{FuncId, Linkage, Module};
use spark_diagnostics::Diagnostics;
use spark_intern::kw;
use spark_mir::{BlockId, Operand, Place, Terminator};
use spark_resolve::DefinitionKind;
use spark_types::{Ty, TypeTable};

use super::context::{self, EmitCtx, FunctionContext};
use super::emit_rvalue;
use super::types as clif_types;
use crate::abi;

/// Emits a MIR terminator instruction.
pub fn emit_terminator<M: Module>(
    ctx: &mut FunctionContext<'_>,
    terminator: &Terminator,
    ecx: &EmitCtx<'_>,
    module: &mut M,
    diagnostics: &mut Diagnostics,
) {
    let table = ecx.table;
    match terminator {
        Terminator::Goto { target } => {
            let target_block = ctx.clif_block(*target);
            ctx.builder.ins().jump(target_block, &[]);
        }

        Terminator::Return => {
            let return_type = ctx.mir_func.return_type;
            if abi::is_zero_sized(return_type, table) {
                ctx.builder.ins().return_(&[]);
            } else if let Some(sret_ptr) = ctx.sret_ptr {
                // Aggregate return: copy Local(0) data to the sret pointer.
                if let context::LocalStorage::StackSlot(slot, size) =
                    ctx.local_storage(spark_mir::Local::RETURN_PLACE)
                {
                    let src = ctx.builder.ins().stack_addr(ctx.ptr_type, slot, 0);
                    let config = module.target_config();
                    ctx.builder.emit_small_memory_copy(
                        config,
                        sret_ptr,
                        src,
                        u64::from(size),
                        1,
                        1,
                        true,
                        MemFlags::new(),
                    );
                }
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
            emit_switch_int(ctx, discriminant, targets, *otherwise, ecx, module);
        }

        Terminator::Call {
            func,
            args,
            destination,
            target,
            span: _,
        } => {
            emit_call(
                ctx,
                func,
                args,
                destination,
                *target,
                ecx,
                module,
                diagnostics,
            );
        }

        Terminator::Unreachable => {
            ctx.builder.ins().trap(TrapCode::unwrap_user(1));
        }
    }
}

/// Emits a `SwitchInt` terminator as a chain of compare-and-branch instructions.
fn emit_switch_int<M: Module>(
    ctx: &mut FunctionContext<'_>,
    discriminant: &Operand,
    targets: &[(i128, BlockId)],
    otherwise: BlockId,
    ecx: &EmitCtx<'_>,
    module: &mut M,
) {
    let disc_val = emit_rvalue::emit_operand(ctx, discriminant, ecx, module)
        .expect("ICE: switch discriminant operand produced no value; this is a compiler bug");

    for (value, target) in targets {
        let val_ty = ctx.builder.func.dfg.value_type(disc_val);
        let cmp_val = ctx.builder.ins().iconst(val_ty, *value as i64);
        let cmp = ctx.builder.ins().icmp(
            cranelift_codegen::ir::condcodes::IntCC::Equal,
            disc_val,
            cmp_val,
        );

        let target_block = ctx.clif_block(*target);
        let next_block = ctx.builder.create_block();

        ctx.builder
            .ins()
            .brif(cmp, target_block, &[], next_block, &[]);
        ctx.builder.switch_to_block(next_block);
    }

    let otherwise_block = ctx.clif_block(otherwise);
    ctx.builder.ins().jump(otherwise_block, &[]);
}

/// Emits a function `Call` terminator.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn emit_call<M: Module>(
    ctx: &mut FunctionContext<'_>,
    func: &Operand,
    args: &[Operand],
    destination: &Place,
    target: BlockId,
    ecx: &EmitCtx<'_>,
    module: &mut M,
    diagnostics: &mut Diagnostics,
) {
    let table = ecx.table;

    // Intercept intrinsic calls introduced by the deforestation pass.
    if let Some(intrinsic) = resolve_intrinsic(func, ctx) {
        emit_intrinsic_call(ctx, intrinsic, args, destination, target, ecx, module);
        return;
    }

    // Intercept built-in `print` calls and method calls for type-based dispatch.
    // The func operand is typically `Copy(place)` of a local that was assigned
    // a `Constant::FunctionRef(def_id)`, so we resolve the DefId from the MIR.
    if let Some(callee_def_id) = ctx.mir_func.resolve_callee_def_id(func) {
        if let Some(info) = ecx.definitions.get(callee_def_id) {
            if matches!(info.kind, DefinitionKind::BuiltinFunction) && info.name == kw::PRINT {
                emit_builtin_print(ctx, args, target, ecx, module);
                return;
            }
            if matches!(info.kind, DefinitionKind::BuiltinFunction)
                && (info.name == kw::SORT || info.name == kw::TAKE || info.name == kw::REVERSE)
            {
                emit_builtin_list_fn(ctx, info.name, args, destination, target, ecx, module);
                return;
            }
            if matches!(info.kind, DefinitionKind::BuiltinMethod) {
                emit_builtin_method(ctx, info.name, args, destination, target, ecx, module);
                return;
            }
            if matches!(info.kind, DefinitionKind::ExternFunction) {
                emit_extern_call(
                    ctx,
                    callee_def_id,
                    info,
                    args,
                    destination,
                    target,
                    ecx,
                    module,
                );
                return;
            }
            if matches!(info.kind, DefinitionKind::BuiltinFunction)
                && (info.name == kw::PARALLEL_FOREACH || info.name == kw::PARALLEL_MAP)
            {
                emit_parallel_call(ctx, info.name, args, destination, target, ecx, module);
                return;
            }
            if matches!(info.kind, DefinitionKind::BuiltinFunction)
                && info.name == kw::PARALLEL_MAP_REDUCE
            {
                emit_parallel_map_reduce_call(ctx, args, destination, target, ecx, module);
                return;
            }
            if matches!(info.kind, DefinitionKind::BuiltinFunction) && info.name == kw::SPARK_SPAWN
            {
                emit_spawn_call(ctx, args, destination, target, ecx, module);
                return;
            }
            if matches!(info.kind, DefinitionKind::BuiltinFunction) && info.name == kw::SPARK_AWAIT
            {
                emit_await_call(ctx, args, destination, target, ecx, module);
                return;
            }
            if matches!(info.kind, DefinitionKind::BuiltinFunction) && info.name == kw::TRY_POLL {
                emit_try_poll_call(ctx, args, destination, target, ecx, module);
                return;
            }
        }
    }

    // Devirtualization: if the function operand traces back to a known
    // FunctionRef, we can emit a direct call even when the operand is
    // Copy/Move of a local (skipping the fat-pointer thunk entirely).
    //
    // Lambda functions are excluded: they have a hidden env_ptr parameter
    // in their Cranelift signature that isn't present in MIR call args.
    // Calling a lambda directly without env_ptr would cause a verifier error.
    let is_lambda = |id: &spark_resolve::DefId| {
        ecx.definitions
            .get(*id)
            .is_some_and(|info| matches!(info.kind, spark_resolve::DefinitionKind::Lambda))
    };
    let resolved_def_id = match func {
        spark_mir::Operand::Constant(spark_mir::Constant::FunctionRef(def_id)) => Some(*def_id),
        _ => ctx
            .mir_func
            .resolve_callee_def_id(func)
            .filter(|id| ecx.func_ids.contains_key(id)),
    }
    .filter(|id| !is_lambda(id));

    let arg_vals: Vec<Value> = args
        .iter()
        .filter_map(|arg| emit_rvalue::emit_operand(ctx, arg, ecx, module))
        .collect();

    // Check if the callee returns an aggregate (sret convention).
    let dest_ty = ctx.local_type_id(destination.local);
    let sret = abi::needs_sret(dest_ty, table);

    let call_result = if sret {
        // Aggregate return: pass destination address as sret pointer.
        // The callee writes directly to this address; no return value.
        let sret_addr = ctx
            .local_stack_addr(destination.local)
            .expect("ICE: sret destination local has no stack slot; this is a compiler bug");
        if let Some(def_id) = resolved_def_id {
            emit_direct_call_sret(
                ctx,
                def_id,
                sret_addr,
                &arg_vals,
                ecx.func_ids,
                module,
                diagnostics,
            );
        } else {
            let func_val = emit_rvalue::emit_operand(ctx, func, ecx, module);
            emit_indirect_call_sret(
                ctx,
                func_val,
                sret_addr,
                &arg_vals,
                destination,
                table,
                module,
            );
        }
        None // data already written to sret_addr
    } else if let Some(def_id) = resolved_def_id {
        emit_direct_call(ctx, def_id, &arg_vals, ecx.func_ids, module, diagnostics)
    } else {
        let func_val = emit_rvalue::emit_operand(ctx, func, ecx, module);
        emit_indirect_call(ctx, func_val, &arg_vals, destination, table, module)
    };

    if let Some(result_val) = call_result {
        if destination.projections.is_empty() {
            ctx.write_local(destination.local, result_val);
        }
    }

    let target_block = ctx.clif_block(target);
    ctx.builder.ins().jump(target_block, &[]);
}

/// Resolves an [`Intrinsic`] from a function operand.
///
/// The deforestation pass stores `Constant::IntrinsicRef(intrinsic)` directly
/// in the function operand. We also check locals that were assigned one.
fn resolve_intrinsic(func: &Operand, ctx: &FunctionContext<'_>) -> Option<spark_mir::Intrinsic> {
    if let spark_mir::Operand::Constant(spark_mir::Constant::IntrinsicRef(intrinsic)) = func {
        return Some(*intrinsic);
    }

    let local = match func {
        spark_mir::Operand::Copy(place) | spark_mir::Operand::Move(place)
            if place.projections.is_empty() =>
        {
            place.local
        }
        _ => return None,
    };

    for block in &ctx.mir_func.blocks {
        for stmt in &block.statements {
            if let spark_mir::Statement::Assign { place, rvalue, .. } = stmt {
                if place.local == local && place.projections.is_empty() {
                    if let spark_mir::Rvalue::Use(spark_mir::Operand::Constant(
                        spark_mir::Constant::IntrinsicRef(intrinsic),
                    )) = rvalue
                    {
                        return Some(*intrinsic);
                    }
                }
            }
        }
    }

    None
}

/// Emits a call to a compiler intrinsic introduced by the deforestation pass.
///
/// Intrinsic args are already rewritten by the deforestation pass:
/// `[start, end, inclusive_i64, step, ...rest_args]`
#[allow(clippy::too_many_lines)] // covers all intrinsic variants
fn emit_intrinsic_call<M: Module>(
    ctx: &mut FunctionContext<'_>,
    intrinsic: spark_mir::Intrinsic,
    args: &[Operand],
    destination: &spark_mir::Place,
    target: BlockId,
    ecx: &EmitCtx<'_>,
    module: &mut M,
) {
    let table = ecx.table;

    // Evaluate range parameters: start, end, inclusive, step (args[0..4]).
    let start_val = args
        .first()
        .and_then(|a| emit_rvalue::emit_operand(ctx, a, ecx, module));
    let end_val = args
        .get(1)
        .and_then(|a| emit_rvalue::emit_operand(ctx, a, ecx, module));
    let inclusive_val = args
        .get(2)
        .and_then(|a| emit_rvalue::emit_operand(ctx, a, ecx, module));
    let step_val = args
        .get(3)
        .and_then(|a| emit_rvalue::emit_operand(ctx, a, ecx, module));

    let (Some(s), Some(e), Some(inc), Some(st)) = (start_val, end_val, inclusive_val, step_val)
    else {
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    };

    // Truncate inclusive from i64 to i8 (the runtime expects i8).
    let inc_i8 = ctx.builder.ins().ireduce(types::I8, inc);

    match intrinsic {
        spark_mir::Intrinsic::ParallelRangeForEach => {
            // args[4] = callback fat pointer
            let cb_val = args
                .get(4)
                .and_then(|a| emit_rvalue::emit_operand(ctx, a, ecx, module));
            let Some(fat_ptr) = cb_val else {
                let target_block = ctx.clif_block(target);
                ctx.builder.ins().jump(target_block, &[]);
                return;
            };
            let fn_ptr = ctx
                .builder
                .ins()
                .load(ctx.ptr_type, MemFlags::new(), fat_ptr, 0);
            let env_ptr = ctx.builder.ins().load(
                ctx.ptr_type,
                MemFlags::new(),
                fat_ptr,
                ctx.ptr_type.bytes() as i32,
            );

            // Compute cb_kind from callback type.
            let cb_kind = compute_cb_kind_for_arg(ctx, args, 4, table);
            let cb_kind_val = ctx.builder.ins().iconst(types::I8, i64::from(cb_kind));

            let func_ref =
                module.declare_func_in_func(ctx.stubs.parallel_range_foreach, ctx.builder.func);
            ctx.builder
                .ins()
                .call(func_ref, &[s, e, inc_i8, st, fn_ptr, env_ptr, cb_kind_val]);
        }

        spark_mir::Intrinsic::ParallelRangeMap => {
            // args[4] = callback fat pointer
            let cb_val = args
                .get(4)
                .and_then(|a| emit_rvalue::emit_operand(ctx, a, ecx, module));
            let Some(fat_ptr) = cb_val else {
                let target_block = ctx.clif_block(target);
                ctx.builder.ins().jump(target_block, &[]);
                return;
            };
            let fn_ptr = ctx
                .builder
                .ins()
                .load(ctx.ptr_type, MemFlags::new(), fat_ptr, 0);
            let env_ptr = ctx.builder.ins().load(
                ctx.ptr_type,
                MemFlags::new(),
                fat_ptr,
                ctx.ptr_type.bytes() as i32,
            );

            let cb_kind = compute_cb_kind_for_arg(ctx, args, 4, table);
            let cb_kind_val = ctx.builder.ins().iconst(types::I8, i64::from(cb_kind));

            // Compute result_elem_size from the callback's return type.
            let ret_ty = get_callback_return_type(ctx, args, 4, table);
            let result_layout = ctx.layouts.layout_of(ret_ty, table);
            let result_elem_size_val = ctx
                .builder
                .ins()
                .iconst(types::I64, i64::from(result_layout.size));

            let func_ref =
                module.declare_func_in_func(ctx.stubs.parallel_range_map, ctx.builder.func);
            let call = ctx.builder.ins().call(
                func_ref,
                &[
                    s,
                    e,
                    inc_i8,
                    st,
                    fn_ptr,
                    env_ptr,
                    result_elem_size_val,
                    cb_kind_val,
                ],
            );
            let result = ctx.builder.inst_results(call)[0];
            if destination.projections.is_empty() {
                ctx.write_local(destination.local, result);
            }
        }

        spark_mir::Intrinsic::ParallelRangeMapReduce => {
            // args: [start, end, inclusive, step, mapper, combiner, initial]
            let mapper_val = args
                .get(4)
                .and_then(|a| emit_rvalue::emit_operand(ctx, a, ecx, module));
            let combiner_val = args
                .get(5)
                .and_then(|a| emit_rvalue::emit_operand(ctx, a, ecx, module));
            let initial_val = args
                .get(6)
                .and_then(|a| emit_rvalue::emit_operand(ctx, a, ecx, module));

            let (Some(mapper_fp), Some(combiner_fp), Some(initial)) =
                (mapper_val, combiner_val, initial_val)
            else {
                let target_block = ctx.clif_block(target);
                ctx.builder.ins().jump(target_block, &[]);
                return;
            };

            let map_fn = ctx
                .builder
                .ins()
                .load(ctx.ptr_type, MemFlags::new(), mapper_fp, 0);
            let map_env = ctx.builder.ins().load(
                ctx.ptr_type,
                MemFlags::new(),
                mapper_fp,
                ctx.ptr_type.bytes() as i32,
            );
            let reduce_fn = ctx
                .builder
                .ins()
                .load(ctx.ptr_type, MemFlags::new(), combiner_fp, 0);
            let reduce_env = ctx.builder.ins().load(
                ctx.ptr_type,
                MemFlags::new(),
                combiner_fp,
                ctx.ptr_type.bytes() as i32,
            );

            let map_cb_kind = compute_cb_kind_for_arg(ctx, args, 4, table);
            let map_cb_kind_val = ctx.builder.ins().iconst(types::I8, i64::from(map_cb_kind));
            let reduce_cb_kind = compute_cb_kind_for_arg(ctx, args, 5, table);
            let reduce_cb_kind_val = ctx
                .builder
                .ins()
                .iconst(types::I8, i64::from(reduce_cb_kind));

            let func_ref =
                module.declare_func_in_func(ctx.stubs.parallel_range_map_reduce, ctx.builder.func);
            let call = ctx.builder.ins().call(
                func_ref,
                &[
                    s,
                    e,
                    inc_i8,
                    st,
                    map_fn,
                    map_env,
                    reduce_fn,
                    reduce_env,
                    initial,
                    map_cb_kind_val,
                    reduce_cb_kind_val,
                ],
            );
            let result = ctx.builder.inst_results(call)[0];
            if destination.projections.is_empty() {
                ctx.write_local(destination.local, result);
            }
        }
    }

    let target_block = ctx.clif_block(target);
    ctx.builder.ins().jump(target_block, &[]);
}

/// Computes the `cb_kind` bitmask for a callback argument at the given index.
fn compute_cb_kind_for_arg(
    ctx: &FunctionContext<'_>,
    args: &[Operand],
    arg_idx: usize,
    table: &TypeTable,
) -> u8 {
    let cb_ty_id = match args.get(arg_idx) {
        Some(Operand::Copy(place) | Operand::Move(place)) => ctx.local_type_id(place.local),
        _ => spark_types::TypeId::ERROR,
    };
    let (param_ty, ret_ty) = match table.resolve(cb_ty_id).clone() {
        Ty::Function { params, ret, .. } => {
            let p = params
                .first()
                .copied()
                .unwrap_or(spark_types::TypeId::ERROR);
            (p, ret)
        }
        _ => (spark_types::TypeId::ERROR, spark_types::TypeId::ERROR),
    };

    let input_kind: u8 = u8::from(matches!(table.resolve(param_ty), Ty::Float(_)));
    let output_kind: u8 = if matches!(table.resolve(ret_ty), Ty::Void) {
        2
    } else {
        u8::from(matches!(table.resolve(ret_ty), Ty::Float(_)))
    };
    input_kind | (output_kind << 2)
}

/// Gets the return type of a callback at the given argument index.
fn get_callback_return_type(
    ctx: &FunctionContext<'_>,
    args: &[Operand],
    arg_idx: usize,
    table: &TypeTable,
) -> spark_types::TypeId {
    let cb_ty_id = match args.get(arg_idx) {
        Some(Operand::Copy(place) | Operand::Move(place)) => ctx.local_type_id(place.local),
        _ => return spark_types::TypeId::ERROR,
    };
    match table.resolve(cb_ty_id).clone() {
        Ty::Function { ret, .. } => ret,
        _ => spark_types::TypeId::ERROR,
    }
}

/// Emits a built-in `print` call, dispatching to the correct runtime stub
/// based on the argument's type.
fn emit_builtin_print<M: Module>(
    ctx: &mut FunctionContext<'_>,
    args: &[Operand],
    target: BlockId,
    ecx: &EmitCtx<'_>,
    module: &mut M,
) {
    let table = ecx.table;

    // Evaluate the single argument.
    let arg_val = args
        .first()
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, ecx, module));

    if let Some(val) = arg_val {
        // Determine the argument's type from the MIR operand.
        let resolved_ty = match &args[0] {
            spark_mir::Operand::Copy(place) | spark_mir::Operand::Move(place) => {
                table.resolve(ctx.local_type_id(place.local))
            }
            spark_mir::Operand::Constant(c) => match c {
                spark_mir::Constant::Float(_) => &Ty::Float(spark_types::FloatWidth::F64),
                spark_mir::Constant::Bool(_) => &Ty::Bool,
                spark_mir::Constant::String(_) => &Ty::String,
                _ => &Ty::Int(spark_types::IntWidth::I64),
            },
        };

        let stub_id = match resolved_ty {
            Ty::String => ctx.stubs.print_string,
            Ty::Bool => ctx.stubs.print_bool,
            Ty::Float(_) => ctx.stubs.print_float,
            _ => ctx.stubs.print_int,
        };

        let func_ref = module.declare_func_in_func(stub_id, ctx.builder.func);
        ctx.builder.ins().call(func_ref, &[val]);
    }

    let target_block = ctx.clif_block(target);
    ctx.builder.ins().jump(target_block, &[]);
}

/// Emits a built-in list function call (sort, take, reverse).
///
/// These are free functions that take a list as the first argument and return a new list.
/// `expr via sort` is desugared to `sort(expr)` by HIR, so args[0] is the list.
fn emit_builtin_list_fn<M: Module>(
    ctx: &mut FunctionContext<'_>,
    fn_name: spark_intern::Symbol,
    args: &[Operand],
    destination: &Place,
    target: BlockId,
    ecx: &EmitCtx<'_>,
    module: &mut M,
) {
    let table = ecx.table;

    // Evaluate the list argument (args[0]).
    let list_val = args
        .first()
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, ecx, module));

    let Some(list_ptr) = list_val else {
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    };

    if fn_name == kw::SORT {
        // Determine the sort type tag from the list's element type.
        let tag: i64 = if let Some(Operand::Copy(place) | Operand::Move(place)) = args.first() {
            let list_ty_id = ctx.local_type_id(place.local);
            let (_, stripped) = clif_types::unwrap_mutable(list_ty_id, table);
            if let Ty::List(elem) = stripped {
                let (_, inner) = clif_types::unwrap_mutable(*elem, table);
                match inner {
                    Ty::Float(_) => 1,
                    Ty::String => 2,
                    Ty::Bool => 3,
                    _ => 0, // Int and fallback
                }
            } else {
                0
            }
        } else {
            0
        };
        let tag_val = ctx.builder.ins().iconst(types::I64, tag);
        let func_ref = module.declare_func_in_func(ctx.stubs.list_sort, ctx.builder.func);
        let call = ctx.builder.ins().call(func_ref, &[list_ptr, tag_val]);
        let result = ctx.builder.inst_results(call)[0];
        if destination.projections.is_empty() {
            ctx.write_local(destination.local, result);
        }
    } else if fn_name == kw::TAKE {
        // args[1] is the count
        let count_val = args
            .get(1)
            .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, ecx, module))
            .unwrap_or_else(|| ctx.builder.ins().iconst(types::I64, 0));
        let func_ref = module.declare_func_in_func(ctx.stubs.list_take, ctx.builder.func);
        let call = ctx.builder.ins().call(func_ref, &[list_ptr, count_val]);
        let result = ctx.builder.inst_results(call)[0];
        if destination.projections.is_empty() {
            ctx.write_local(destination.local, result);
        }
    } else if fn_name == kw::REVERSE {
        let func_ref = module.declare_func_in_func(ctx.stubs.list_reverse, ctx.builder.func);
        let call = ctx.builder.ins().call(func_ref, &[list_ptr]);
        let result = ctx.builder.inst_results(call)[0];
        if destination.projections.is_empty() {
            ctx.write_local(destination.local, result);
        }
    }

    let target_block = ctx.clif_block(target);
    ctx.builder.ins().jump(target_block, &[]);
}

/// Emits a parallel iteration call (`__spark_parallel_foreach` or `__spark_parallel_map`).
///
/// MIR args: `[list, callback]` where `callback` is a fat pointer `(fn_ptr, env_ptr)`.
/// The function unpacks the fat pointer and computes `elem_size` and `cb_kind` from types,
/// then calls the appropriate runtime stub with the raw arguments.
fn emit_parallel_call<M: Module>(
    ctx: &mut FunctionContext<'_>,
    fn_name: spark_intern::Symbol,
    args: &[Operand],
    destination: &Place,
    target: BlockId,
    ecx: &EmitCtx<'_>,
    module: &mut M,
) {
    let table = ecx.table;
    let is_map = fn_name == kw::PARALLEL_MAP;

    // Evaluate the list argument (args[0]).
    let list_val = args
        .first()
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, ecx, module));

    let Some(list_ptr) = list_val else {
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    };

    // Evaluate the callback argument (args[1]) — a fat pointer aggregate.
    let cb_val = args
        .get(1)
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, ecx, module));

    let Some(fat_ptr_addr) = cb_val else {
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    };

    // Load fn_ptr and env_ptr from the fat pointer.
    let fn_ptr = ctx
        .builder
        .ins()
        .load(ctx.ptr_type, MemFlags::new(), fat_ptr_addr, 0);
    let env_ptr = ctx.builder.ins().load(
        ctx.ptr_type,
        MemFlags::new(),
        fat_ptr_addr,
        ctx.ptr_type.bytes() as i32,
    );

    // Determine elem_size from the list's element type.
    let list_ty_id = if let Some(Operand::Copy(place)) = args.first() {
        ctx.local_type_id(place.local)
    } else {
        spark_types::TypeId::ERROR
    };
    let elem_ty = match table.resolve(list_ty_id).clone() {
        Ty::List(elem) => elem,
        _ => spark_types::TypeId::ERROR,
    };
    let elem_layout = ctx.layouts.layout_of(elem_ty, table);
    let elem_size_val = ctx
        .builder
        .ins()
        .iconst(types::I64, i64::from(elem_layout.size));

    // Compute cb_kind flag from callback type info.
    // Bits 0-1: input kind (0 = int-like, 1 = float)
    // Bits 2-3: output kind (0 = int-like, 1 = float, 2 = void)
    let cb_ty_id = if let Some(Operand::Copy(place)) = args.get(1) {
        ctx.local_type_id(place.local)
    } else {
        spark_types::TypeId::ERROR
    };
    let (param_ty, ret_ty) = match table.resolve(cb_ty_id).clone() {
        Ty::Function { params, ret, .. } => {
            let p = params
                .first()
                .copied()
                .unwrap_or(spark_types::TypeId::ERROR);
            (p, ret)
        }
        _ => (spark_types::TypeId::ERROR, spark_types::TypeId::ERROR),
    };

    let input_kind: u8 = u8::from(matches!(table.resolve(param_ty), Ty::Float(_)));
    let output_kind: u8 = if matches!(table.resolve(ret_ty), Ty::Void) {
        2
    } else {
        u8::from(matches!(table.resolve(ret_ty), Ty::Float(_)))
    };
    let cb_kind: u8 = input_kind | (output_kind << 2);
    let cb_kind_val = ctx.builder.ins().iconst(types::I8, i64::from(cb_kind));

    if is_map {
        // __spark_parallel_map(list, fn_ptr, env_ptr, elem_size, result_elem_size, cb_kind) -> ptr
        let result_elem_layout = ctx.layouts.layout_of(ret_ty, table);
        let result_elem_size_val = ctx
            .builder
            .ins()
            .iconst(types::I64, i64::from(result_elem_layout.size));
        let func_ref = module.declare_func_in_func(ctx.stubs.parallel_map, ctx.builder.func);
        let call = ctx.builder.ins().call(
            func_ref,
            &[
                list_ptr,
                fn_ptr,
                env_ptr,
                elem_size_val,
                result_elem_size_val,
                cb_kind_val,
            ],
        );
        let result = ctx.builder.inst_results(call)[0];
        if destination.projections.is_empty() {
            ctx.write_local(destination.local, result);
        }
    } else {
        // __spark_parallel_foreach(list, fn_ptr, env_ptr, elem_size, cb_kind) -> void
        let func_ref = module.declare_func_in_func(ctx.stubs.parallel_foreach, ctx.builder.func);
        ctx.builder.ins().call(
            func_ref,
            &[list_ptr, fn_ptr, env_ptr, elem_size_val, cb_kind_val],
        );
    }

    let target_block = ctx.clif_block(target);
    ctx.builder.ins().jump(target_block, &[]);
}

/// Emits a fused `parallel_map_reduce` call.
///
/// Arguments: [list, `mapper_fat_ptr`, `combiner_fat_ptr`, `initial_value`]
/// Runtime: `__spark_parallel_map_reduce(list, map_fn, map_env, reduce_fn, reduce_env, initial, elem_size, map_cb_kind, reduce_cb_kind) -> i64`
#[allow(clippy::too_many_lines)] // must unpack all fat-pointer and cb_kind args
fn emit_parallel_map_reduce_call<M: Module>(
    ctx: &mut FunctionContext<'_>,
    args: &[Operand],
    destination: &Place,
    target: BlockId,
    ecx: &EmitCtx<'_>,
    module: &mut M,
) {
    let table = ecx.table;

    // Evaluate list (args[0])
    let list_val = args
        .first()
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, ecx, module));
    let Some(list_ptr) = list_val else {
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    };

    // Evaluate mapper fat pointer (args[1])
    let mapper_val = args
        .get(1)
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, ecx, module));
    let Some(mapper_fat_ptr) = mapper_val else {
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    };

    // Evaluate combiner fat pointer (args[2])
    let combiner_val = args
        .get(2)
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, ecx, module));
    let Some(combiner_fat_ptr) = combiner_val else {
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    };

    // Evaluate initial value (args[3])
    let initial_val = args
        .get(3)
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, ecx, module));
    let Some(initial) = initial_val else {
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    };

    // Load fn_ptr and env_ptr from mapper fat pointer
    let map_fn = ctx
        .builder
        .ins()
        .load(ctx.ptr_type, MemFlags::new(), mapper_fat_ptr, 0);
    let map_env = ctx.builder.ins().load(
        ctx.ptr_type,
        MemFlags::new(),
        mapper_fat_ptr,
        ctx.ptr_type.bytes() as i32,
    );

    // Load fn_ptr and env_ptr from combiner fat pointer
    let reduce_fn = ctx
        .builder
        .ins()
        .load(ctx.ptr_type, MemFlags::new(), combiner_fat_ptr, 0);
    let reduce_env = ctx.builder.ins().load(
        ctx.ptr_type,
        MemFlags::new(),
        combiner_fat_ptr,
        ctx.ptr_type.bytes() as i32,
    );

    // Determine elem_size from list element type
    let list_ty_id = if let Some(Operand::Copy(place)) = args.first() {
        ctx.local_type_id(place.local)
    } else {
        spark_types::TypeId::ERROR
    };
    let elem_ty = match table.resolve(list_ty_id).clone() {
        Ty::List(elem) => elem,
        _ => spark_types::TypeId::ERROR,
    };
    let elem_layout = ctx.layouts.layout_of(elem_ty, table);
    let elem_size_val = ctx
        .builder
        .ins()
        .iconst(types::I64, i64::from(elem_layout.size));

    // Compute map_cb_kind from mapper type
    let map_cb_ty_id = if let Some(Operand::Copy(place)) = args.get(1) {
        ctx.local_type_id(place.local)
    } else {
        spark_types::TypeId::ERROR
    };
    let (map_param_ty, map_ret_ty) = match table.resolve(map_cb_ty_id).clone() {
        Ty::Function { params, ret, .. } => {
            let p = params
                .first()
                .copied()
                .unwrap_or(spark_types::TypeId::ERROR);
            (p, ret)
        }
        _ => (spark_types::TypeId::ERROR, spark_types::TypeId::ERROR),
    };
    let map_input_kind: u8 = u8::from(matches!(table.resolve(map_param_ty), Ty::Float(_)));
    let map_output_kind: u8 = u8::from(matches!(table.resolve(map_ret_ty), Ty::Float(_)));
    let map_cb_kind: u8 = map_input_kind | (map_output_kind << 2);
    let map_cb_kind_val = ctx.builder.ins().iconst(types::I8, i64::from(map_cb_kind));

    // Compute reduce_cb_kind from combiner type
    let reduce_cb_ty_id = if let Some(Operand::Copy(place)) = args.get(2) {
        ctx.local_type_id(place.local)
    } else {
        spark_types::TypeId::ERROR
    };
    let (reduce_param_ty, reduce_ret_ty) = match table.resolve(reduce_cb_ty_id).clone() {
        Ty::Function { params, ret, .. } => {
            let p = params
                .first()
                .copied()
                .unwrap_or(spark_types::TypeId::ERROR);
            (p, ret)
        }
        _ => (spark_types::TypeId::ERROR, spark_types::TypeId::ERROR),
    };
    let reduce_input_kind: u8 = u8::from(matches!(table.resolve(reduce_param_ty), Ty::Float(_)));
    let reduce_output_kind: u8 = u8::from(matches!(table.resolve(reduce_ret_ty), Ty::Float(_)));
    let reduce_cb_kind: u8 = reduce_input_kind | (reduce_output_kind << 2);
    let reduce_cb_kind_val = ctx
        .builder
        .ins()
        .iconst(types::I8, i64::from(reduce_cb_kind));

    let func_ref = module.declare_func_in_func(ctx.stubs.parallel_map_reduce, ctx.builder.func);
    let call = ctx.builder.ins().call(
        func_ref,
        &[
            list_ptr,
            map_fn,
            map_env,
            reduce_fn,
            reduce_env,
            initial,
            elem_size_val,
            map_cb_kind_val,
            reduce_cb_kind_val,
        ],
    );
    let result = ctx.builder.inst_results(call)[0];
    if destination.projections.is_empty() {
        ctx.write_local(destination.local, result);
    }

    let target_block = ctx.clif_block(target);
    ctx.builder.ins().jump(target_block, &[]);
}

/// Emits a `spawn` call, using the cooperative path when a poll function is available.
///
/// **Cooperative path** (when the spawned lambda wraps an async function with a poll entry):
/// 1. Allocates an async state struct via `__spark_alloc_state(state_size)`
/// 2. Stores the async function's parameters into the state struct
/// 3. Sets the dispatch tag to 0 (initial segment)
/// 4. Calls `__spark_spawn_coop(poll_fn_ptr, state_ptr, state_size, result_size, result_offset)`
///
/// **Blocking fallback** (no poll function available):
/// Extracts `fn_ptr` and `env_ptr` from the closure fat pointer and calls `__spark_spawn`.
fn emit_spawn_call<M: Module>(
    ctx: &mut FunctionContext<'_>,
    args: &[Operand],
    destination: &Place,
    target: BlockId,
    ecx: &EmitCtx<'_>,
    module: &mut M,
) {
    let table = ecx.table;

    // Try cooperative spawn path.
    if try_emit_coop_spawn(ctx, args, destination, target, ecx, module).is_some() {
        return;
    }

    // Fallback: blocking spawn via __spark_spawn.
    let cb_val = args
        .first()
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, ecx, module));

    let Some(fat_ptr_addr) = cb_val else {
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    };

    // Load fn_ptr and env_ptr from the fat pointer.
    let fn_ptr = ctx
        .builder
        .ins()
        .load(ctx.ptr_type, MemFlags::new(), fat_ptr_addr, 0);
    let env_ptr = ctx.builder.ins().load(
        ctx.ptr_type,
        MemFlags::new(),
        fat_ptr_addr,
        ctx.ptr_type.bytes() as i32,
    );

    // Determine result_size from the destination type (Future[T] → T).
    let dest_ty_id = ctx.local_type_id(destination.local);
    let inner_ty = match table.resolve(dest_ty_id) {
        Ty::Future { inner } => *inner,
        _ => dest_ty_id,
    };
    let result_layout = ctx.layouts.layout_of(inner_ty, table);
    let result_size_val = ctx
        .builder
        .ins()
        .iconst(types::I64, i64::from(result_layout.size));

    // Determine is_float flag from the closure's return type.
    let cb_ty_id = if let Some(Operand::Copy(place)) = args.first() {
        ctx.local_type_id(place.local)
    } else {
        spark_types::TypeId::ERROR
    };
    let is_float = match table.resolve(cb_ty_id) {
        Ty::Function { ret, .. } => matches!(table.resolve(*ret), Ty::Float(_)),
        _ => false,
    };
    let is_float_val = ctx
        .builder
        .ins()
        .iconst(types::I8, i64::from(u8::from(is_float)));

    // Call __spark_spawn(fn_ptr, env_ptr, result_size, is_float) -> ptr
    let func_ref = module.declare_func_in_func(ctx.stubs.spawn, ctx.builder.func);
    let call = ctx
        .builder
        .ins()
        .call(func_ref, &[fn_ptr, env_ptr, result_size_val, is_float_val]);
    let result = ctx.builder.inst_results(call)[0];
    if destination.projections.is_empty() {
        ctx.write_local(destination.local, result);
    }

    let target_block = ctx.clif_block(target);
    ctx.builder.ins().jump(target_block, &[]);
}

/// Attempts to emit a cooperative spawn. Returns `Some(())` if successful.
///
/// Detects if the spawned closure wraps an async function that has a poll entry,
/// and if so, emits the cooperative spawn sequence instead of the blocking path.
#[allow(clippy::too_many_lines)] // cooperative spawn setup is inherently complex
fn try_emit_coop_spawn<M: Module>(
    ctx: &mut FunctionContext<'_>,
    args: &[Operand],
    destination: &Place,
    target: BlockId,
    ecx: &EmitCtx<'_>,
    module: &mut M,
) -> Option<()> {
    let table = ecx.table;

    // Step 1: Find the closure local from the spawn argument.
    let closure_local = match args.first() {
        Some(Operand::Copy(place) | Operand::Move(place)) if place.projections.is_empty() => {
            place.local
        }
        _ => return None,
    };

    // Step 2: Scan MIR for the MakeClosure that assigned to this local.
    let mut lambda_def_id = None;
    let mut capture_operands = Vec::new();
    for block in &ctx.mir_func.blocks {
        for stmt in &block.statements {
            if let spark_mir::Statement::Assign { place, rvalue, .. } = stmt {
                if place.local == closure_local && place.projections.is_empty() {
                    if let spark_mir::Rvalue::Aggregate(
                        spark_mir::AggregateKind::Closure { func_def_id },
                        operands,
                    ) = rvalue
                    {
                        lambda_def_id = Some(*func_def_id);
                        capture_operands = operands.clone();
                    }
                }
            }
        }
    }

    let lambda_def_id = lambda_def_id?;

    // Step 3: Check if this lambda wraps an async function with a poll entry.
    let async_def_id = ecx.lambda_to_async.get(&lambda_def_id)?;
    let poll_info = ecx.poll_fn_map.get(async_def_id)?;

    // Step 4: Get the poll function's FuncId.
    let poll_func_id = ecx.func_ids.get(&poll_info.poll_def_id)?;

    // Step 5: Compute state layout.
    let state_layout = ctx
        .layouts
        .async_state_layout_of(&poll_info.state_fields, table);
    let state_size = state_layout.layout.size;

    // Step 6: Allocate state via __spark_alloc_state(state_size).
    let state_size_val = ctx.builder.ins().iconst(types::I64, i64::from(state_size));
    let alloc_ref = module.declare_func_in_func(ctx.stubs.alloc_state, ctx.builder.func);
    let alloc_call = ctx.builder.ins().call(alloc_ref, &[state_size_val]);
    let state_ptr = ctx.builder.inst_results(alloc_call)[0];

    // Step 7: Store capture values (async function params) into state struct fields.
    // The state struct fields begin with __tag, then __param_<name> fields for each param.
    // Captures correspond to the param fields in order.
    let tag_offset = state_layout.field_offsets.first().copied().unwrap_or(0);

    // Store __tag = 0 (initial segment).
    let tag_zero = ctx.builder.ins().iconst(types::I64, 0);
    ctx.builder.ins().store(
        MemFlags::new(),
        tag_zero,
        state_ptr,
        i32::try_from(tag_offset).unwrap_or(0),
    );

    // Store each capture into its corresponding state field.
    // Captures correspond to the async function's params (Local(1), Local(2), ...).
    // State field names are __local_1, __local_2, etc.
    for (i, cap_op) in capture_operands.iter().enumerate() {
        let param_idx = i + 1; // Local(1), Local(2), ...
        let field_name = format!("__local_{param_idx}");

        // Find the state field by name.
        let field_pos = poll_info
            .state_fields
            .iter()
            .position(|(name, _)| name.as_str() == field_name);
        let Some(field_idx) = field_pos else { continue };
        if field_idx >= state_layout.field_offsets.len() {
            continue;
        }
        let field_offset = state_layout.field_offsets[field_idx];
        let field_ty = poll_info.state_fields[field_idx].1;

        if let Some(val) = emit_rvalue::emit_operand(ctx, cap_op, ecx, module) {
            if abi::is_scalar(field_ty, table) {
                ctx.builder.ins().store(
                    MemFlags::new(),
                    val,
                    state_ptr,
                    i32::try_from(field_offset).unwrap_or(0),
                );
            } else {
                // Aggregate capture: memcpy from the value address.
                let field_layout = ctx.layouts.layout_of(field_ty, table);
                let dest = ctx
                    .builder
                    .ins()
                    .iadd_imm(state_ptr, i64::from(field_offset));
                let config = module.target_config();
                ctx.builder.emit_small_memory_copy(
                    config,
                    dest,
                    val,
                    u64::from(field_layout.size),
                    1,
                    1,
                    true,
                    MemFlags::new(),
                );
            }
        }
    }

    // Step 8: Get poll function pointer.
    let poll_func_ref = module.declare_func_in_func(*poll_func_id, ctx.builder.func);
    let poll_fn_ptr = ctx.builder.ins().func_addr(ctx.ptr_type, poll_func_ref);

    // Step 9: Compute result_size and result_offset.
    let dest_ty_id = ctx.local_type_id(destination.local);
    let inner_ty = match table.resolve(dest_ty_id) {
        Ty::Future { inner } => *inner,
        _ => dest_ty_id,
    };
    let result_layout = ctx.layouts.layout_of(inner_ty, table);
    let result_size_val = ctx
        .builder
        .ins()
        .iconst(types::I64, i64::from(result_layout.size));

    // Find __result field offset in the state struct.
    let result_field_idx = poll_info
        .state_fields
        .iter()
        .position(|(name, _)| name.as_str() == "__result")
        .unwrap_or(2); // __result is typically field index 2
    let result_offset = if result_field_idx < state_layout.field_offsets.len() {
        state_layout.field_offsets[result_field_idx]
    } else {
        0
    };
    let result_offset_val = ctx
        .builder
        .ins()
        .iconst(types::I64, i64::from(result_offset));

    // Step 10: Call __spark_spawn_coop(poll_fn_ptr, state_ptr, state_size, result_size, result_offset) -> ptr.
    let spawn_coop_ref = module.declare_func_in_func(ctx.stubs.spawn_coop, ctx.builder.func);
    let spawn_call = ctx.builder.ins().call(
        spawn_coop_ref,
        &[
            poll_fn_ptr,
            state_ptr,
            state_size_val,
            result_size_val,
            result_offset_val,
        ],
    );
    let future_ptr = ctx.builder.inst_results(spawn_call)[0];
    if destination.projections.is_empty() {
        ctx.write_local(destination.local, future_ptr);
    }

    let target_block = ctx.clif_block(target);
    ctx.builder.ins().jump(target_block, &[]);
    Some(())
}

/// Emits an `await` call: passes the future pointer to `__spark_await` and
/// loads the result value from the returned buffer pointer.
fn emit_await_call<M: Module>(
    ctx: &mut FunctionContext<'_>,
    args: &[Operand],
    destination: &Place,
    target: BlockId,
    ecx: &EmitCtx<'_>,
    module: &mut M,
) {
    let table = ecx.table;

    // args[0] is the Future[T] pointer.
    let future_val = args
        .first()
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, ecx, module));

    let Some(future_ptr) = future_val else {
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    };

    // Call __spark_await(future_ptr) -> ptr (pointer to result buffer)
    let func_ref = module.declare_func_in_func(ctx.stubs.await_future, ctx.builder.func);
    let call = ctx.builder.ins().call(func_ref, &[future_ptr]);
    let result_ptr = ctx.builder.inst_results(call)[0];

    // Load the result value from the buffer based on the destination type.
    let dest_ty_id = ctx.local_type_id(destination.local);
    if let Some(clif_ty) = crate::abi::spark_type_to_clif(dest_ty_id, table, ctx.ptr_type) {
        let result_val = ctx
            .builder
            .ins()
            .load(clif_ty, MemFlags::new(), result_ptr, 0);
        if destination.projections.is_empty() {
            ctx.write_local(destination.local, result_val);
        }
    }

    let target_block = ctx.clif_block(target);
    ctx.builder.ins().jump(target_block, &[]);
}

/// Emits a `__spark_try_poll` call: passes the future pointer to `__spark_try_poll`
/// and stores the returned result pointer (null if pending, non-null if ready).
fn emit_try_poll_call<M: Module>(
    ctx: &mut FunctionContext<'_>,
    args: &[Operand],
    destination: &Place,
    target: BlockId,
    ecx: &EmitCtx<'_>,
    module: &mut M,
) {
    // args[0] is the future pointer (opaque i64/ptr).
    let future_val = args
        .first()
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, ecx, module));

    let Some(future_ptr) = future_val else {
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    };

    // Call __spark_try_poll(future_ptr) -> ptr (null = pending, non-null = result pointer)
    let func_ref = module.declare_func_in_func(ctx.stubs.try_poll, ctx.builder.func);
    let call = ctx.builder.ins().call(func_ref, &[future_ptr]);
    let result_ptr = ctx.builder.inst_results(call)[0];

    // Store the result pointer to the destination (the poll function checks for null).
    if destination.projections.is_empty() {
        ctx.write_local(destination.local, result_ptr);
    }

    let target_block = ctx.clif_block(target);
    ctx.builder.ins().jump(target_block, &[]);
}

/// Emits a built-in method call, dispatching by receiver type and method name.
///
/// Method calls arrive as `Call` terminators with args[0] = receiver, args[1..] = method args.
/// We resolve the receiver type and dispatch to the appropriate runtime stub.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn emit_builtin_method<M: Module>(
    ctx: &mut FunctionContext<'_>,
    method_name: spark_intern::Symbol,
    args: &[Operand],
    destination: &Place,
    target: BlockId,
    ecx: &EmitCtx<'_>,
    module: &mut M,
) {
    let table = ecx.table;

    // args[0] is the receiver. Read its type to determine dispatch.
    let recv_ty_id = match &args[0] {
        spark_mir::Operand::Copy(place) | spark_mir::Operand::Move(place) => {
            ctx.local_type_id(place.local)
        }
        spark_mir::Operand::Constant(_) => {
            // Constant receiver — shouldn't happen for method calls
            let target_block = ctx.clif_block(target);
            ctx.builder.ins().jump(target_block, &[]);
            return;
        }
    };

    let (_, stripped_ty) = clif_types::unwrap_mutable(recv_ty_id, table);

    // Read the receiver pointer (no rc_inc — we're operating on it, not copying).
    let recv_ptr = match &args[0] {
        spark_mir::Operand::Copy(place) | spark_mir::Operand::Move(place) => {
            emit_rvalue::emit_place_read(ctx, place, table, module)
        }
        spark_mir::Operand::Constant(_) => ctx.builder.ins().iconst(ctx.ptr_type, 0),
    };

    // Helper: get the recv local for write-back after COW
    let recv_local = match &args[0] {
        spark_mir::Operand::Copy(place) | spark_mir::Operand::Move(place)
            if place.projections.is_empty() =>
        {
            Some(place.local)
        }
        _ => None,
    };

    // Is this a mutating method?
    let is_mutating =
        method_name == kw::APPEND || method_name == kw::REMOVE || method_name == kw::ADD;

    let unary_stub = match (method_name, stripped_ty) {
        (kw::LEN, Ty::List(_)) => Some(ctx.stubs.list_len),
        (kw::LEN, Ty::Map(_, _)) => Some(ctx.stubs.map_len),
        (kw::LEN, Ty::Set(_)) => Some(ctx.stubs.set_len),
        (kw::LEN, Ty::String) => Some(ctx.stubs.string_len),
        (kw::LINES, Ty::String) => Some(ctx.stubs.string_lines),
        (kw::SPLIT_WHITESPACE, Ty::String) => Some(ctx.stubs.string_split_whitespace),
        (kw::TRIM, Ty::String) => Some(ctx.stubs.string_trim),
        (kw::SECTIONS, Ty::String) => Some(ctx.stubs.string_sections),
        (kw::CHARS, Ty::String) => Some(ctx.stubs.string_chars),
        _ => None,
    };
    if let Some(stub_id) = unary_stub {
        let func_ref = module.declare_func_in_func(stub_id, ctx.builder.func);
        let call = ctx.builder.ins().call(func_ref, &[recv_ptr]);
        let result = ctx.builder.inst_results(call)[0];
        if destination.projections.is_empty() {
            ctx.write_local(destination.local, result);
        }
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    }

    if let (kw::SPLIT, Ty::String, [_, Operand::Copy(delimiter) | Operand::Move(delimiter)]) =
        (method_name, stripped_ty, args)
    {
        let delimiter = emit_rvalue::emit_place_read(ctx, delimiter, table, module);
        let stub = module.declare_func_in_func(ctx.stubs.string_split, ctx.builder.func);
        let call = ctx.builder.ins().call(stub, &[recv_ptr, delimiter]);
        let result = ctx.builder.inst_results(call)[0];
        if destination.projections.is_empty() {
            ctx.write_local(destination.local, result);
        }
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    }

    // --- CONTAINS ---
    if method_name == kw::CONTAINS {
        // args[1] is the element/key to search for
        let arg_val = args
            .get(1)
            .and_then(|a| emit_rvalue::emit_operand(ctx, a, ecx, module));

        if let Some(val) = arg_val {
            let (stub_id, arg_ty) = match stripped_ty {
                Ty::List(elem) => (ctx.stubs.list_contains, *elem),
                Ty::Map(k, _) => (ctx.stubs.map_contains, *k),
                Ty::Set(elem) => (ctx.stubs.set_contains, *elem),
                _ => {
                    let target_block = ctx.clif_block(target);
                    ctx.builder.ins().jump(target_block, &[]);
                    return;
                }
            };
            // Store the value to a temp slot to get its address.
            let val_addr = emit_rvalue::store_scalar_to_temp(ctx, val, arg_ty, table);
            let func_ref = module.declare_func_in_func(stub_id, ctx.builder.func);
            let call = ctx.builder.ins().call(func_ref, &[recv_ptr, val_addr]);
            let result = ctx.builder.inst_results(call)[0];
            if destination.projections.is_empty() {
                ctx.write_local(destination.local, result);
            }
        }
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    }

    // --- MUTATING METHODS: APPEND, REMOVE, ADD ---
    if is_mutating {
        // COW: call the appropriate cow function and write back the new pointer.
        let cow_ptr = emit_cow(ctx, recv_ptr, stripped_ty, table, module);
        if let Some(local) = recv_local {
            ctx.write_local(local, cow_ptr);
        }

        match (stripped_ty, method_name) {
            // List.append(item)
            (Ty::List(elem), _) if method_name == kw::APPEND => {
                let elem_ty = *elem;
                if let Some(arg) = args.get(1) {
                    if let Some(elem_addr) =
                        emit_rvalue::emit_operand_addr(ctx, arg, elem_ty, ecx, module)
                    {
                        let func_ref =
                            module.declare_func_in_func(ctx.stubs.list_push, ctx.builder.func);
                        ctx.builder.ins().call(func_ref, &[cow_ptr, elem_addr]);
                    }
                }
            }
            // List.remove(index)
            (Ty::List(_), _) if method_name == kw::REMOVE => {
                if let Some(arg) = args.get(1) {
                    if let Some(idx_val) = emit_rvalue::emit_operand(ctx, arg, ecx, module) {
                        let func_ref =
                            module.declare_func_in_func(ctx.stubs.list_remove, ctx.builder.func);
                        ctx.builder.ins().call(func_ref, &[cow_ptr, idx_val]);
                    }
                }
            }
            // Set.add(item)
            (Ty::Set(elem), _) if method_name == kw::ADD => {
                let elem_ty = *elem;
                if let Some(arg) = args.get(1) {
                    if let Some(elem_addr) =
                        emit_rvalue::emit_operand_addr(ctx, arg, elem_ty, ecx, module)
                    {
                        let func_ref =
                            module.declare_func_in_func(ctx.stubs.set_insert, ctx.builder.func);
                        ctx.builder.ins().call(func_ref, &[cow_ptr, elem_addr]);
                    }
                }
            }
            // Set.remove(item)
            (Ty::Set(elem), _) if method_name == kw::REMOVE => {
                let elem_ty = *elem;
                if let Some(arg) = args.get(1) {
                    if let Some(elem_addr) =
                        emit_rvalue::emit_operand_addr(ctx, arg, elem_ty, ecx, module)
                    {
                        let func_ref =
                            module.declare_func_in_func(ctx.stubs.set_remove, ctx.builder.func);
                        ctx.builder.ins().call(func_ref, &[cow_ptr, elem_addr]);
                    }
                }
            }
            // Map.remove(key)
            (Ty::Map(k, _), _) if method_name == kw::REMOVE => {
                let key_ty = *k;
                if let Some(arg) = args.get(1) {
                    if let Some(key_addr) =
                        emit_rvalue::emit_operand_addr(ctx, arg, key_ty, ecx, module)
                    {
                        let func_ref =
                            module.declare_func_in_func(ctx.stubs.map_remove, ctx.builder.func);
                        ctx.builder.ins().call(func_ref, &[cow_ptr, key_addr]);
                    }
                }
            }
            _ => {} // Unsupported combination — silently skip
        }

        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    }

    // --- CHANNEL METHODS: send, recv, tryRecv, close ---
    if matches!(stripped_ty, Ty::Channel { .. }) {
        let inner_ty = if let Ty::Channel { inner } = stripped_ty {
            *inner
        } else {
            unreachable!()
        };
        let elem_layout = ctx.layouts.layout_of(inner_ty, table);
        let elem_size = i64::from(elem_layout.size);
        let elem_size_val = ctx.builder.ins().iconst(types::I64, elem_size);

        if method_name == kw::SEND {
            // ch.send(value) → store value to temp, call __spark_channel_send(ch, &val, elem_size)
            if let Some(arg) = args.get(1) {
                if let Some(val_addr) =
                    emit_rvalue::emit_operand_addr(ctx, arg, inner_ty, ecx, module)
                {
                    let func_ref =
                        module.declare_func_in_func(ctx.stubs.channel_send, ctx.builder.func);
                    ctx.builder
                        .ins()
                        .call(func_ref, &[recv_ptr, val_addr, elem_size_val]);
                }
            }
        } else if method_name == kw::RECV {
            // ch.recv() → call __spark_channel_recv(ch, elem_size) → ptr to value
            let func_ref = module.declare_func_in_func(ctx.stubs.channel_recv, ctx.builder.func);
            let call = ctx.builder.ins().call(func_ref, &[recv_ptr, elem_size_val]);
            let result_ptr = ctx.builder.inst_results(call)[0];

            // Load the value from the returned pointer.
            if let Some(clif_ty) = abi::spark_type_to_clif(inner_ty, table, ctx.ptr_type) {
                let val = ctx
                    .builder
                    .ins()
                    .load(clif_ty, MemFlags::new(), result_ptr, 0);
                if destination.projections.is_empty() {
                    ctx.write_local(destination.local, val);
                }
            }
        } else if method_name == kw::TRY_RECV {
            // ch.tryRecv() → runtime writes Optional directly to dest StackSlot.
            // __spark_channel_try_recv(ch, elem_size, dest_ptr, payload_offset)
            if destination.projections.is_empty() {
                if let Some(dest_addr) = ctx.local_stack_addr(destination.local) {
                    let payload_offset = (1 + elem_layout.align - 1) & !(elem_layout.align - 1);
                    let payload_offset_val = ctx
                        .builder
                        .ins()
                        .iconst(types::I64, i64::from(payload_offset));
                    let func_ref =
                        module.declare_func_in_func(ctx.stubs.channel_try_recv, ctx.builder.func);
                    ctx.builder.ins().call(
                        func_ref,
                        &[recv_ptr, elem_size_val, dest_addr, payload_offset_val],
                    );
                }
            }
        } else if method_name == kw::CLOSE {
            // ch.close() → call __spark_channel_close(ch)
            let func_ref = module.declare_func_in_func(ctx.stubs.channel_close, ctx.builder.func);
            ctx.builder.ins().call(func_ref, &[recv_ptr]);
        }

        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    }

    // Fallback: unknown method — jump to continuation
    let target_block = ctx.clif_block(target);
    ctx.builder.ins().jump(target_block, &[]);
}

/// Emits a call to an extern (FFI) function.
///
/// Uses the function's name as the linker symbol. Builds a Cranelift
/// signature from the function's type, declares it as an import, and
/// emits the call instruction.
#[allow(clippy::too_many_arguments)]
fn emit_extern_call<M: Module>(
    ctx: &mut FunctionContext<'_>,
    callee_def_id: spark_resolve::DefId,
    info: &spark_resolve::DefinitionInfo,
    args: &[Operand],
    destination: &Place,
    target: BlockId,
    ecx: &EmitCtx<'_>,
    module: &mut M,
) {
    let table = ecx.table;
    let symbol_name = info.name.as_str();

    // Evaluate arguments
    let arg_vals: Vec<Value> = args
        .iter()
        .filter_map(|arg| emit_rvalue::emit_operand(ctx, arg, ecx, module))
        .collect();

    // Build Cranelift signature from the function's type in the TypeTable
    let call_conv = module.isa().default_call_conv();
    let mut sig = Signature::new(call_conv);

    if let Some(fn_type_id) = table.get_def_type(callee_def_id) {
        if let Ty::Function { params, ret, .. } = table.resolve(fn_type_id) {
            for param_ty in params {
                if let Some(clif_ty) = abi::spark_type_to_clif(*param_ty, table, ctx.ptr_type) {
                    sig.params.push(AbiParam::new(clif_ty));
                }
            }
            if let Some(clif_ret) = abi::spark_type_to_clif(*ret, table, ctx.ptr_type) {
                sig.returns.push(AbiParam::new(clif_ret));
            }
        }
    }

    // Declare the function as an import (idempotent in Cranelift)
    let Ok(func_id) = module.declare_function(symbol_name, Linkage::Import, &sig) else {
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    };

    let func_ref = module.declare_func_in_func(func_id, ctx.builder.func);
    let call = ctx.builder.ins().call(func_ref, &arg_vals);

    if let Some(result_val) = ctx.builder.inst_results(call).first().copied() {
        if destination.projections.is_empty() {
            ctx.write_local(destination.local, result_val);
        }
    }

    let target_block = ctx.clif_block(target);
    ctx.builder.ins().jump(target_block, &[]);
}

/// Emits COW (copy-on-write) for a mutable collection receiver.
///
/// Returns the (possibly new) pointer after COW.
// TAG [SPARK-115]
fn emit_cow<M: Module>(
    ctx: &mut FunctionContext<'_>,
    recv_ptr: Value,
    stripped_ty: &Ty,
    _table: &TypeTable,
    module: &mut M,
) -> Value {
    match stripped_ty {
        Ty::List(_) => {
            let func_ref = module.declare_func_in_func(ctx.stubs.list_cow, ctx.builder.func);
            let call = ctx.builder.ins().call(func_ref, &[recv_ptr]);
            ctx.builder.inst_results(call)[0]
        }
        Ty::Map(_, _) => {
            let func_ref = module.declare_func_in_func(ctx.stubs.map_cow, ctx.builder.func);
            let call = ctx.builder.ins().call(func_ref, &[recv_ptr]);
            ctx.builder.inst_results(call)[0]
        }
        Ty::Set(_) => {
            let func_ref = module.declare_func_in_func(ctx.stubs.set_cow, ctx.builder.func);
            let call = ctx.builder.ins().call(func_ref, &[recv_ptr]);
            ctx.builder.inst_results(call)[0]
        }
        _ => recv_ptr,
    }
}

/// Emits a direct function call via `FunctionRef`.
fn emit_direct_call<M: Module>(
    ctx: &mut FunctionContext<'_>,
    def_id: spark_resolve::DefId,
    arg_vals: &[Value],
    func_ids: &rustc_hash::FxHashMap<spark_resolve::DefId, FuncId>,
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

/// Emits a direct function call with sret (aggregate return).
///
/// The `sret_addr` pointer is prepended to the argument list. The callee
/// writes the return value directly to this address.
fn emit_direct_call_sret<M: Module>(
    ctx: &mut FunctionContext<'_>,
    def_id: spark_resolve::DefId,
    sret_addr: Value,
    arg_vals: &[Value],
    func_ids: &rustc_hash::FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
    diagnostics: &mut Diagnostics,
) {
    if let Some(&func_id) = func_ids.get(&def_id) {
        let func_ref = module.declare_func_in_func(func_id, ctx.builder.func);
        let mut full_args = vec![sret_addr];
        full_args.extend_from_slice(arg_vals);
        ctx.builder.ins().call(func_ref, &full_args);
    } else {
        diagnostics.warning(format!(
            "call to unknown function DefId({})",
            def_id.as_u32()
        ));
    }
}

/// Emits an indirect function call through a fat pointer `(fn_ptr, env_ptr)`.
///
/// `func_val` is the address of the fat pointer (a stack slot address).
/// We load `fn_ptr` from offset 0 and `env_ptr` from offset `ptr_size`,
/// then call `fn_ptr(env_ptr, arg1, arg2, ...)`.
fn emit_indirect_call<M: Module>(
    ctx: &mut FunctionContext<'_>,
    func_val: Option<Value>,
    arg_vals: &[Value],
    destination: &Place,
    table: &TypeTable,
    module: &M,
) -> Option<Value> {
    let fat_ptr_addr = func_val?;

    // Load fn_ptr and env_ptr from the fat pointer.
    let fn_ptr = ctx.builder.ins().load(
        ctx.ptr_type,
        cranelift_codegen::ir::MemFlags::new(),
        fat_ptr_addr,
        0,
    );
    let env_ptr = ctx.builder.ins().load(
        ctx.ptr_type,
        cranelift_codegen::ir::MemFlags::new(),
        fat_ptr_addr,
        ctx.ptr_type.bytes() as i32,
    );

    // Build signature: (env_ptr, ...args) -> ret
    let dest_ty = ctx.local_type_id(destination.local);
    let mut sig = Signature::new(module.isa().default_call_conv());
    sig.params.push(AbiParam::new(ctx.ptr_type)); // env_ptr
    for av in arg_vals {
        sig.params
            .push(AbiParam::new(ctx.builder.func.dfg.value_type(*av)));
    }
    if let Some(ret_clif) = abi::spark_type_to_clif(dest_ty, table, ctx.ptr_type) {
        sig.returns.push(AbiParam::new(ret_clif));
    }

    let mut full_args = vec![env_ptr];
    full_args.extend_from_slice(arg_vals);

    let sig_ref = ctx.builder.import_signature(sig);
    let call = ctx.builder.ins().call_indirect(sig_ref, fn_ptr, &full_args);
    ctx.builder.inst_results(call).first().copied()
}

/// Emits an indirect function call through a fat pointer with sret (aggregate return).
///
/// Like [`emit_indirect_call`], but uses the sret calling convention:
/// `fn_ptr(sret_ptr, env_ptr, arg1, arg2, ...)` with no return value.
fn emit_indirect_call_sret<M: Module>(
    ctx: &mut FunctionContext<'_>,
    func_val: Option<Value>,
    sret_addr: Value,
    arg_vals: &[Value],
    _destination: &Place,
    _table: &TypeTable,
    module: &M,
) {
    let Some(fat_ptr_addr) = func_val else { return };

    // Load fn_ptr and env_ptr from the fat pointer.
    let fn_ptr = ctx.builder.ins().load(
        ctx.ptr_type,
        cranelift_codegen::ir::MemFlags::new(),
        fat_ptr_addr,
        0,
    );
    let env_ptr = ctx.builder.ins().load(
        ctx.ptr_type,
        cranelift_codegen::ir::MemFlags::new(),
        fat_ptr_addr,
        ctx.ptr_type.bytes() as i32,
    );

    // Build signature: (sret_ptr, env_ptr, ...args) -> void
    let mut sig = Signature::new(module.isa().default_call_conv());
    sig.params.push(AbiParam::new(ctx.ptr_type)); // sret_ptr
    sig.params.push(AbiParam::new(ctx.ptr_type)); // env_ptr
    for av in arg_vals {
        sig.params
            .push(AbiParam::new(ctx.builder.func.dfg.value_type(*av)));
    }
    // No return value — callee writes directly to sret_ptr.

    let mut full_args = vec![sret_addr, env_ptr];
    full_args.extend_from_slice(arg_vals);

    let sig_ref = ctx.builder.import_signature(sig);
    ctx.builder.ins().call_indirect(sig_ref, fn_ptr, &full_args);
}
