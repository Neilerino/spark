//! Rvalue lowering: MIR rvalues → Cranelift IR instructions.
//!
//! Handles `Use`, `BinaryOp`, `UnaryOp`, `Ref`, `Aggregate`, `Discriminant`,
//! and `Cast` rvalues.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{InstBuilder, InstructionData, MemFlags, Value};
use cranelift_module::Module;
use spark_ast::{BinaryOp, UnaryOp};
use spark_diagnostics::Diagnostics;
use spark_mir::{AggregateKind, Constant, Local, Operand, Place, Projection, Rvalue};
use spark_resolve::DefinitionTable;
use spark_types::{FloatWidth, IntWidth, Ty, TypeId, TypeTable};

use super::context::{FunctionContext, LocalStorage};
use super::types as clif_types;
use crate::abi;

use rustc_hash::FxHashMap;
use cranelift_module::FuncId;

/// Emits an rvalue and returns the resulting Cranelift value.
///
/// For aggregate rvalues, writes directly to the destination and returns
/// the destination address.
#[allow(clippy::too_many_arguments)]
pub fn emit_rvalue<M: Module>(
    ctx: &mut FunctionContext<'_>,
    rvalue: &Rvalue,
    dest_local: Local,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    dtor_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    elem_dtor_ids: &FxHashMap<TypeId, FuncId>,
    module: &mut M,
    diagnostics: &mut Diagnostics,
) -> Option<Value> {
    match rvalue {
        Rvalue::Use(operand) => emit_operand(ctx, operand, table, definitions, func_ids, thunk_ids, module),

        Rvalue::BinaryOp(op, lhs, rhs) => {
            let lhs_val = emit_operand(ctx, lhs, table, definitions, func_ids, thunk_ids, module)?;
            let rhs_val = emit_operand(ctx, rhs, table, definitions, func_ids, thunk_ids, module)?;
            let result_ty = ctx.local_type_id(dest_local);
            Some(emit_binop(ctx, *op, lhs_val, rhs_val, result_ty, table))
        }

        Rvalue::UnaryOp(op, operand) => {
            let val = emit_operand(ctx, operand, table, definitions, func_ids, thunk_ids, module)?;
            let result_ty = ctx.local_type_id(dest_local);
            Some(emit_unaryop(ctx, *op, val, result_ty, table))
        }

        Rvalue::Ref { place, mutable: _ } => {
            // Take address of a place.
            Some(emit_place_addr(ctx, place, table, module))
        }

        Rvalue::Aggregate(kind, operands) => {
            emit_aggregate(ctx, kind, operands, dest_local, table, definitions, func_ids, thunk_ids, dtor_ids, elem_dtor_ids, module, diagnostics)
        }

        Rvalue::Discriminant(place) => {
            Some(emit_discriminant(ctx, place, table, module))
        }

        Rvalue::Cast { operand, target_ty } => {
            let val = emit_operand(ctx, operand, table, definitions, func_ids, thunk_ids, module)?;
            // Determine source type from the operand.
            let src_ty = operand_type(operand, ctx);
            Some(emit_cast(ctx, val, src_ty, *target_ty, table))
        }

        Rvalue::StringInterp(operands) => {
            emit_string_interp(ctx, operands, table, definitions, func_ids, thunk_ids, module)
        }

        Rvalue::ListRange { start, end, inclusive, step } => {
            let start_val = emit_operand(ctx, start, table, definitions, func_ids, thunk_ids, module)?;
            let end_val = emit_operand(ctx, end, table, definitions, func_ids, thunk_ids, module)?;
            let step_val = emit_operand(ctx, step, table, definitions, func_ids, thunk_ids, module)?;
            let inclusive_val = ctx.builder.ins().iconst(types::I8, i64::from(u8::from(*inclusive)));
            let func_ref = module.declare_func_in_func(ctx.stubs.list_from_range, ctx.builder.func);
            let call = ctx.builder.ins().call(func_ref, &[start_val, end_val, inclusive_val, step_val]);
            Some(ctx.builder.inst_results(call)[0])
        }

        Rvalue::RangeLen { start, end, inclusive, step } => {
            let start_val = emit_operand(ctx, start, table, definitions, func_ids, thunk_ids, module)?;
            let end_val = emit_operand(ctx, end, table, definitions, func_ids, thunk_ids, module)?;
            let step_val = emit_operand(ctx, step, table, definitions, func_ids, thunk_ids, module)?;
            let inclusive_val = ctx.builder.ins().iconst(types::I8, i64::from(u8::from(*inclusive)));
            let func_ref = module.declare_func_in_func(ctx.stubs.range_len, ctx.builder.func);
            let call = ctx.builder.ins().call(func_ref, &[start_val, end_val, inclusive_val, step_val]);
            Some(ctx.builder.inst_results(call)[0])
        }
    }
}

/// Emits an operand, returning the Cranelift value.
#[allow(clippy::too_many_arguments)]
pub fn emit_operand<M: Module>(
    ctx: &mut FunctionContext<'_>,
    operand: &Operand,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
) -> Option<Value> {
    match operand {
        Operand::Copy(place) => {
            let val = emit_place_read(ctx, place, table, module);
            // RC inc: when copying an RC'd value, increment its reference count.
            // Use the resolved place type (after projections) to avoid
            // incrementing non-RC values extracted from RC containers.
            let ty_id = if place.projections.is_empty() {
                ctx.local_type_id(place.local)
            } else {
                resolve_place_type(ctx, place, table)
            };
            if spark_drop::needs_rc(ty_id, table) {
                let func_ref = module.declare_func_in_func(ctx.stubs.rc_inc, ctx.builder.func);
                ctx.builder.ins().call(func_ref, &[val]);
            } else if matches!(table.resolve(ty_id), Ty::Function { .. }) {
                // Function copy: RC inc the env_ptr (second slot of the fat pointer).
                // val is the address of the fat pointer (fn_ptr, env_ptr).
                let env_ptr = ctx.builder.ins().load(
                    ctx.ptr_type,
                    MemFlags::new(),
                    val,
                    ctx.ptr_type.bytes() as i32,
                );
                let func_ref = module.declare_func_in_func(ctx.stubs.rc_inc, ctx.builder.func);
                ctx.builder.ins().call(func_ref, &[env_ptr]);
            }
            Some(val)
        }
        Operand::Move(place) => {
            // Move: no RC change — ownership transfers.
            Some(emit_place_read(ctx, place, table, module))
        }
        Operand::Constant(constant) => {
            emit_constant(ctx, constant, definitions, func_ids, thunk_ids, module)
        }
    }
}

