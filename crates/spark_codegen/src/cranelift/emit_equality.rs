//! Fieldwise equality for stored tuple values.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{types, FuncRef, InstBuilder, MemFlags, Type, Value};
use cranelift_frontend::FunctionBuilder;
use spark_types::{Ty, TypeId, TypeTable};

use crate::{abi, layout::LayoutCache};

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_tuple_equality(
    builder: &mut FunctionBuilder<'_>,
    layouts: &mut LayoutCache,
    table: &TypeTable,
    ptr_type: Type,
    string_eq: FuncRef,
    left: Value,
    right: Value,
    fields: &[TypeId],
) -> Value {
    let mut equal = builder.ins().iconst(types::I8, 1);
    for (index, &field) in fields.iter().enumerate() {
        let offset = layouts.tuple_field_offset(fields, index, table);
        let (field, ty) = super::types::unwrap_mutable(field, table);
        let field_equal = if let Ty::Tuple(nested) = ty {
            let a = builder.ins().iadd_imm(left, i64::from(offset));
            let b = builder.ins().iadd_imm(right, i64::from(offset));
            emit_tuple_equality(builder, layouts, table, ptr_type, string_eq, a, b, nested)
        } else {
            let clif_ty = abi::spark_type_to_clif(field, table, ptr_type)
                .expect("checked scalar tuple field");
            let offset = i32::try_from(offset).expect("tuple field offset fits");
            let a = builder.ins().load(clif_ty, MemFlags::new(), left, offset);
            let b = builder.ins().load(clif_ty, MemFlags::new(), right, offset);
            match ty {
                Ty::Float(_) => builder.ins().fcmp(FloatCC::Equal, a, b),
                Ty::String => {
                    let call = builder.ins().call(string_eq, &[a, b]);
                    builder.inst_results(call)[0]
                }
                _ => builder.ins().icmp(IntCC::Equal, a, b),
            }
        };
        equal = builder.ins().band(equal, field_equal);
    }
    equal
}
