//! Equality for stored values and collection slot callbacks.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    types, AbiParam, FuncRef, Function, InstBuilder, MemFlags, Type, UserFuncName, Value,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{FuncId, Linkage, Module};
use rustc_hash::FxHashMap;
use spark_diagnostics::Diagnostics;
use spark_mir::Mir;
use spark_types::check_expr::supports_tuple_equality;
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
        let (_, ty) = super::types::unwrap_mutable(field, table);
        let field_equal = if let Ty::Tuple(nested) = ty {
            let a = builder.ins().iadd_imm(left, i64::from(offset));
            let b = builder.ins().iadd_imm(right, i64::from(offset));
            emit_tuple_equality(builder, layouts, table, ptr_type, string_eq, a, b, nested)
        } else {
            emit_scalar_equality(
                builder, table, ptr_type, string_eq, left, right, field, offset,
            )
        };
        equal = builder.ins().band(equal, field_equal);
    }
    equal
}

#[allow(clippy::too_many_arguments)]
fn emit_scalar_equality(
    builder: &mut FunctionBuilder<'_>,
    table: &TypeTable,
    ptr_type: Type,
    string_eq: FuncRef,
    left: Value,
    right: Value,
    field: TypeId,
    offset: u32,
) -> Value {
    let (field, ty) = super::types::unwrap_mutable(field, table);
    let clif_ty = abi::spark_type_to_clif(field, table, ptr_type).expect("checked scalar field");
    let offset = i32::try_from(offset).expect("field offset fits");
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
}

pub(super) fn generate_list_equalities<M: Module>(
    mir: &Mir,
    table: &TypeTable,
    string_eq: FuncId,
    module: &mut M,
    diagnostics: &mut Diagnostics,
) -> FxHashMap<TypeId, FuncId> {
    let mut ids = FxHashMap::default();
    // List construction, including hash snapshots, has a typed list destination in MIR.
    for local in mir.functions.values().flat_map(|function| &function.locals) {
        let (_, Ty::List(element)) = super::types::unwrap_mutable(local.ty, table) else {
            continue;
        };
        let (element, ty) = super::types::unwrap_mutable(*element, table);
        if ids.contains_key(&element)
            || !(matches!(ty, Ty::Float(_)) || supports_tuple_equality(element, table))
        {
            continue;
        }
        let pointer = module.target_config().pointer_type();
        let mut sig = module.make_signature();
        sig.params
            .extend([AbiParam::new(pointer), AbiParam::new(pointer)]);
        sig.returns.push(AbiParam::new(types::I8));
        let name = format!("__spark_elem_eq_{}", element.as_u32());
        let id = match module.declare_function(&name, Linkage::Local, &sig) {
            Ok(id) => id,
            Err(error) => {
                diagnostics.error(format!("could not declare list equality callback: {error}"));
                continue;
            }
        };
        let mut function = Function::with_name_signature(UserFuncName::user(0, id.as_u32()), sig);
        let string_eq = module.declare_func_in_func(string_eq, &mut function);
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut function, &mut context);
        let block = builder.create_block();
        builder.append_block_params_for_function_params(block);
        builder.switch_to_block(block);
        let left = builder.block_params(block)[0];
        let right = builder.block_params(block)[1];
        let result = if let Ty::Tuple(fields) = ty {
            emit_tuple_equality(
                &mut builder,
                &mut LayoutCache::new(pointer.bytes()),
                table,
                pointer,
                string_eq,
                left,
                right,
                fields,
            )
        } else {
            emit_scalar_equality(
                &mut builder,
                table,
                pointer,
                string_eq,
                left,
                right,
                element,
                0,
            )
        };
        builder.ins().return_(&[result]);
        builder.seal_all_blocks();
        builder.finalize();
        let mut context = cranelift_codegen::Context::for_function(function);
        if let Err(error) = module.define_function(id, &mut context) {
            diagnostics.error(format!("could not define list equality callback: {error}"));
            continue;
        }
        ids.insert(element, id);
    }
    ids
}