/// Emits a constant value.
///
/// For `FunctionRef` constants, produces the address of a fat pointer
/// `(fn_ptr, env_ptr)` stored in a temporary stack slot. Lambdas use their
/// own func_id (they already have the env_ptr convention). Named functions
/// use their thunk (which accepts+ignores the env_ptr).
fn emit_constant<M: Module>(
    ctx: &mut FunctionContext<'_>,
    constant: &Constant,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
) -> Option<Value> {
    match constant {
        Constant::Int(v) => Some(ctx.builder.ins().iconst(types::I64, *v)),
        Constant::Float(v) => Some(ctx.builder.ins().f64const(*v)),
        Constant::Bool(v) => Some(ctx.builder.ins().iconst(types::I8, i64::from(*v))),
        Constant::Char(c) => Some(ctx.builder.ins().iconst(types::I32, i64::from(*c as u32))),
        Constant::Null => Some(ctx.builder.ins().iconst(ctx.ptr_type, 0)),
        Constant::Void => None, // Zero-sized — no value
        Constant::String(sym) => {
            // Create a data object for the string literal bytes.
            let name = format!(".str.{}", sym.as_raw());
            let bytes = sym.as_str().as_bytes();

            let Ok(data_id) = module
                .declare_data(&name, cranelift_module::Linkage::Local, false, false)
            else {
                return None;
            };

            let mut data_desc = cranelift_module::DataDescription::new();
            data_desc.define(bytes.to_vec().into_boxed_slice());
            // The same string literal may appear in multiple functions, so
            // `define_data` can encounter a `DuplicateDefinition` if the
            // data was already emitted by a prior function. That is harmless.
            match module.define_data(data_id, &data_desc) {
                Ok(()) | Err(cranelift_module::ModuleError::DuplicateDefinition(_)) => {}
                Err(_) => return None,
            }

            // Get pointer to static data and the length.
            let gv = module.declare_data_in_func(data_id, ctx.builder.func);
            let data_ptr = ctx.builder.ins().symbol_value(ctx.ptr_type, gv);
            let len = ctx.builder.ins().iconst(types::I64, bytes.len() as i64);

            // Call __spark_string_from_literal(ptr, len) to create a heap String.
            let func_ref = module.declare_func_in_func(ctx.stubs.string_from_literal, ctx.builder.func);
            let call = ctx.builder.ins().call(func_ref, &[data_ptr, len]);
            Some(ctx.builder.inst_results(call)[0])
        }
        Constant::IntrinsicRef(_) => {
            // Intrinsic references are handled directly by the Call terminator
            // emission, not as general constants. Return a null placeholder.
            Some(ctx.builder.ins().iconst(ctx.ptr_type, 0))
        }
        Constant::FunctionRef(def_id) => {
            // For lambdas, use their own func_id (they already have env_ptr convention).
            // For named functions, use the thunk (which accepts+ignores env_ptr).
            let is_lambda = definitions
                .get(*def_id)
                .map_or(false, |info| matches!(info.kind, spark_resolve::DefinitionKind::Lambda));

            let target_func_id = if is_lambda {
                func_ids.get(def_id).copied()
            } else {
                thunk_ids.get(def_id).copied().or_else(|| func_ids.get(def_id).copied())
            };

            // Always produce a fat pointer (fn_ptr, env_ptr) in a stack slot.
            // For builtins without a func_id, store (null, null) — the call
            // path intercepts builtins before the fat pointer is used.
            use cranelift_codegen::ir::{StackSlotData, StackSlotKind};
            let slot = ctx.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                ctx.ptr_type.bytes() * 2,
                0,
            ));
            let addr = ctx.builder.ins().stack_addr(ctx.ptr_type, slot, 0);

            if let Some(fid) = target_func_id {
                let func_ref = module.declare_func_in_func(fid, ctx.builder.func);
                let fn_ptr = ctx.builder.ins().func_addr(ctx.ptr_type, func_ref);
                let null = ctx.builder.ins().iconst(ctx.ptr_type, 0);
                ctx.builder.ins().store(MemFlags::new(), fn_ptr, addr, 0);
                ctx.builder.ins().store(MemFlags::new(), null, addr, ctx.ptr_type.bytes() as i32);
            } else {
                let null = ctx.builder.ins().iconst(ctx.ptr_type, 0);
                ctx.builder.ins().store(MemFlags::new(), null, addr, 0);
                ctx.builder.ins().store(MemFlags::new(), null, addr, ctx.ptr_type.bytes() as i32);
            }
            Some(addr)
        }
    }
}

/// Reads a value from a MIR place (local + projections).
pub fn emit_place_read<M: Module>(
    ctx: &mut FunctionContext<'_>,
    place: &Place,
    table: &TypeTable,
    module: &mut M,
) -> Value {
    if place.projections.is_empty() {
        // Simple local read.
        return ctx.read_local(place.local);
    }

    // Build the address through projections, then load.
    let addr = emit_place_addr(ctx, place, table, module);
    let result_ty = resolve_place_type(ctx, place, table);

    if let Some(clif_ty) = abi::spark_type_to_clif(result_ty, table, ctx.ptr_type) {
        if abi::is_scalar(result_ty, table) {
            ctx.load(clif_ty, addr, 0)
        } else {
            // Aggregate — return the address as the "value"
            addr
        }
    } else {
        // Zero-sized
        ctx.builder.ins().iconst(types::I8, 0)
    }
}

