//! Terminator lowering: MIR terminators → Cranelift IR.
//!
//! Handles `Goto`, `Return`, `SwitchInt`, `Call`, and `Unreachable` terminators.

use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{AbiParam, InstBuilder, MemFlags, Signature, TrapCode, Value};
use cranelift_module::{FuncId, Module};
use rustc_hash::FxHashMap;
use spark_diagnostics::Diagnostics;
use spark_intern::kw;
use spark_mir::{BlockId, Operand, Place, Terminator};
use spark_resolve::{DefinitionKind, DefinitionTable};
use spark_types::{Ty, TypeTable};

use super::context::{self, FunctionContext};
use super::emit_rvalue;
use super::types as clif_types;
use crate::abi;

/// Emits a MIR terminator instruction.
#[allow(clippy::too_many_arguments)]
pub fn emit_terminator<M: Module>(
    ctx: &mut FunctionContext<'_>,
    terminator: &Terminator,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
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
            } else if let Some(sret_ptr) = ctx.sret_ptr {
                // Aggregate return: copy Local(0) data to the sret pointer.
                if let context::LocalStorage::StackSlot(slot, size) =
                    ctx.local_storage(spark_mir::Local::RETURN_PLACE)
                {
                    let src = ctx.builder.ins().stack_addr(ctx.ptr_type, slot, 0);
                    let config = module.target_config();
                    ctx.builder.emit_small_memory_copy(
                        config, sret_ptr, src, u64::from(size),
                        1, 1, true, MemFlags::new(),
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
            emit_switch_int(ctx, discriminant, targets, *otherwise, table, definitions, func_ids, thunk_ids, module);
        }

        Terminator::Call {
            func,
            args,
            destination,
            target,
            span: _,
        } => {
            emit_call(ctx, func, args, destination, *target, table, definitions, func_ids, thunk_ids, module, diagnostics);
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
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
) {
    let disc_val =
        emit_rvalue::emit_operand(ctx, discriminant, table, definitions, func_ids, thunk_ids, module)
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
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
    diagnostics: &mut Diagnostics,
) {
    // Intercept built-in `print` calls and method calls for type-based dispatch.
    // The func operand is typically `Copy(place)` of a local that was assigned
    // a `Constant::FunctionRef(def_id)`, so we resolve the DefId from the MIR.
    if let Some(callee_def_id) = resolve_callee_def_id(func, ctx) {
        if let Some(info) = definitions.get(callee_def_id) {
            if matches!(info.kind, DefinitionKind::BuiltinFunction) && info.name == kw::PRINT {
                emit_builtin_print(ctx, args, target, table, definitions, func_ids, thunk_ids, module);
                return;
            }
            if matches!(info.kind, DefinitionKind::BuiltinFunction)
                && (info.name == kw::SORT || info.name == kw::TAKE || info.name == kw::REVERSE)
            {
                emit_builtin_list_fn(ctx, info.name, args, destination, target, table, definitions, func_ids, thunk_ids, module);
                return;
            }
            if matches!(info.kind, DefinitionKind::BuiltinMethod) {
                emit_builtin_method(ctx, info.name, args, destination, target, table, definitions, func_ids, thunk_ids, module);
                return;
            }
            if matches!(info.kind, DefinitionKind::BuiltinFunction)
                && (info.name == kw::PARALLEL_FOREACH || info.name == kw::PARALLEL_MAP)
            {
                emit_parallel_call(ctx, info.name, args, destination, target, table, definitions, func_ids, thunk_ids, module);
                return;
            }
        }
    }

    let func_val = emit_rvalue::emit_operand(ctx, func, table, definitions, func_ids, thunk_ids, module);

    let arg_vals: Vec<Value> = args
        .iter()
        .filter_map(|arg| emit_rvalue::emit_operand(ctx, arg, table, definitions, func_ids, thunk_ids, module))
        .collect();

    // Check if the callee returns an aggregate (sret convention).
    let dest_ty = ctx.local_type_id(destination.local);
    let sret = abi::needs_sret(dest_ty, table);

    let call_result = if sret {
        // Aggregate return: pass destination address as sret pointer.
        // The callee writes directly to this address; no return value.
        let sret_addr = ctx.local_stack_addr(destination.local)
            .unwrap_or_else(|| ctx.builder.ins().iconst(ctx.ptr_type, 0));
        match func {
            spark_mir::Operand::Constant(spark_mir::Constant::FunctionRef(def_id)) => {
                emit_direct_call_sret(ctx, *def_id, sret_addr, &arg_vals, func_ids, module, diagnostics);
            }
            _ => {
                emit_indirect_call_sret(ctx, func_val, sret_addr, &arg_vals, destination, table, module);
            }
        }
        None // data already written to sret_addr
    } else {
        match func {
            spark_mir::Operand::Constant(spark_mir::Constant::FunctionRef(def_id)) => {
                emit_direct_call(ctx, *def_id, &arg_vals, func_ids, module, diagnostics)
            }
            _ => emit_indirect_call(ctx, func_val, &arg_vals, destination, table, module),
        }
    };

    if let Some(result_val) = call_result {
        if destination.projections.is_empty() {
            ctx.write_local(destination.local, result_val);
        }
    }

    let target_block = ctx.clif_block(target);
    ctx.builder.ins().jump(target_block, &[]);
}

/// Resolves the [`DefId`] of a function reference from a call operand.
///
/// The MIR typically stores function references as `Copy(Place::from_local(l))`
/// where `l` was assigned `Rvalue::Use(Operand::Constant(Constant::FunctionRef(def_id)))`.
/// This scans the MIR blocks to recover the `DefId`.
fn resolve_callee_def_id(
    func: &Operand,
    ctx: &FunctionContext<'_>,
) -> Option<spark_resolve::DefId> {
    // Direct constant case (rare but handle it).
    if let spark_mir::Operand::Constant(spark_mir::Constant::FunctionRef(def_id)) = func {
        return Some(*def_id);
    }

    // Copy/Move of a local — scan statements for the assignment.
    let local = match func {
        spark_mir::Operand::Copy(place) | spark_mir::Operand::Move(place) => {
            if place.projections.is_empty() {
                place.local
            } else {
                return None;
            }
        }
        spark_mir::Operand::Constant(_) => return None,
    };

    for block in &ctx.mir_func.blocks {
        for stmt in &block.statements {
            if let spark_mir::Statement::Assign { place, rvalue, .. } = stmt {
                if place.local == local && place.projections.is_empty() {
                    if let spark_mir::Rvalue::Use(spark_mir::Operand::Constant(
                        spark_mir::Constant::FunctionRef(def_id),
                    )) = rvalue
                    {
                        return Some(*def_id);
                    }
                }
            }
        }
    }

    None
}

/// Emits a built-in `print` call, dispatching to the correct runtime stub
/// based on the argument's type.
#[allow(clippy::too_many_arguments)]
fn emit_builtin_print<M: Module>(
    ctx: &mut FunctionContext<'_>,
    args: &[Operand],
    target: BlockId,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
) {
    // Evaluate the single argument.
    let arg_val = args
        .first()
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, table, definitions, func_ids, thunk_ids, module));

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
#[allow(clippy::too_many_arguments)]
fn emit_builtin_list_fn<M: Module>(
    ctx: &mut FunctionContext<'_>,
    fn_name: spark_intern::Symbol,
    args: &[Operand],
    destination: &Place,
    target: BlockId,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
) {
    // Evaluate the list argument (args[0]).
    let list_val = args
        .first()
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, table, definitions, func_ids, thunk_ids, module));

    let Some(list_ptr) = list_val else {
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    };

    if fn_name == kw::SORT {
        let func_ref = module.declare_func_in_func(ctx.stubs.list_sort, ctx.builder.func);
        let call = ctx.builder.ins().call(func_ref, &[list_ptr]);
        let result = ctx.builder.inst_results(call)[0];
        if destination.projections.is_empty() {
            ctx.write_local(destination.local, result);
        }
    } else if fn_name == kw::TAKE {
        // args[1] is the count
        let count_val = args
            .get(1)
            .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, table, definitions, func_ids, thunk_ids, module))
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
#[allow(clippy::too_many_arguments)]
fn emit_parallel_call<M: Module>(
    ctx: &mut FunctionContext<'_>,
    fn_name: spark_intern::Symbol,
    args: &[Operand],
    destination: &Place,
    target: BlockId,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
) {
    let is_map = fn_name == kw::PARALLEL_MAP;

    // Evaluate the list argument (args[0]).
    let list_val = args
        .first()
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, table, definitions, func_ids, thunk_ids, module));

    let Some(list_ptr) = list_val else {
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    };

    // Evaluate the callback argument (args[1]) — a fat pointer aggregate.
    let cb_val = args
        .get(1)
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, table, definitions, func_ids, thunk_ids, module));

    let Some(fat_ptr_addr) = cb_val else {
        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    };

    // Load fn_ptr and env_ptr from the fat pointer.
    let fn_ptr = ctx.builder.ins().load(
        ctx.ptr_type,
        MemFlags::new(),
        fat_ptr_addr,
        0,
    );
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
    let elem_size_val = ctx.builder.ins().iconst(types::I64, i64::from(elem_layout.size));

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
            let p = params.first().copied().unwrap_or(spark_types::TypeId::ERROR);
            (p, ret)
        }
        _ => (spark_types::TypeId::ERROR, spark_types::TypeId::ERROR),
    };

    let input_kind: u8 = if matches!(table.resolve(param_ty), Ty::Float(_)) { 1 } else { 0 };
    let output_kind: u8 = if matches!(table.resolve(ret_ty), Ty::Void) {
        2
    } else if matches!(table.resolve(ret_ty), Ty::Float(_)) {
        1
    } else {
        0
    };
    let cb_kind: u8 = input_kind | (output_kind << 2);
    let cb_kind_val = ctx.builder.ins().iconst(types::I8, i64::from(cb_kind));

    if is_map {
        // __spark_parallel_map(list, fn_ptr, env_ptr, elem_size, result_elem_size, cb_kind) -> ptr
        let result_elem_layout = ctx.layouts.layout_of(ret_ty, table);
        let result_elem_size_val = ctx.builder.ins().iconst(types::I64, i64::from(result_elem_layout.size));
        let func_ref = module.declare_func_in_func(ctx.stubs.parallel_map, ctx.builder.func);
        let call = ctx.builder.ins().call(func_ref, &[list_ptr, fn_ptr, env_ptr, elem_size_val, result_elem_size_val, cb_kind_val]);
        let result = ctx.builder.inst_results(call)[0];
        if destination.projections.is_empty() {
            ctx.write_local(destination.local, result);
        }
    } else {
        // __spark_parallel_foreach(list, fn_ptr, env_ptr, elem_size, cb_kind) -> void
        let func_ref = module.declare_func_in_func(ctx.stubs.parallel_foreach, ctx.builder.func);
        ctx.builder.ins().call(func_ref, &[list_ptr, fn_ptr, env_ptr, elem_size_val, cb_kind_val]);
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
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
) {
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
    let is_mutating = method_name == kw::APPEND
        || method_name == kw::REMOVE
        || method_name == kw::ADD;

    // --- LEN ---
    if method_name == kw::LEN {
        let stub_id = match stripped_ty {
            Ty::List(_) => ctx.stubs.list_len,
            Ty::Map(_, _) => ctx.stubs.map_len,
            Ty::Set(_) => ctx.stubs.set_len,
            Ty::String => ctx.stubs.string_len,
            _ => {
                let target_block = ctx.clif_block(target);
                ctx.builder.ins().jump(target_block, &[]);
                return;
            }
        };
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

    // --- CONTAINS ---
    if method_name == kw::CONTAINS {
        // args[1] is the element/key to search for
        let arg_val = args.get(1).and_then(|a| {
            emit_rvalue::emit_operand(ctx, a, table, definitions, func_ids, thunk_ids, module)
        });

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
                    if let Some(elem_addr) = emit_rvalue::emit_operand_addr(
                        ctx, arg, elem_ty, table, definitions, func_ids, thunk_ids, module,
                    ) {
                        let func_ref = module.declare_func_in_func(ctx.stubs.list_push, ctx.builder.func);
                        ctx.builder.ins().call(func_ref, &[cow_ptr, elem_addr]);
                    }
                }
            }
            // List.remove(index)
            (Ty::List(elem), _) if method_name == kw::REMOVE => {
                let elem_ty = *elem;
                if let Some(arg) = args.get(1) {
                    if let Some(idx_val) = emit_rvalue::emit_operand(ctx, arg, table, definitions, func_ids, thunk_ids, module) {
                        let elem_rc = if spark_drop::needs_rc(elem_ty, table) {
                            ctx.builder.ins().iconst(types::I8, 1)
                        } else {
                            ctx.builder.ins().iconst(types::I8, 0)
                        };
                        let func_ref = module.declare_func_in_func(ctx.stubs.list_remove, ctx.builder.func);
                        ctx.builder.ins().call(func_ref, &[cow_ptr, idx_val, elem_rc]);
                    }
                }
            }
            // Set.add(item)
            (Ty::Set(elem), _) if method_name == kw::ADD => {
                let elem_ty = *elem;
                if let Some(arg) = args.get(1) {
                    if let Some(elem_addr) = emit_rvalue::emit_operand_addr(
                        ctx, arg, elem_ty, table, definitions, func_ids, thunk_ids, module,
                    ) {
                        let func_ref = module.declare_func_in_func(ctx.stubs.set_insert, ctx.builder.func);
                        ctx.builder.ins().call(func_ref, &[cow_ptr, elem_addr]);
                    }
                }
            }
            // Set.remove(item)
            (Ty::Set(elem), _) if method_name == kw::REMOVE => {
                let elem_ty = *elem;
                if let Some(arg) = args.get(1) {
                    if let Some(elem_addr) = emit_rvalue::emit_operand_addr(
                        ctx, arg, elem_ty, table, definitions, func_ids, thunk_ids, module,
                    ) {
                        let elem_rc = if spark_drop::needs_rc(elem_ty, table) {
                            ctx.builder.ins().iconst(types::I8, 1)
                        } else {
                            ctx.builder.ins().iconst(types::I8, 0)
                        };
                        let func_ref = module.declare_func_in_func(ctx.stubs.set_remove, ctx.builder.func);
                        ctx.builder.ins().call(func_ref, &[cow_ptr, elem_addr, elem_rc]);
                    }
                }
            }
            // Map.remove(key)
            (Ty::Map(k, v), _) if method_name == kw::REMOVE => {
                let key_ty = *k;
                let val_ty = *v;
                if let Some(arg) = args.get(1) {
                    if let Some(key_addr) = emit_rvalue::emit_operand_addr(
                        ctx, arg, key_ty, table, definitions, func_ids, thunk_ids, module,
                    ) {
                        let key_rc = if spark_drop::needs_rc(key_ty, table) {
                            ctx.builder.ins().iconst(types::I8, 1)
                        } else {
                            ctx.builder.ins().iconst(types::I8, 0)
                        };
                        let val_rc = if spark_drop::needs_rc(val_ty, table) {
                            ctx.builder.ins().iconst(types::I8, 1)
                        } else {
                            ctx.builder.ins().iconst(types::I8, 0)
                        };
                        let func_ref = module.declare_func_in_func(ctx.stubs.map_remove, ctx.builder.func);
                        ctx.builder.ins().call(func_ref, &[cow_ptr, key_addr, key_rc, val_rc]);
                    }
                }
            }
            _ => {} // Unsupported combination — silently skip
        }

        let target_block = ctx.clif_block(target);
        ctx.builder.ins().jump(target_block, &[]);
        return;
    }

    // Fallback: unknown method — jump to continuation
    let target_block = ctx.clif_block(target);
    ctx.builder.ins().jump(target_block, &[]);
}

