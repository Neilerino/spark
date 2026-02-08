//! Rvalue lowering: MIR rvalues → Cranelift IR instructions.
//!
//! Handles `Use`, `BinaryOp`, `UnaryOp`, `Ref`, `Aggregate`, `Discriminant`,
//! and `Cast` rvalues.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{InstBuilder, MemFlags, Value};
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
    module: &mut M,
    diagnostics: &mut Diagnostics,
) -> Option<Value> {
    match rvalue {
        Rvalue::Use(operand) => emit_operand(ctx, operand, table, definitions, func_ids, module),

        Rvalue::BinaryOp(op, lhs, rhs) => {
            let lhs_val = emit_operand(ctx, lhs, table, definitions, func_ids, module)?;
            let rhs_val = emit_operand(ctx, rhs, table, definitions, func_ids, module)?;
            let result_ty = ctx.local_type_id(dest_local);
            Some(emit_binop(ctx, *op, lhs_val, rhs_val, result_ty, table))
        }

        Rvalue::UnaryOp(op, operand) => {
            let val = emit_operand(ctx, operand, table, definitions, func_ids, module)?;
            let result_ty = ctx.local_type_id(dest_local);
            Some(emit_unaryop(ctx, *op, val, result_ty, table))
        }

        Rvalue::Ref { place, mutable: _ } => {
            // Take address of a place.
            Some(emit_place_addr(ctx, place, table))
        }

        Rvalue::Aggregate(kind, operands) => {
            emit_aggregate(ctx, kind, operands, dest_local, table, definitions, func_ids, module, diagnostics)
        }

        Rvalue::Discriminant(place) => {
            Some(emit_discriminant(ctx, place, table))
        }

        Rvalue::Cast { operand, target_ty } => {
            let val = emit_operand(ctx, operand, table, definitions, func_ids, module)?;
            // Determine source type from the operand.
            let src_ty = operand_type(operand, ctx);
            Some(emit_cast(ctx, val, src_ty, *target_ty, table))
        }
    }
}

/// Emits an operand, returning the Cranelift value.
pub fn emit_operand<M: Module>(
    ctx: &mut FunctionContext<'_>,
    operand: &Operand,
    table: &TypeTable,
    definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
    module: &mut M,
) -> Option<Value> {
    match operand {
        Operand::Copy(place) | Operand::Move(place) => {
            Some(emit_place_read(ctx, place, table))
        }
        Operand::Constant(constant) => {
            emit_constant(ctx, constant, definitions, func_ids, module)
        }
    }
}

/// Emits a constant value.
fn emit_constant<M: Module>(
    ctx: &mut FunctionContext<'_>,
    constant: &Constant,
    _definitions: &DefinitionTable,
    func_ids: &FxHashMap<spark_resolve::DefId, FuncId>,
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
            // Create a data object for the string literal.
            let name = format!(".str.{}", sym.as_raw());
            let bytes = sym.as_str().as_bytes();

            let data_id = module
                .declare_data(&name, cranelift_module::Linkage::Local, false, false)
                .expect("data declaration should succeed");

            let mut data_desc = cranelift_module::DataDescription::new();
            data_desc.define(bytes.to_vec().into_boxed_slice());
            module
                .define_data(data_id, &data_desc)
                .expect("data definition should succeed");

            let gv = module.declare_data_in_func(data_id, ctx.builder.func);
            Some(ctx.builder.ins().symbol_value(ctx.ptr_type, gv))
        }
        Constant::FunctionRef(def_id) => {
            if let Some(&func_id) = func_ids.get(def_id) {
                let func_ref = module.declare_func_in_func(func_id, ctx.builder.func);
                Some(ctx.builder.ins().func_addr(ctx.ptr_type, func_ref))
            } else {
                Some(ctx.builder.ins().iconst(ctx.ptr_type, 0))
            }
        }
    }
}