/// Computes the address of a MIR place (local + projections).
pub(super) fn emit_place_addr<M: Module>(
    ctx: &mut FunctionContext<'_>,
    place: &Place,
    table: &TypeTable,
    module: &mut M,
) -> Value {
    let mut addr = match ctx.local_storage(place.local) {
        LocalStorage::StackSlot(slot, _) => ctx.builder.ins().stack_addr(ctx.ptr_type, slot, 0),
        LocalStorage::Var(_, _) => {
            // For scalar locals stored in registers, we spill to a
            // temporary stack slot so we can take their address.
            let val = ctx.read_local(place.local);
            let ty_id = ctx.local_type_id(place.local);
            store_scalar_to_temp(ctx, val, ty_id, table)
        }
        LocalStorage::ZeroSized => ctx.builder.ins().iconst(ctx.ptr_type, 0),
    };

    let mut current_ty = ctx.local_type_id(place.local);

    for proj in &place.projections {
        match proj {
            Projection::Field(field_name) => {
                let (unwrapped_id, unwrapped_ty) = clif_types::unwrap_mutable(current_ty, table);
                match unwrapped_ty {
                    Ty::Struct(def_id) => {
                        let def_id = *def_id;
                        if let Some(offset) = ctx.layouts.field_offset(def_id, *field_name, table) {
                            addr = ctx.builder.ins().iadd_imm(addr, i64::from(offset));
                        }
                        // Update current type to the field's type.
                        if let Some(fields) = table.get_struct_fields(def_id) {
                            if let Some((_, field_ty)) = fields.iter().find(|(n, _)| *n == *field_name) {
                                current_ty = *field_ty;
                            }
                        }
                    }
                    Ty::Applied { base, args } => {
                        let base = *base;
                        let args = args.clone();
                        if let Some(offset) = ctx.layouts.field_offset_for_type(unwrapped_id, *field_name, table) {
                            addr = ctx.builder.ins().iadd_imm(addr, i64::from(offset));
                        }
                        // Update current type to the substituted field type.
                        if let Some(fields) = table.get_struct_fields(base) {
                            if let Some((_, field_ty)) = fields.iter().find(|(n, _)| *n == *field_name) {
                                let params = table
                                    .get_generic_params(base)
                                    .map(<[_]>::to_vec)
                                    .unwrap_or_default();
                                let subst = spark_types::substitute::build_subst_map(&params, &args);
                                current_ty = spark_types::substitute::substitute(*field_ty, &subst, table);
                            }
                        }
                    }
                    Ty::AsyncState { fields, .. } => {
                        let fields = fields.clone();
                        if let Some(offset) = ctx.layouts.field_offset_for_type(unwrapped_id, *field_name, table) {
                            addr = ctx.builder.ins().iadd_imm(addr, i64::from(offset));
                        }
                        if let Some((_, field_ty)) = fields.iter().find(|(n, _)| *n == *field_name) {
                            current_ty = *field_ty;
                        }
                    }
                    _ => {
                        // For enums or other types with field access, we'd handle
                        // similarly. For now, just offset by 0.
                        let _ = unwrapped_id;
                    }
                }
            }
            Projection::Index(idx_local) => {
                let idx_val = ctx.read_local(*idx_local);
                let (_, unwrapped_ty) = clif_types::unwrap_mutable(current_ty, table);
                match unwrapped_ty {
                    Ty::List(elem_ty) => {
                        let elem_ty = *elem_ty;
                        // Load the list pointer from the address (addr points to
                        // the local's storage, not the list itself).
                        let list_ptr = ctx.load(ctx.ptr_type, addr, 0);
                        let func_ref = module.declare_func_in_func(
                            ctx.stubs.list_get,
                            ctx.builder.func,
                        );
                        let call = ctx.builder.ins().call(func_ref, &[list_ptr, idx_val]);
                        addr = ctx.builder.inst_results(call)[0];
                        current_ty = elem_ty;
                    }
                    Ty::Map(key_ty, value_ty) => {
                        let key_ty = *key_ty;
                        let value_ty = *value_ty;
                        // Store the key to a temp slot, then call __spark_map_get.
                        let key_addr = store_scalar_to_temp(ctx, idx_val, key_ty, table);
                        let map_ptr = ctx.load(ctx.ptr_type, addr, 0);
                        let func_ref = module.declare_func_in_func(
                            ctx.stubs.map_get,
                            ctx.builder.func,
                        );
                        let call = ctx.builder.ins().call(func_ref, &[map_ptr, key_addr]);
                        addr = ctx.builder.inst_results(call)[0];
                        current_ty = value_ty;
                    }
                    _ => {
                        // Fallback: raw pointer arithmetic for array-like types.
                        let elem_size_val = ctx.builder.ins().iconst(ctx.ptr_type, 8);
                        let offset = ctx.builder.ins().imul(idx_val, elem_size_val);
                        addr = ctx.builder.ins().iadd(addr, offset);
                    }
                }
            }
            Projection::Deref => {
                // Load the pointer value.
                addr = ctx.builder.ins().load(ctx.ptr_type, MemFlags::new(), addr, 0);
                let (_, unwrapped_ty) = clif_types::unwrap_mutable(current_ty, table);
                if let Ty::Ref { inner, .. } = unwrapped_ty {
                    current_ty = *inner;
                }
            }
            Projection::UnionPayload(member_ty) => {
                let (_, unwrapped_ty) = clif_types::unwrap_mutable(current_ty, table);
                if matches!(unwrapped_ty, Ty::Optional(_)) {
                    // Optional[T]: 1-byte tag at offset 0, payload at align_to(1, inner_align).
                    // Since alignment is always a power of 2, align_to(1, a) == a.
                    let inner_layout = ctx.layouts.layout_of(*member_ty, table);
                    let payload_offset = inner_layout.align;
                    addr = ctx.builder.ins().iadd_imm(addr, i64::from(payload_offset));
                } else {
                    // Union: use the union layout's payload offset.
                    let ul = ctx.layouts.union_layout_of(current_ty, table);
                    addr = ctx.builder.ins().iadd_imm(addr, i64::from(ul.payload_offset));
                }
                current_ty = *member_ty;
            }
        }
    }

    addr
}

/// Resolves the type of a place after all projections.
pub(super) fn resolve_place_type(ctx: &FunctionContext<'_>, place: &Place, table: &TypeTable) -> TypeId {
    let mut current_ty = ctx.local_type_id(place.local);

    for proj in &place.projections {
        let (_, unwrapped) = clif_types::unwrap_mutable(current_ty, table);
        match proj {
            Projection::Field(field_name) => {
                match unwrapped {
                    Ty::Struct(def_id) => {
                        if let Some(fields) = table.get_struct_fields(*def_id) {
                            if let Some((_, field_ty)) = fields.iter().find(|(n, _)| *n == *field_name) {
                                current_ty = *field_ty;
                            }
                        }
                    }
                    Ty::Applied { base, args } => {
                        if let Some(fields) = table.get_struct_fields(*base) {
                            if let Some((_, field_ty)) = fields.iter().find(|(n, _)| *n == *field_name) {
                                let params = table
                                    .get_generic_params(*base)
                                    .map(<[_]>::to_vec)
                                    .unwrap_or_default();
                                let subst = spark_types::substitute::build_subst_map(&params, &args.clone());
                                current_ty = spark_types::substitute::substitute(*field_ty, &subst, table);
                            }
                        }
                    }
                    Ty::AsyncState { fields, .. } => {
                        if let Some((_, field_ty)) = fields.iter().find(|(n, _)| *n == *field_name) {
                            current_ty = *field_ty;
                        }
                    }
                    _ => {}
                }
            }
            Projection::Index(_) => {
                match unwrapped {
                    Ty::List(elem_ty) => current_ty = *elem_ty,
                    Ty::Map(_, value_ty) => current_ty = *value_ty,
                    _ => {}
                }
            }
            Projection::Deref => {
                if let Ty::Ref { inner, .. } = unwrapped {
                    current_ty = *inner;
                }
            }
            Projection::UnionPayload(member_ty) => {
                current_ty = *member_ty;
            }
        }
    }

    current_ty
}

/// Emits a binary operation.
fn emit_binop(
    ctx: &mut FunctionContext<'_>,
    op: BinaryOp,
    lhs: Value,
    rhs: Value,
    result_ty: TypeId,
    table: &TypeTable,
) -> Value {
    let (_, ty) = clif_types::unwrap_mutable(result_ty, table);

    // Check if the operands are float.
    let is_float = matches!(ty, Ty::Float(_));
    // For comparisons, the result is Bool but operands may be int/float.
    // We need to detect this from the operation kind.
    let is_comparison = op.is_comparison();

    if is_float && !is_comparison {
        emit_float_binop(ctx, op, lhs, rhs)
    } else if is_comparison {
        // For comparisons, check if result is Bool — the operand types determine int vs float.
        // We'll check the lhs type by inspecting the cranelift value type.
        let lhs_ty = ctx.builder.func.dfg.value_type(lhs);
        if lhs_ty == types::F32 || lhs_ty == types::F64 {
            emit_float_cmp(ctx, op, lhs, rhs)
        } else {
            // Check if we need signed comparison.
            emit_int_binop(ctx, op, lhs, rhs, true)
        }
    } else {
        // Default: check if the result is a signed integer for div/mod.
        let signed = matches!(ty, Ty::Int(IntWidth::I8 | IntWidth::I16 | IntWidth::I32 | IntWidth::I64));
        emit_int_binop(ctx, op, lhs, rhs, signed)
    }
}

/// Extracts the immediate value from a Cranelift `iconst` instruction.
///
/// Returns `Some(imm)` if the value was produced by an `iconst`, `None` otherwise.
fn try_get_iconst(ctx: &FunctionContext<'_>, val: Value) -> Option<i64> {
    let dfg = &ctx.builder.func.dfg;
    let inst = match dfg.value_def(val) {
        cranelift_codegen::ir::ValueDef::Result(inst, _) => inst,
        _ => return None,
    };
    match dfg.insts[inst] {
        InstructionData::UnaryImm { imm, .. } => Some(imm.bits()),
        _ => None,
    }
}

