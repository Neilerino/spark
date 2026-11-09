//! Native scalar cast conversions shared by the supported target architectures.

use cranelift_codegen::ir::{types, InstBuilder, Value};
use spark_types::{FloatWidth, IntWidth, Ty, TypeId, TypeTable};

use super::context::FunctionContext;
use super::types as clif_types;
use crate::abi;

pub(super) fn convert_scalar(
    ctx: &mut FunctionContext<'_>,
    val: Value,
    src_ty: TypeId,
    target_ty: TypeId,
    table: &TypeTable,
) -> Option<Value> {
    let (_, src) = clif_types::unwrap_mutable(src_ty, table);
    let (_, tgt) = clif_types::unwrap_mutable(target_ty, table);

    let result = match (src, tgt) {
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
        (Ty::Float(_), Ty::Int(w)) => float_to_int(ctx, val, *w),

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
            if int_ty == types::I8 {
                val
            } else {
                ctx.builder.ins().uextend(int_ty, val)
            }
        }
        (Ty::Byte, Ty::Int(IntWidth::U8)) | (Ty::Int(IntWidth::U8), Ty::Byte) => val,
        (Ty::Float(a), Ty::Float(b)) if a == b => val,
        _ => return None,
    };
    Some(result)
}

fn float_to_int(ctx: &mut FunctionContext<'_>, val: Value, width: IntWidth) -> Value {
    let target = clif_types::int_width_to_clif(width);
    let conversion = if target.bits() < 32 {
        types::I32
    } else {
        target
    };
    let signed = abi::int_is_signed(width);
    let converted = if signed {
        ctx.builder.ins().fcvt_to_sint_sat(conversion, val)
    } else {
        ctx.builder.ins().fcvt_to_uint_sat(conversion, val)
    };
    if conversion == target {
        return converted;
    }
    // x86-64 converts floats to 32- or 64-bit integers. Clamp before narrowing.
    let clamped = if signed {
        let maximum = ctx
            .builder
            .ins()
            .iconst(types::I32, (1_i64 << (target.bits() - 1)) - 1);
        let minimum = ctx
            .builder
            .ins()
            .iconst(types::I32, -(1_i64 << (target.bits() - 1)));
        let upper = ctx.builder.ins().smin(converted, maximum);
        ctx.builder.ins().smax(upper, minimum)
    } else {
        let maximum = ctx
            .builder
            .ins()
            .iconst(types::I32, (1_i64 << target.bits()) - 1);
        ctx.builder.ins().umin(converted, maximum)
    };
    ctx.builder.ins().ireduce(target, clamped)
}