/// Reads a value from a MIR place (local + projections).
fn emit_place_read(ctx: &mut FunctionContext<'_>, place: &Place, table: &TypeTable) -> Value {
    if place.projections.is_empty() {
        // Simple local read.
        return ctx.read_local(place.local);
    }

    // Build the address through projections, then load.
    let addr = emit_place_addr(ctx, place, table);
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
fn emit_place_addr(ctx: &mut FunctionContext<'_>, place: &Place, table: &TypeTable) -> Value {
    let mut addr = match ctx.local_storage(place.local) {
        LocalStorage::StackSlot(slot, _) => ctx.builder.ins().stack_addr(ctx.ptr_type, slot, 0),
        LocalStorage::Var(_, _) => {
            // For scalar locals, we need a stack slot to take address.
            // This is a simplified approach — in practice, we'd allocate
            // a temporary stack slot. For now, just return the value.
            ctx.read_local(place.local)
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
                // For lists/arrays, compute base + idx * elem_size.
                // This is a stub — proper implementation needs runtime support.
                if let Ty::List(elem_ty) = unwrapped_ty {
                    let elem_layout = ctx.layouts.layout_of(*elem_ty, table);
                    let elem_size = ctx.builder.ins().iconst(ctx.ptr_type, i64::from(elem_layout.size));
                    let offset = ctx.builder.ins().imul(idx_val, elem_size);
                    addr = ctx.builder.ins().iadd(addr, offset);
                    current_ty = *elem_ty;
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
        }
    }

    addr
}

/// Resolves the type of a place after all projections.
fn resolve_place_type(ctx: &FunctionContext<'_>, place: &Place, table: &TypeTable) -> TypeId {
    let mut current_ty = ctx.local_type_id(place.local);

    for proj in &place.projections {
        let (_, unwrapped) = clif_types::unwrap_mutable(current_ty, table);
        match proj {
            Projection::Field(field_name) => {
                if let Ty::Struct(def_id) = unwrapped {
                    if let Some(fields) = table.get_struct_fields(*def_id) {
                        if let Some((_, field_ty)) = fields.iter().find(|(n, _)| *n == *field_name) {
                            current_ty = *field_ty;
                        }
                    }
                }
            }
            Projection::Index(_) => {
                if let Ty::List(elem_ty) = unwrapped {
                    current_ty = *elem_ty;
                }
            }
            Projection::Deref => {
                if let Ty::Ref { inner, .. } = unwrapped {
                    current_ty = *inner;
                }
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
                ctx.builder.ins().sdiv(lhs, rhs)
            } else {
                ctx.builder.ins().udiv(lhs, rhs)
            }
        }
        BinaryOp::Mod => {
            if signed {
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
    module: &mut M,
    diagnostics: &mut Diagnostics,
) -> Option<Value> {
    match kind {
        AggregateKind::Struct { def_id, fields } => {
            let dest_addr = ctx.local_stack_addr(dest_local)?;
            let sl = ctx.layouts.struct_layout_of(*def_id, table);

            for (i, operand) in operands.iter().enumerate() {
                if let Some(val) = emit_operand(ctx, operand, table, definitions, func_ids, module) {
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
                .unwrap_or(0);

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

                                if let Some(val) = emit_operand(ctx, operand, table, definitions, func_ids, module) {
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

                        if let Some(val) = emit_operand(ctx, operand, table, definitions, func_ids, module) {
                            ctx.store(val, dest_addr, i32::try_from(offset).unwrap_or(0));
                        }

                        offset += elem_layout.size;
                        max_align = max_align.max(elem_layout.align);
                    }
                }
            }

            Some(dest_addr)
        }

        AggregateKind::List | AggregateKind::Map | AggregateKind::Set => {
            // These need runtime support. Emit a diagnostic for now.
            diagnostics.warning("collection construction not yet supported in codegen");
            Some(ctx.builder.ins().iconst(ctx.ptr_type, 0))
        }
    }
}

/// Emits a discriminant read from an enum place.
fn emit_discriminant(ctx: &mut FunctionContext<'_>, place: &Place, table: &TypeTable) -> Value {
    let addr = emit_place_addr(ctx, place, table);
    let place_ty = ctx.local_type_id(place.local);
    let (_, unwrapped) = clif_types::unwrap_mutable(place_ty, table);

    if let Ty::Enum(def_id) = unwrapped {
        let el = ctx.layouts.enum_layout_of(*def_id, table);
        if el.tag_size > 0 {
            let tag_ty = clif_types::int_type_for_bytes(el.tag_size);
            return ctx.load(tag_ty, addr, 0);
        }
    }

    // Single-variant or non-enum — discriminant is 0.
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

/// Rounds `offset` up to the next multiple of `align`.
const fn align_to(offset: u32, align: u32) -> u32 {
    let mask = align - 1;
    (offset + mask) & !mask
}