/// Attempts to strength-reduce signed division or modulo by a constant
/// power-of-2 into bitwise operations.
///
/// For signed `x / (2^k)`, emits the standard compiler trick:
/// ```text
/// sign     = sshr x, (bits - 1)      // -1 if negative, 0 if positive
/// bias     = ushr sign, (bits - k)   // (2^k - 1) if negative, 0 if positive
/// adjusted = iadd x, bias
/// result   = sshr adjusted, k
/// ```
///
/// For signed `x % (2^k)`, computes `x - (x / 2^k) * 2^k`.
///
/// Returns `Some(value)` if the optimization applied, `None` to fall through.
fn try_strength_reduce_signed_divmod(
    ctx: &mut FunctionContext<'_>,
    op: BinaryOp,
    lhs: Value,
    rhs: Value,
) -> Option<Value> {
    let divisor = try_get_iconst(ctx, rhs)?;
    if divisor <= 0 || !(divisor as u64).is_power_of_two() {
        return None;
    }

    let k = (divisor as u64).trailing_zeros();
    let bits = i64::from(ctx.builder.func.dfg.value_type(lhs).bits());

    // Compute signed division: (x + bias) >> k where bias = (2^k - 1) if x < 0
    let sign = ctx.builder.ins().sshr_imm(lhs, bits - 1);
    let bias = ctx.builder.ins().ushr_imm(sign, bits - i64::from(k));
    let adjusted = ctx.builder.ins().iadd(lhs, bias);
    let div_result = ctx.builder.ins().sshr_imm(adjusted, i64::from(k));

    match op {
        BinaryOp::Div => Some(div_result),
        BinaryOp::Mod => {
            // x % (2^k) = x - (x / 2^k) * 2^k
            let mul_back = ctx.builder.ins().ishl_imm(div_result, i64::from(k));
            Some(ctx.builder.ins().isub(lhs, mul_back))
        }
        _ => None,
    }
}

/// Emits an integer binary operation.
fn emit_int_binop(
    ctx: &mut FunctionContext<'_>,
    op: BinaryOp,
    lhs: Value,
    rhs: Value,
    signed: bool,
) -> Value {
    match op {
        BinaryOp::Add => ctx.builder.ins().iadd(lhs, rhs),
        BinaryOp::Sub => ctx.builder.ins().isub(lhs, rhs),
        BinaryOp::Mul => ctx.builder.ins().imul(lhs, rhs),
        BinaryOp::Div => {
            if signed {
                if let Some(result) = try_strength_reduce_signed_divmod(ctx, op, lhs, rhs) {
                    return result;
                }
                ctx.builder.ins().sdiv(lhs, rhs)
            } else {
                ctx.builder.ins().udiv(lhs, rhs)
            }
        }
        BinaryOp::Mod => {
            if signed {
                if let Some(result) = try_strength_reduce_signed_divmod(ctx, op, lhs, rhs) {
                    return result;
                }
                ctx.builder.ins().srem(lhs, rhs)
            } else {
                ctx.builder.ins().urem(lhs, rhs)
            }
        }
        BinaryOp::Eq => ctx.builder.ins().icmp(IntCC::Equal, lhs, rhs),
        BinaryOp::Ne => ctx.builder.ins().icmp(IntCC::NotEqual, lhs, rhs),
        BinaryOp::Lt => {
            let cc = if signed { IntCC::SignedLessThan } else { IntCC::UnsignedLessThan };
            ctx.builder.ins().icmp(cc, lhs, rhs)
        }
        BinaryOp::Le => {
            let cc = if signed { IntCC::SignedLessThanOrEqual } else { IntCC::UnsignedLessThanOrEqual };
            ctx.builder.ins().icmp(cc, lhs, rhs)
        }
        BinaryOp::Gt => {
            let cc = if signed { IntCC::SignedGreaterThan } else { IntCC::UnsignedGreaterThan };
            ctx.builder.ins().icmp(cc, lhs, rhs)
        }
        BinaryOp::Ge => {
            let cc = if signed { IntCC::SignedGreaterThanOrEqual } else { IntCC::UnsignedGreaterThanOrEqual };
            ctx.builder.ins().icmp(cc, lhs, rhs)
        }
        BinaryOp::And | BinaryOp::BitAnd => ctx.builder.ins().band(lhs, rhs),
        BinaryOp::Or | BinaryOp::BitOr => ctx.builder.ins().bor(lhs, rhs),
        BinaryOp::BitXor => ctx.builder.ins().bxor(lhs, rhs),
        BinaryOp::Shl => ctx.builder.ins().ishl(lhs, rhs),
        BinaryOp::Shr => {
            if signed {
                ctx.builder.ins().sshr(lhs, rhs)
            } else {
                ctx.builder.ins().ushr(lhs, rhs)
            }
        }
    }
}

/// Emits a floating-point binary operation.
fn emit_float_binop(ctx: &mut FunctionContext<'_>, op: BinaryOp, lhs: Value, rhs: Value) -> Value {
    match op {
        BinaryOp::Add => ctx.builder.ins().fadd(lhs, rhs),
        BinaryOp::Sub => ctx.builder.ins().fsub(lhs, rhs),
        BinaryOp::Mul => ctx.builder.ins().fmul(lhs, rhs),
        BinaryOp::Div => ctx.builder.ins().fdiv(lhs, rhs),
        _ => {
            // Float doesn't support mod, bitwise, shifts, logical.
            // Return lhs as a fallback (should not happen with valid MIR).
            lhs
        }
    }
}

/// Emits a floating-point comparison.
fn emit_float_cmp(ctx: &mut FunctionContext<'_>, op: BinaryOp, lhs: Value, rhs: Value) -> Value {
    let cc = match op {
        BinaryOp::Ne => FloatCC::NotEqual,
        BinaryOp::Lt => FloatCC::LessThan,
        BinaryOp::Le => FloatCC::LessThanOrEqual,
        BinaryOp::Gt => FloatCC::GreaterThan,
        BinaryOp::Ge => FloatCC::GreaterThanOrEqual,
        _ => FloatCC::Equal,
    };
    ctx.builder.ins().fcmp(cc, lhs, rhs)
}

/// Emits a unary operation.
fn emit_unaryop(
    ctx: &mut FunctionContext<'_>,
    op: UnaryOp,
    val: Value,
    result_ty: TypeId,
    table: &TypeTable,
) -> Value {
    let (_, ty) = clif_types::unwrap_mutable(result_ty, table);
    let is_float = matches!(ty, Ty::Float(_));

    match op {
        UnaryOp::Neg => {
            if is_float {
                ctx.builder.ins().fneg(val)
            } else {
                ctx.builder.ins().ineg(val)
            }
        }
        UnaryOp::Not => {
            // Boolean not: XOR with 1
            ctx.builder.ins().bxor_imm(val, 1)
        }
        UnaryOp::BitNot => ctx.builder.ins().bnot(val),
    }
}

