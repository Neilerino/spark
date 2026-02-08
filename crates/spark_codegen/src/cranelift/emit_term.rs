//! Terminator lowering: MIR terminators → Cranelift IR.
//!
//! Handles `Goto`, `Return`, `SwitchInt`, `Call`, and `Unreachable` terminators.

use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{AbiParam, InstBuilder, Signature, TrapCode, Value};
use cranelift_module::{FuncId, Module};
use rustc_hash::FxHashMap;
use spark_diagnostics::Diagnostics;
use spark_intern::kw;
use spark_mir::{BlockId, Operand, Place, Terminator};
use spark_resolve::{DefinitionKind, DefinitionTable};
use spark_types::{Ty, TypeTable};

use super::context::FunctionContext;
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
    // Intercept built-in `print` calls and method calls for type-based dispatch.
    // The func operand is typically `Copy(place)` of a local that was assigned
    // a `Constant::FunctionRef(def_id)`, so we resolve the DefId from the MIR.
    if let Some(callee_def_id) = resolve_callee_def_id(func, ctx) {
        if let Some(info) = definitions.get(callee_def_id) {
            if matches!(info.kind, DefinitionKind::BuiltinFunction) && info.name == kw::PRINT {
                emit_builtin_print(ctx, args, target, table, definitions, func_ids, module);
                return;
            }
            if matches!(info.kind, DefinitionKind::BuiltinMethod) {
                emit_builtin_method(ctx, info.name, args, destination, target, table, definitions, func_ids, module);
                return;
            }
        }
    }

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
    module: &mut M,
) {
    // Evaluate the single argument.
    let arg_val = args
        .first()
        .and_then(|arg| emit_rvalue::emit_operand(ctx, arg, table, definitions, func_ids, module));

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
            emit_rvalue::emit_operand(ctx, a, table, definitions, func_ids, module)
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
                        ctx, arg, elem_ty, table, definitions, func_ids, module,
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
                    if let Some(idx_val) = emit_rvalue::emit_operand(ctx, arg, table, definitions, func_ids, module) {
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
                        ctx, arg, elem_ty, table, definitions, func_ids, module,
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
                        ctx, arg, elem_ty, table, definitions, func_ids, module,
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
                        ctx, arg, key_ty, table, definitions, func_ids, module,
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
