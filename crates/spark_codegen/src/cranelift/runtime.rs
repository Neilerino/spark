//! Runtime function stubs for features that need runtime support.
//!
//! Declares external functions that will be provided by a `spark_runtime`
//! library at link time. Until that library exists, these remain unresolved
//! external symbols.

use cranelift_codegen::ir::{types, AbiParam, Signature};
use cranelift_codegen::isa::CallConv;
use cranelift_module::{FuncId, Linkage, Module};

/// Collection of runtime function declarations.
#[derive(Debug)]
pub struct RuntimeStubs {
    /// `__spark_string_new() -> ptr`
    pub string_new: FuncId,
    /// `__spark_string_from_literal(data: ptr, len: i64) -> ptr`
    pub string_from_literal: FuncId,
    /// `__spark_list_new() -> ptr`
    pub list_new: FuncId,
    /// `__spark_map_new() -> ptr`
    pub map_new: FuncId,
    /// `__spark_set_new() -> ptr`
    pub set_new: FuncId,
    /// `__spark_rc_release(ptr) -> void`
    pub rc_release: FuncId,
    /// `__spark_print_int(i64) -> void`
    pub print_int: FuncId,
    /// `__spark_print_string(ptr) -> void`
    pub print_string: FuncId,
    /// `__spark_print_bool(i8) -> void`
    pub print_bool: FuncId,
    /// `__spark_print_float(f64) -> void`
    pub print_float: FuncId,
}

impl RuntimeStubs {
    /// Declares all runtime stubs in the given module.
    pub fn declare_all<M: Module>(module: &mut M, call_conv: CallConv) -> Self {
        let ptr_type = module.target_config().pointer_type();

        Self {
            string_new: declare_fn(
                module,
                "__spark_string_new",
                &[],
                Some(ptr_type),
                call_conv,
            ),
            string_from_literal: declare_fn(
                module,
                "__spark_string_from_literal",
                &[ptr_type, types::I64],
                Some(ptr_type),
                call_conv,
            ),
            list_new: declare_fn(module, "__spark_list_new", &[], Some(ptr_type), call_conv),
            map_new: declare_fn(module, "__spark_map_new", &[], Some(ptr_type), call_conv),
            set_new: declare_fn(module, "__spark_set_new", &[], Some(ptr_type), call_conv),
            rc_release: declare_fn(
                module,
                "__spark_rc_release",
                &[ptr_type],
                None,
                call_conv,
            ),
            print_int: declare_fn(
                module,
                "__spark_print_int",
                &[types::I64],
                None,
                call_conv,
            ),
            print_string: declare_fn(
                module,
                "__spark_print_string",
                &[ptr_type],
                None,
                call_conv,
            ),
            print_bool: declare_fn(
                module,
                "__spark_print_bool",
                &[types::I8],
                None,
                call_conv,
            ),
            print_float: declare_fn(
                module,
                "__spark_print_float",
                &[types::F64],
                None,
                call_conv,
            ),
        }
    }
}

/// Helper to declare a single runtime function as an imported symbol.
fn declare_fn<M: Module>(
    module: &mut M,
    name: &str,
    params: &[cranelift_codegen::ir::Type],
    ret: Option<cranelift_codegen::ir::Type>,
    call_conv: CallConv,
) -> FuncId {
    let mut sig = Signature::new(call_conv);
    for &p in params {
        sig.params.push(AbiParam::new(p));
    }
    if let Some(r) = ret {
        sig.returns.push(AbiParam::new(r));
    }
    module
        .declare_function(name, Linkage::Import, &sig)
        .expect("runtime stub declaration should not fail")
}