/// Emits an aggregate construction.
#[allow(clippy::too_many_arguments)]
fn emit_aggregate<M: Module>(
    ctx: &mut FunctionContext<'_>,
    kind: &AggregateKind,
    operands: &[Operand],
    dest_local: Local,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    dtor_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    elem_dtor_ids: &FxHashMap<TypeId, FuncId>,
    module: &mut M,
    diagnostics: &mut Diagnostics,
) -> Option<Value> {
    match kind {
        AggregateKind::Struct { def_id, fields } => {
            let dest_addr = ctx.local_stack_addr(dest_local)?;

            // Check if the dest local has an Applied type (generic struct instantiation).
            // If so, compute layout with substituted field types.
            let dest_ty = ctx.local_type_id(dest_local);
            let (_, unwrapped) = clif_types::unwrap_mutable(dest_ty, table);
            let sl = if matches!(unwrapped, Ty::Applied { .. }) {
                let (unwrapped_id, _) = clif_types::unwrap_mutable(dest_ty, table);
                ctx.layouts.applied_struct_layout_of(unwrapped_id, table)
            } else {
                ctx.layouts.struct_layout_of(*def_id, table)
            };

            for (i, operand) in operands.iter().enumerate() {
                if let Some(val) = emit_operand(ctx, operand, table, definitions, func_ids, thunk_ids, module) {
                    let offset = sl.field_offsets[i];
                    ctx.store(val, dest_addr, i32::try_from(offset).unwrap_or(0));
                }
            }

            let _ = fields;
            Some(dest_addr)
        }

        AggregateKind::EnumVariant { def_id, variant } => {
            let dest_addr = ctx.local_stack_addr(dest_local)?;
            let el = ctx.layouts.enum_layout_of(*def_id, table);

            // Find variant index.
            let variant_idx = table
                .get_enum_variants(*def_id)
                .and_then(|variants| variants.iter().position(|(name, _)| *name == *variant))
                .expect("ICE: enum variant not found in variant list; this is a compiler bug");

            // Store the tag.
            if el.tag_size > 0 {
                let tag_ty = clif_types::int_type_for_bytes(el.tag_size);
                let tag_val = ctx.builder.ins().iconst(tag_ty, variant_idx as i64);
                ctx.store(tag_val, dest_addr, 0);
            }

            // Store variant fields at payload offset.
            // Look up variant fields from the variant's DefId.
            if let Some(variants) = table.get_enum_variants(*def_id) {
                if let Some((_, variant_def_id)) = variants.iter().find(|(n, _)| *n == *variant) {
                    if let Some(variant_fields) = table.get_variant_fields(*variant_def_id) {
                        let mut field_offset = el.payload_offset;
                        for (i, operand) in operands.iter().enumerate() {
                            if i < variant_fields.len() {
                                let (_, field_ty) = variant_fields[i];
                                let field_layout = ctx.layouts.layout_of(field_ty, table);
                                field_offset = align_to(field_offset, field_layout.align);

                                if let Some(val) = emit_operand(ctx, operand, table, definitions, func_ids, thunk_ids, module) {
                                    ctx.store(val, dest_addr, i32::try_from(field_offset).unwrap_or(0));
                                }

                                field_offset += field_layout.size;
                            }
                        }
                    }
                }
            }

            Some(dest_addr)
        }

        AggregateKind::Tuple => {
            let dest_addr = ctx.local_stack_addr(dest_local)?;
            let dest_ty = ctx.local_type_id(dest_local);
            let (_, unwrapped) = clif_types::unwrap_mutable(dest_ty, table);

            if let Ty::Tuple(elem_types) = unwrapped {
                let elem_types = elem_types.clone();
                let mut offset = 0u32;
                let mut max_align = 1u32;

                for (i, operand) in operands.iter().enumerate() {
                    if i < elem_types.len() {
                        let elem_layout = ctx.layouts.layout_of(elem_types[i], table);
                        offset = align_to(offset, elem_layout.align);

                        if let Some(val) = emit_operand(ctx, operand, table, definitions, func_ids, thunk_ids, module) {
                            ctx.store(val, dest_addr, i32::try_from(offset).unwrap_or(0));
                        }

                        offset += elem_layout.size;
                        max_align = max_align.max(elem_layout.align);
                    }
                }
            }

            Some(dest_addr)
        }

        AggregateKind::List => {
            emit_list_construction(ctx, operands, dest_local, table, definitions, func_ids, thunk_ids, elem_dtor_ids, module)
        }

        AggregateKind::Map => {
            emit_map_construction(ctx, operands, dest_local, table, definitions, func_ids, thunk_ids, elem_dtor_ids, module)
        }

        AggregateKind::Set => {
            emit_set_construction(ctx, operands, dest_local, table, definitions, func_ids, thunk_ids, elem_dtor_ids, module, diagnostics)
        }

        AggregateKind::Closure { func_def_id } => {
            emit_closure_construction(
                ctx, *func_def_id, operands, dest_local, table, definitions,
                func_ids, dtor_ids, module,
            )
        }

        AggregateKind::Union { union_ty, member_ty } => {
            emit_union_construction(ctx, *union_ty, *member_ty, operands, dest_local, table, definitions, func_ids, thunk_ids, module)
        }

        AggregateKind::Channel => {
            // Channel[T]() → call __spark_channel_new(elem_size)
            let dest_ty = ctx.local_type_id(dest_local);
            let (_, unwrapped) = clif_types::unwrap_mutable(dest_ty, table);
            let elem_size = if let Ty::Channel { inner } = unwrapped {
                let layout = ctx.layouts.layout_of(*inner, table);
                i64::from(layout.size)
            } else {
                8 // fallback
            };
            let elem_size_val = ctx.builder.ins().iconst(types::I64, elem_size);
            let func_ref = module.declare_func_in_func(ctx.stubs.channel_new, ctx.builder.func);
            let call = ctx.builder.ins().call(func_ref, &[elem_size_val]);
            let result = ctx.builder.inst_results(call)[0];
            ctx.write_local(dest_local, result);
            Some(result)
        }
    }
}

/// Emits list construction: `[e1, e2, ...]`.
///
/// Calls `__spark_list_new(elem_size)`, then `__spark_list_push` for each element.
#[allow(clippy::too_many_arguments, clippy::unnecessary_wraps)]
fn emit_list_construction<M: Module>(
    ctx: &mut FunctionContext<'_>,
    operands: &[Operand],
    dest_local: Local,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    elem_dtor_ids: &FxHashMap<TypeId, FuncId>,
    module: &mut M,
) -> Option<Value> {
    let dest_ty = ctx.local_type_id(dest_local);
    let (_, unwrapped) = clif_types::unwrap_mutable(dest_ty, table);

    let elem_ty = match unwrapped {
        Ty::List(elem_ty) => *elem_ty,
        _ => return Some(ctx.builder.ins().iconst(ctx.ptr_type, 0)),
    };

    let elem_layout = ctx.layouts.layout_of(elem_ty, table);
    let elem_size_val = ctx.builder.ins().iconst(types::I64, i64::from(elem_layout.size));

    // Get the element destructor function address (or null).
    let elem_drop_val = elem_dtor_func_addr(ctx, elem_ty, elem_dtor_ids, module);

    // Call __spark_list_new(elem_size, elem_drop).
    let new_ref = module.declare_func_in_func(ctx.stubs.list_new, ctx.builder.func);
    let call = ctx.builder.ins().call(new_ref, &[elem_size_val, elem_drop_val]);
    let list_ptr = ctx.builder.inst_results(call)[0];

    // Push each element.
    let push_ref = module.declare_func_in_func(ctx.stubs.list_push, ctx.builder.func);
    for operand in operands {
        if let Some(elem_addr) = emit_operand_addr(ctx, operand, elem_ty, table, definitions, func_ids, thunk_ids, module) {
            ctx.builder.ins().call(push_ref, &[list_ptr, elem_addr]);
        }
    }

    Some(list_ptr)
}