/// Emits COW (copy-on-write) for a mutable collection receiver.
///
/// Returns the (possibly new) pointer after COW.
fn emit_cow<M: Module>(
    ctx: &mut FunctionContext<'_>,
    recv_ptr: Value,
    stripped_ty: &Ty,
    table: &TypeTable,
    module: &mut M,
) -> Value {
    match stripped_ty {
        Ty::List(elem) => {
            let elem_rc = if spark_drop::needs_rc(*elem, table) {
                ctx.builder.ins().iconst(types::I8, 1)
            } else {
                ctx.builder.ins().iconst(types::I8, 0)
            };
            let func_ref = module.declare_func_in_func(ctx.stubs.list_cow, ctx.builder.func);
            let call = ctx.builder.ins().call(func_ref, &[recv_ptr, elem_rc]);
            ctx.builder.inst_results(call)[0]
        }
        Ty::Map(k, v) => {
            let key_rc = if spark_drop::needs_rc(*k, table) {
                ctx.builder.ins().iconst(types::I8, 1)
            } else {
                ctx.builder.ins().iconst(types::I8, 0)
            };
            let val_rc = if spark_drop::needs_rc(*v, table) {
                ctx.builder.ins().iconst(types::I8, 1)
            } else {
                ctx.builder.ins().iconst(types::I8, 0)
            };
            let func_ref = module.declare_func_in_func(ctx.stubs.map_cow, ctx.builder.func);
            let call = ctx.builder.ins().call(func_ref, &[recv_ptr, key_rc, val_rc]);
            ctx.builder.inst_results(call)[0]
        }
        Ty::Set(elem) => {
            // Set uses list_cow internally
            let elem_rc = if spark_drop::needs_rc(*elem, table) {
                ctx.builder.ins().iconst(types::I8, 1)
            } else {
                ctx.builder.ins().iconst(types::I8, 0)
            };
            let func_ref = module.declare_func_in_func(ctx.stubs.list_cow, ctx.builder.func);
            let call = ctx.builder.ins().call(func_ref, &[recv_ptr, elem_rc]);
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

/// Emits a direct function call with sret (aggregate return).
///
/// The `sret_addr` pointer is prepended to the argument list. The callee
/// writes the return value directly to this address.
fn emit_direct_call_sret<M: Module>(
    ctx: &mut FunctionContext<'_>,
    def_id: spark_resolve::DefId,
    sret_addr: Value,
    arg_vals: &[Value],
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
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
        sig.params.push(AbiParam::new(
            ctx.builder.func.dfg.value_type(*av),
        ));
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
        sig.params.push(AbiParam::new(
            ctx.builder.func.dfg.value_type(*av),
        ));
    }
    // No return value — callee writes directly to sret_ptr.

    let mut full_args = vec![sret_addr, env_ptr];
    full_args.extend_from_slice(arg_vals);

    let sig_ref = ctx.builder.import_signature(sig);
    ctx.builder.ins().call_indirect(sig_ref, fn_ptr, &full_args);
}