/// Emits map construction: `{"k1": v1, "k2": v2, ...}`.
///
/// Calls `__spark_map_new(key_size, value_size)`, then `__spark_map_insert`
/// for each key-value pair. Operands alternate: key, value, key, value, ...
#[allow(clippy::too_many_arguments, clippy::unnecessary_wraps)]
fn emit_map_construction<M: Module>(
    ctx: &mut FunctionContext<'_>,
    operands: &[Operand],
    dest_local: Local,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    elem_dtor_ids: &FxHashMap<TypeId, FuncId>,
    module: &mut M,
) -> Option<Value> {
    let dest_ty = ctx.local_type_id(dest_local);
    let (_, unwrapped) = clif_types::unwrap_mutable(dest_ty, table);

    let (key_ty, value_ty) = match unwrapped {
        Ty::Map(k, v) => (*k, *v),
        _ => return Some(ctx.builder.ins().iconst(ctx.ptr_type, 0)),
    };

    let key_layout = ctx.layouts.layout_of(key_ty, table);
    let val_layout = ctx.layouts.layout_of(value_ty, table);
    let key_size_val = ctx.builder.ins().iconst(types::I64, i64::from(key_layout.size));
    let val_size_val = ctx.builder.ins().iconst(types::I64, i64::from(val_layout.size));

    // Get key/value destructor function addresses (or null).
    let key_drop_val = elem_dtor_func_addr(ctx, key_ty, elem_dtor_ids, module);
    let val_drop_val = elem_dtor_func_addr(ctx, value_ty, elem_dtor_ids, module);

    // Call __spark_map_new(key_size, value_size, key_drop, val_drop).
    let new_ref = module.declare_func_in_func(ctx.stubs.map_new, ctx.builder.func);
    let call = ctx.builder.ins().call(new_ref, &[key_size_val, val_size_val, key_drop_val, val_drop_val]);
    let map_ptr = ctx.builder.inst_results(call)[0];

    // Insert each key-value pair. Operands come in pairs: [k0, v0, k1, v1, ...].
    let insert_ref = module.declare_func_in_func(ctx.stubs.map_insert, ctx.builder.func);
    let mut i = 0;
    while i + 1 < operands.len() {
        let key_addr = emit_operand_addr(ctx, &operands[i], key_ty, table, definitions, func_ids, thunk_ids, module);
        let val_addr = emit_operand_addr(ctx, &operands[i + 1], value_ty, table, definitions, func_ids, thunk_ids, module);
        if let (Some(k), Some(v)) = (key_addr, val_addr) {
            ctx.builder.ins().call(insert_ref, &[map_ptr, k, v]);
        }
        i += 2;
    }

    Some(map_ptr)
}

/// Emits set construction.
///
/// Calls `__spark_set_new(elem_size)`, then `__spark_set_insert` for each element.
#[allow(clippy::too_many_arguments, clippy::unnecessary_wraps)]
fn emit_set_construction<M: Module>(
    ctx: &mut FunctionContext<'_>,
    operands: &[Operand],
    dest_local: Local,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    elem_dtor_ids: &FxHashMap<TypeId, FuncId>,
    module: &mut M,
    diagnostics: &mut Diagnostics,
) -> Option<Value> {
    let dest_ty = ctx.local_type_id(dest_local);
    let (_, unwrapped) = clif_types::unwrap_mutable(dest_ty, table);

    let elem_ty = if let Ty::Set(elem_ty) = unwrapped {
        *elem_ty
    } else {
        diagnostics.warning("set construction: expected Set type on destination");
        return Some(ctx.builder.ins().iconst(ctx.ptr_type, 0));
    };

    let elem_layout = ctx.layouts.layout_of(elem_ty, table);
    let elem_size_val = ctx.builder.ins().iconst(types::I64, i64::from(elem_layout.size));

    // Get the element destructor function address (or null).
    let elem_drop_val = elem_dtor_func_addr(ctx, elem_ty, elem_dtor_ids, module);

    // Call __spark_set_new(elem_size, elem_drop).
    let new_ref = module.declare_func_in_func(ctx.stubs.set_new, ctx.builder.func);
    let call = ctx.builder.ins().call(new_ref, &[elem_size_val, elem_drop_val]);
    let set_ptr = ctx.builder.inst_results(call)[0];

    // Insert each element.
    let insert_ref = module.declare_func_in_func(ctx.stubs.set_insert, ctx.builder.func);
    for operand in operands {
        if let Some(elem_addr) = emit_operand_addr(ctx, operand, elem_ty, table, definitions, func_ids, thunk_ids, module) {
            ctx.builder.ins().call(insert_ref, &[set_ptr, elem_addr]);
        }
    }

    Some(set_ptr)
}

/// Emits closure construction: allocates env, stores captures, builds fat pointer.
///
/// 1. Computes the env size from capture operand types
/// Returns the Cranelift `Value` for an element destructor function address.
///
/// If the element type has a pre-generated dtor in `elem_dtor_ids`, returns
/// a `func_addr` instruction referencing it. Otherwise returns `iconst(0)`.
fn elem_dtor_func_addr<M: Module>(
    ctx: &mut FunctionContext<'_>,
    elem_ty: TypeId,
    elem_dtor_ids: &FxHashMap<TypeId, FuncId>,
    module: &mut M,
) -> Value {
    if let Some(&dtor_fid) = elem_dtor_ids.get(&elem_ty) {
        let func_ref = module.declare_func_in_func(dtor_fid, ctx.builder.func);
        ctx.builder.ins().func_addr(ctx.ptr_type, func_ref)
    } else {
        ctx.builder.ins().iconst(ctx.ptr_type, 0)
    }
}

/// 2. Calls `__spark_env_alloc(size)` to get the env pointer
/// 3. Stores each captured value at the correct offset in the env
/// 4. Writes `(fn_ptr, env_ptr)` into the destination stack slot (fat pointer)
#[allow(clippy::too_many_arguments)]
fn emit_closure_construction<M: Module>(
    ctx: &mut FunctionContext<'_>,
    func_def_id: spark_resolve::DefId,
    operands: &[Operand],
    dest_local: Local,
    table: &TypeTable,
    _definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    dtor_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
) -> Option<Value> {
    let dest_addr = ctx.local_stack_addr(dest_local)?;

    // Compute env layout: each capture at its natural offset.
    let mut env_size: u32 = 0;
    let mut capture_offsets = Vec::with_capacity(operands.len());
    for operand in operands {
        let cap_ty = match operand {
            Operand::Copy(place) | Operand::Move(place) => ctx.local_type_id(place.local),
            Operand::Constant(_) => {
                capture_offsets.push(env_size);
                env_size += 8; // fallback
                continue;
            }
        };
        let layout = ctx.layouts.layout_of(cap_ty, table);
        env_size = align_to(env_size, layout.align);
        capture_offsets.push(env_size);
        env_size += layout.size;
    }

    // Allocate the environment.
    let env_size_val = ctx.builder.ins().iconst(types::I64, i64::from(env_size));
    let env_alloc_ref = module.declare_func_in_func(ctx.stubs.env_alloc, ctx.builder.func);
    let alloc_call = ctx.builder.ins().call(env_alloc_ref, &[env_size_val]);
    let env_ptr = ctx.builder.inst_results(alloc_call)[0];

    // Store destructor pointer at offset 16 in the env header (if one was generated).
    if let Some(&dtor_func_id) = dtor_ids.get(&func_def_id) {
        let dtor_ref = module.declare_func_in_func(dtor_func_id, ctx.builder.func);
        let dtor_addr = ctx.builder.ins().func_addr(ctx.ptr_type, dtor_ref);
        ctx.builder.ins().store(MemFlags::new(), dtor_addr, env_ptr, 16);
    }

    // Store each captured value into the environment.
    for (i, operand) in operands.iter().enumerate() {
        let cap_ty = match operand {
            Operand::Copy(place) | Operand::Move(place) => ctx.local_type_id(place.local),
            Operand::Constant(_) => continue,
        };
        let val = match operand {
            Operand::Copy(place) | Operand::Move(place) => {
                emit_place_read(ctx, place, table, module)
            }
            Operand::Constant(_) => continue,
        };
        // RC inc for Copy captures: the closure env keeps its own reference.
        if matches!(operand, Operand::Copy(_)) {
            if spark_drop::needs_rc(cap_ty, table) {
                let func_ref = module.declare_func_in_func(ctx.stubs.rc_inc, ctx.builder.func);
                ctx.builder.ins().call(func_ref, &[val]);
            } else if matches!(table.resolve(cap_ty), Ty::Function { .. }) {
                // Function capture: RC inc the env_ptr (second slot of the fat pointer).
                let inner_env = ctx.builder.ins().load(
                    ctx.ptr_type,
                    MemFlags::new(),
                    val,
                    ctx.ptr_type.bytes() as i32,
                );
                let func_ref = module.declare_func_in_func(ctx.stubs.rc_inc, ctx.builder.func);
                ctx.builder.ins().call(func_ref, &[inner_env]);
            }
        }
        // Add 24 bytes for the header (rc + alloc_size + dtor) prepended by __spark_env_alloc.
        let offset = capture_offsets[i] + 24;
        if abi::is_scalar(cap_ty, table) {
            ctx.builder.ins().store(
                MemFlags::new(),
                val,
                env_ptr,
                i32::try_from(offset).unwrap_or(0),
            );
        } else {
            // Aggregate capture — memcpy from val (address) to env_ptr + offset.
            let dest = ctx.builder.ins().iadd_imm(env_ptr, i64::from(offset));
            let layout = ctx.layouts.layout_of(cap_ty, table);
            let config = module.target_config();
            ctx.builder.emit_small_memory_copy(
                config,
                dest,
                val,
                u64::from(layout.size),
                1, 1, true,
                MemFlags::new(),
            );
        }
    }

    // Get the function pointer for the lifted lambda.
    let fn_ptr = if let Some(&fid) = func_ids.get(&func_def_id) {
        let func_ref = module.declare_func_in_func(fid, ctx.builder.func);
        ctx.builder.ins().func_addr(ctx.ptr_type, func_ref)
    } else {
        ctx.builder.ins().iconst(ctx.ptr_type, 0)
    };

    // Store (fn_ptr, env_ptr) into the destination fat pointer slot.
    ctx.builder.ins().store(MemFlags::new(), fn_ptr, dest_addr, 0);
    ctx.builder.ins().store(
        MemFlags::new(),
        env_ptr,
        dest_addr,
        ctx.ptr_type.bytes() as i32,
    );

    Some(dest_addr)
}

/// Emits union construction: writes tag + payload into a stack slot.
///
/// The tag byte is the index of `member_ty` in the union's member list.
/// The payload is written at the union's payload offset.
#[allow(clippy::too_many_arguments)]
fn emit_union_construction<M: Module>(
    ctx: &mut FunctionContext<'_>,
    union_ty: TypeId,
    member_ty: TypeId,
    operands: &[Operand],
    dest_local: Local,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
) -> Option<Value> {
    let dest_addr = ctx.local_stack_addr(dest_local)?;

    let ul = ctx.layouts.union_layout_of(union_ty, table);

    // Find the member index (tag value).
    let member_idx = match table.resolve(union_ty) {
        Ty::Union(members) => {
            members.iter().position(|m| *m == member_ty)
                .expect("ICE: union member type not found in union members; this is a compiler bug")
        }
        Ty::Optional(inner) => {
            // Optional: tag 0 = Null, tag 1 = Some(inner).
            let member_resolved = table.resolve(member_ty);
            if matches!(member_resolved, Ty::Null) {
                0
            } else if member_ty == *inner {
                1
            } else {
                1 // default to Some for non-null values
            }
        }
        other => panic!("ICE: expected Union or Optional type for union tag, found {other:?}; this is a compiler bug"),
    };

    // Write the tag.
    if ul.tag_size > 0 {
        let tag_ty = clif_types::int_type_for_bytes(ul.tag_size);
        let tag_val = ctx.builder.ins().iconst(tag_ty, member_idx as i64);
        ctx.store(tag_val, dest_addr, 0);
    }

    // Write the payload at payload_offset.
    if let Some(operand) = operands.first() {
        if let Some(val) = emit_operand(ctx, operand, table, definitions, func_ids, thunk_ids, module) {
            let payload_addr = ctx.builder.ins().iadd_imm(dest_addr, i64::from(ul.payload_offset));
            if abi::is_scalar(member_ty, table) {
                ctx.store(val, payload_addr, 0);
            } else {
                // Aggregate payload — memcpy from val (address) to payload_addr.
                let member_layout = ctx.layouts.layout_of(member_ty, table);
                let config = module.target_config();
                ctx.builder.emit_small_memory_copy(
                    config,
                    payload_addr,
                    val,
                    u64::from(member_layout.size),
                    1, 1, true,
                    MemFlags::new(),
                );
            }
        }
    }

    Some(dest_addr)
}

/// Emits a discriminant read from an enum or union place.
fn emit_discriminant<M: Module>(
    ctx: &mut FunctionContext<'_>,
    place: &Place,
    table: &TypeTable,
    module: &mut M,
) -> Value {
    let addr = emit_place_addr(ctx, place, table, module);
    let place_ty = if place.projections.is_empty() {
        ctx.local_type_id(place.local)
    } else {
        resolve_place_type(ctx, place, table)
    };
    let (_, unwrapped) = clif_types::unwrap_mutable(place_ty, table);

    if let Ty::Enum(def_id) = unwrapped {
        let el = ctx.layouts.enum_layout_of(*def_id, table);
        if el.tag_size > 0 {
            let tag_ty = clif_types::int_type_for_bytes(el.tag_size);
            return ctx.load(tag_ty, addr, 0);
        }
    }

    if let Ty::Union(members) = unwrapped {
        let members = members.clone();
        let ul = ctx.layouts.union_layout_of(place_ty, table);
        if ul.tag_size > 0 {
            let tag_ty = clif_types::int_type_for_bytes(ul.tag_size);
            return ctx.load(tag_ty, addr, 0);
        }
        let _ = members;
    }

    // Optional[T] — tag byte at offset 0 (0 = null, 1 = Some).
    if matches!(unwrapped, Ty::Optional(_)) {
        return ctx.load(types::I8, addr, 0);
    }

    // Single-variant or non-enum/non-union — discriminant is 0.
    ctx.builder.ins().iconst(types::I8, 0)
}

/// Emits a type cast.
fn emit_cast(
    ctx: &mut FunctionContext<'_>,
    val: Value,
    src_ty: TypeId,
    target_ty: TypeId,
    table: &TypeTable,
) -> Value {
    let (_, src) = clif_types::unwrap_mutable(src_ty, table);
    let (_, tgt) = clif_types::unwrap_mutable(target_ty, table);

    match (src, tgt) {
        // Int → Int casts
        (Ty::Int(src_w), Ty::Int(tgt_w)) => {
            let src_size = clif_types::int_width_bytes(*src_w);
            let tgt_size = clif_types::int_width_bytes(*tgt_w);
            let tgt_clif = clif_types::int_width_to_clif(*tgt_w);

            match tgt_size.cmp(&src_size) {
                std::cmp::Ordering::Less => ctx.builder.ins().ireduce(tgt_clif, val),
                std::cmp::Ordering::Greater => {
                    if abi::int_is_signed(*src_w) {
                        ctx.builder.ins().sextend(tgt_clif, val)
                    } else {
                        ctx.builder.ins().uextend(tgt_clif, val)
                    }
                }
                std::cmp::Ordering::Equal => val,
            }
        }

        // Int → Float
        (Ty::Int(w), Ty::Float(fw)) => {
            let float_ty = match fw {
                FloatWidth::F32 => types::F32,
                FloatWidth::F64 => types::F64,
            };
            if abi::int_is_signed(*w) {
                ctx.builder.ins().fcvt_from_sint(float_ty, val)
            } else {
                ctx.builder.ins().fcvt_from_uint(float_ty, val)
            }
        }

        // Float → Int
        (Ty::Float(_), Ty::Int(w)) => {
            let int_ty = clif_types::int_width_to_clif(*w);
            if abi::int_is_signed(*w) {
                ctx.builder.ins().fcvt_to_sint_sat(int_ty, val)
            } else {
                ctx.builder.ins().fcvt_to_uint_sat(int_ty, val)
            }
        }

        // Float → Float
        (Ty::Float(FloatWidth::F32), Ty::Float(FloatWidth::F64)) => {
            ctx.builder.ins().fpromote(types::F64, val)
        }
        (Ty::Float(FloatWidth::F64), Ty::Float(FloatWidth::F32)) => {
            ctx.builder.ins().fdemote(types::F32, val)
        }

        // Bool → Int
        (Ty::Bool, Ty::Int(w)) => {
            let int_ty = clif_types::int_width_to_clif(*w);
            ctx.builder.ins().uextend(int_ty, val)
        }

        // Default: no-op cast
        _ => val,
    }
}

/// Determines the type of an operand.
fn operand_type(operand: &Operand, ctx: &FunctionContext<'_>) -> TypeId {
    match operand {
        Operand::Copy(place) | Operand::Move(place) => ctx.local_type_id(place.local),
        Operand::Constant(_) => {
            // For constants, we use a best-effort approach.
            // The actual type comes from the destination.
            ctx.local_type_id(Local(0))
        }
    }
}

/// Stores a scalar value to a temporary stack slot and returns the slot's address.
///
/// This is needed when calling runtime functions that expect pointers to values
/// (e.g., `__spark_list_push(list, elem_ptr)`), but the value is in a register.
pub fn store_scalar_to_temp(
    ctx: &mut FunctionContext<'_>,
    val: Value,
    ty_id: TypeId,
    table: &TypeTable,
) -> Value {
    use cranelift_codegen::ir::{StackSlotData, StackSlotKind};

    let layout = ctx.layouts.layout_of(ty_id, table);
    let size = layout.size.max(1); // At least 1 byte
    let slot = ctx.builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        size,
        u8::try_from(layout.align.trailing_zeros()).unwrap_or(0),
    ));
    let addr = ctx.builder.ins().stack_addr(ctx.ptr_type, slot, 0);
    ctx.store(val, addr, 0);
    addr
}

/// Emits a value from an operand and returns its address (pointer).
///
/// For scalars, stores the value into a temp stack slot and returns the address.
/// For aggregates, the value *is* already an address.
#[allow(clippy::too_many_arguments)]
pub fn emit_operand_addr<M: Module>(
    ctx: &mut FunctionContext<'_>,
    operand: &Operand,
    operand_ty: TypeId,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
) -> Option<Value> {
    let val = emit_operand(ctx, operand, table, definitions, func_ids, thunk_ids, module)?;
    if abi::is_scalar(operand_ty, table) {
        Some(store_scalar_to_temp(ctx, val, operand_ty, table))
    } else {
        // Aggregate — val is already a pointer to the data.
        Some(val)
    }
}

/// Emits a string interpolation: converts each part to a string pointer,
/// then chains `__spark_string_concat` calls left-to-right.
#[allow(clippy::too_many_arguments)]
fn emit_string_interp<M: Module>(
    ctx: &mut FunctionContext<'_>,
    operands: &[Operand],
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    thunk_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
) -> Option<Value> {
    if operands.is_empty() {
        // Empty interpolation → empty string.
        let func_ref = module.declare_func_in_func(ctx.stubs.string_new, ctx.builder.func);
        let call = ctx.builder.ins().call(func_ref, &[]);
        return Some(ctx.builder.inst_results(call)[0]);
    }

    // Convert each part to a string pointer.
    let mut string_vals: Vec<Value> = Vec::with_capacity(operands.len());
    for operand in operands {
        let val = emit_operand(ctx, operand, table, definitions, func_ids, thunk_ids, module);
        let part_ty = operand_type(operand, ctx);
        let (_, resolved) = clif_types::unwrap_mutable(part_ty, table);

        let string_ptr = match resolved {
            Ty::String => {
                // Already a string pointer — use directly.
                val?
            }
            Ty::Int(_) => {
                let v = val?;
                let func_ref = module.declare_func_in_func(ctx.stubs.int_to_string, ctx.builder.func);
                let call = ctx.builder.ins().call(func_ref, &[v]);
                ctx.builder.inst_results(call)[0]
            }
            Ty::Float(_) => {
                let v = val?;
                let func_ref = module.declare_func_in_func(ctx.stubs.float_to_string, ctx.builder.func);
                let call = ctx.builder.ins().call(func_ref, &[v]);
                ctx.builder.inst_results(call)[0]
            }
            Ty::Bool => {
                let v = val?;
                let func_ref = module.declare_func_in_func(ctx.stubs.bool_to_string, ctx.builder.func);
                let call = ctx.builder.ins().call(func_ref, &[v]);
                ctx.builder.inst_results(call)[0]
            }
            Ty::Char => {
                // Char → treat as Int for now (char code to string).
                let v = val?;
                // Extend i32 char to i64 for int_to_string.
                let wide = ctx.builder.ins().sextend(types::I64, v);
                let func_ref = module.declare_func_in_func(ctx.stubs.int_to_string, ctx.builder.func);
                let call = ctx.builder.ins().call(func_ref, &[wide]);
                ctx.builder.inst_results(call)[0]
            }
            _ => {
                // Unsupported type — emit empty string as fallback.
                let func_ref = module.declare_func_in_func(ctx.stubs.string_new, ctx.builder.func);
                let call = ctx.builder.ins().call(func_ref, &[]);
                ctx.builder.inst_results(call)[0]
            }
        };
        string_vals.push(string_ptr);
    }

    // Chain concat calls: result = concat(parts[0], parts[1]), then
    // result = concat(result, parts[2]), etc.
    let concat_ref = module.declare_func_in_func(ctx.stubs.string_concat, ctx.builder.func);
    let mut result = string_vals[0];
    for part in &string_vals[1..] {
        let call = ctx.builder.ins().call(concat_ref, &[result, *part]);
        result = ctx.builder.inst_results(call)[0];
    }

    Some(result)
}

/// Rounds `offset` up to the next multiple of `align`.
const fn align_to(offset: u32, align: u32) -> u32 {
    let mask = align - 1;
    (offset + mask) & !mask
}
