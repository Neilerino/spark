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
    /// `__spark_list_new(elem_size: i64) -> ptr`
    pub list_new: FuncId,
    /// `__spark_list_push(list: ptr, elem_ptr: ptr) -> void`
    pub list_push: FuncId,
    /// `__spark_list_get(list: ptr, index: i64) -> ptr`
    pub list_get: FuncId,
    /// `__spark_map_new(key_size: i64, value_size: i64) -> ptr`
    pub map_new: FuncId,
    /// `__spark_map_insert(map: ptr, key_ptr: ptr, value_ptr: ptr) -> void`
    pub map_insert: FuncId,
    /// `__spark_map_get(map: ptr, key_ptr: ptr) -> ptr`
    pub map_get: FuncId,
    /// `__spark_set_new(elem_size: i64) -> ptr`
    pub set_new: FuncId,
    /// `__spark_set_insert(set: ptr, elem_ptr: ptr) -> void`
    pub set_insert: FuncId,
    /// `__spark_rc_release(ptr) -> void`
    pub rc_release: FuncId,
    /// `__spark_rc_inc(ptr) -> void`
    pub rc_inc: FuncId,
    /// `__spark_string_rc_dec(ptr) -> void`
    pub string_rc_dec: FuncId,
    /// `__spark_list_rc_dec(ptr, i8) -> void`
    pub list_rc_dec: FuncId,
    /// `__spark_map_rc_dec(ptr, i8, i8) -> void`
    pub map_rc_dec: FuncId,
    /// `__spark_set_rc_dec(ptr, i8) -> void`
    pub set_rc_dec: FuncId,
    /// `__spark_string_cow(ptr) -> ptr`
    pub string_cow: FuncId,
    /// `__spark_list_cow(ptr, i8) -> ptr`
    pub list_cow: FuncId,
    /// `__spark_map_cow(ptr, i8, i8) -> ptr`
    pub map_cow: FuncId,
    /// `__spark_list_len(ptr) -> i64`
    pub list_len: FuncId,
    /// `__spark_list_contains(ptr, ptr) -> i8`
    pub list_contains: FuncId,
    /// `__spark_list_remove(ptr, i64, i8) -> void`
    pub list_remove: FuncId,
    /// `__spark_map_len(ptr) -> i64`
    pub map_len: FuncId,
    /// `__spark_map_contains(ptr, ptr) -> i8`
    pub map_contains: FuncId,
    /// `__spark_map_remove(ptr, ptr, i8, i8) -> void`
    pub map_remove: FuncId,
    /// `__spark_set_len(ptr) -> i64`
    pub set_len: FuncId,
    /// `__spark_set_contains(ptr, ptr) -> i8`
    pub set_contains: FuncId,
    /// `__spark_set_remove(ptr, ptr, i8) -> void`
    pub set_remove: FuncId,
    /// `__spark_string_len(ptr) -> i64`
    pub string_len: FuncId,
    /// `__spark_print_int(i64) -> void`
    pub print_int: FuncId,
    /// `__spark_print_string(ptr) -> void`
    pub print_string: FuncId,
    /// `__spark_print_bool(i8) -> void`
    pub print_bool: FuncId,
    /// `__spark_print_float(f64) -> void`
    pub print_float: FuncId,
    /// `__spark_string_concat(ptr, ptr) -> ptr`
    pub string_concat: FuncId,
    /// `__spark_int_to_string(i64) -> ptr`
    pub int_to_string: FuncId,
    /// `__spark_float_to_string(f64) -> ptr`
    pub float_to_string: FuncId,
    /// `__spark_bool_to_string(i8) -> ptr`
    pub bool_to_string: FuncId,
    /// `__spark_env_alloc(size: i64) -> ptr`
    pub env_alloc: FuncId,
    /// `__spark_env_rc_dec(ptr) -> void`
    pub env_rc_dec: FuncId,
    /// `__spark_list_sort(ptr) -> ptr`
    pub list_sort: FuncId,
    /// `__spark_list_take(ptr, i64) -> ptr`
    pub list_take: FuncId,
    /// `__spark_list_reverse(ptr) -> ptr`
    pub list_reverse: FuncId,
    /// `__spark_parallel_foreach(list_ptr, fn_ptr, env_ptr, elem_size, cb_kind) -> void`
    pub parallel_foreach: FuncId,
    /// `__spark_parallel_map(list_ptr, fn_ptr, env_ptr, elem_size, result_elem_size, cb_kind) -> ptr`
    pub parallel_map: FuncId,
    /// `__spark_spawn(fn_ptr, env_ptr, result_size, is_float) -> ptr`
    pub spawn: FuncId,
    /// `__spark_await(future_ptr) -> ptr`
    pub await_future: FuncId,
    /// `__spark_future_rc_dec(future_ptr)`
    pub future_rc_dec: FuncId,
    /// `__spark_alloc_state(size: i64) -> ptr`
    pub alloc_state: FuncId,
    /// `__spark_spawn_coop(poll_fn, state_ptr, state_size, result_size, result_offset) -> ptr`
    pub spawn_coop: FuncId,
    /// `__spark_try_poll(future_ptr) -> ptr`
    pub try_poll: FuncId,
}

impl RuntimeStubs {
    /// Declares all runtime stubs in the given module.
    #[allow(clippy::too_many_lines)]
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
            list_new: declare_fn(
                module,
                "__spark_list_new",
                &[types::I64],
                Some(ptr_type),
                call_conv,
            ),
            list_push: declare_fn(
                module,
                "__spark_list_push",
                &[ptr_type, ptr_type],
                None,
                call_conv,
            ),
            list_get: declare_fn(
                module,
                "__spark_list_get",
                &[ptr_type, types::I64],
                Some(ptr_type),
                call_conv,
            ),
            map_new: declare_fn(
                module,
                "__spark_map_new",
                &[types::I64, types::I64],
                Some(ptr_type),
                call_conv,
            ),
            map_insert: declare_fn(
                module,
                "__spark_map_insert",
                &[ptr_type, ptr_type, ptr_type],
                None,
                call_conv,
            ),
            map_get: declare_fn(
                module,
                "__spark_map_get",
                &[ptr_type, ptr_type],
                Some(ptr_type),
                call_conv,
            ),
            set_new: declare_fn(
                module,
                "__spark_set_new",
                &[types::I64],
                Some(ptr_type),
                call_conv,
            ),
            set_insert: declare_fn(
                module,
                "__spark_set_insert",
                &[ptr_type, ptr_type],
                None,
                call_conv,
            ),
            rc_release: declare_fn(
                module,
                "__spark_rc_release",
                &[ptr_type],
                None,
                call_conv,
            ),
            rc_inc: declare_fn(
                module,
                "__spark_rc_inc",
                &[ptr_type],
                None,
                call_conv,
            ),
            string_rc_dec: declare_fn(
                module,
                "__spark_string_rc_dec",
                &[ptr_type],
                None,
                call_conv,
            ),
            list_rc_dec: declare_fn(
                module,
                "__spark_list_rc_dec",
                &[ptr_type, types::I8],
                None,
                call_conv,
            ),
            map_rc_dec: declare_fn(
                module,
                "__spark_map_rc_dec",
                &[ptr_type, types::I8, types::I8],
                None,
                call_conv,
            ),
            set_rc_dec: declare_fn(
                module,
                "__spark_set_rc_dec",
                &[ptr_type, types::I8],
                None,
                call_conv,
            ),
            string_cow: declare_fn(
                module,
                "__spark_string_cow",
                &[ptr_type],
                Some(ptr_type),
                call_conv,
            ),
            list_cow: declare_fn(
                module,
                "__spark_list_cow",
                &[ptr_type, types::I8],
                Some(ptr_type),
                call_conv,
            ),
            map_cow: declare_fn(
                module,
                "__spark_map_cow",
                &[ptr_type, types::I8, types::I8],
                Some(ptr_type),
                call_conv,
            ),
            list_len: declare_fn(
                module,
                "__spark_list_len",
                &[ptr_type],
                Some(types::I64),
                call_conv,
            ),
            list_contains: declare_fn(
                module,
                "__spark_list_contains",
                &[ptr_type, ptr_type],
                Some(types::I8),
                call_conv,
            ),
            list_remove: declare_fn(
                module,
                "__spark_list_remove",
                &[ptr_type, types::I64, types::I8],
                None,
                call_conv,
            ),
            map_len: declare_fn(
                module,
                "__spark_map_len",
                &[ptr_type],
                Some(types::I64),
                call_conv,
            ),
            map_contains: declare_fn(
                module,
                "__spark_map_contains",
                &[ptr_type, ptr_type],
                Some(types::I8),
                call_conv,
            ),
            map_remove: declare_fn(
                module,
                "__spark_map_remove",
                &[ptr_type, ptr_type, types::I8, types::I8],
                None,
                call_conv,
            ),
            set_len: declare_fn(
                module,
                "__spark_set_len",
                &[ptr_type],
                Some(types::I64),
                call_conv,
            ),
            set_contains: declare_fn(
                module,
                "__spark_set_contains",
                &[ptr_type, ptr_type],
                Some(types::I8),
                call_conv,
            ),
            set_remove: declare_fn(
                module,
                "__spark_set_remove",
                &[ptr_type, ptr_type, types::I8],
                None,
                call_conv,
            ),
            string_len: declare_fn(
                module,
                "__spark_string_len",
                &[ptr_type],
                Some(types::I64),
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
            string_concat: declare_fn(
                module,
                "__spark_string_concat",
                &[ptr_type, ptr_type],
                Some(ptr_type),
                call_conv,
            ),
            int_to_string: declare_fn(
                module,
                "__spark_int_to_string",
                &[types::I64],
                Some(ptr_type),
                call_conv,
            ),
            float_to_string: declare_fn(
                module,
                "__spark_float_to_string",
                &[types::F64],
                Some(ptr_type),
                call_conv,
            ),
            bool_to_string: declare_fn(
                module,
                "__spark_bool_to_string",
                &[types::I8],
                Some(ptr_type),
                call_conv,
            ),
            env_alloc: declare_fn(
                module,
                "__spark_env_alloc",
                &[types::I64],
                Some(ptr_type),
                call_conv,
            ),
            env_rc_dec: declare_fn(
                module,
                "__spark_env_rc_dec",
                &[ptr_type],
                None,
                call_conv,
            ),
            list_sort: declare_fn(
                module,
                "__spark_list_sort",
                &[ptr_type],
                Some(ptr_type),
                call_conv,
            ),
            list_take: declare_fn(
                module,
                "__spark_list_take",
                &[ptr_type, types::I64],
                Some(ptr_type),
                call_conv,
            ),
            list_reverse: declare_fn(
                module,
                "__spark_list_reverse",
                &[ptr_type],
                Some(ptr_type),
                call_conv,
            ),
            parallel_foreach: declare_fn(
                module,
                "__spark_parallel_foreach",
                &[ptr_type, ptr_type, ptr_type, types::I64, types::I8],
                None,
                call_conv,
            ),
            parallel_map: declare_fn(
                module,
                "__spark_parallel_map",
                &[ptr_type, ptr_type, ptr_type, types::I64, types::I64, types::I8],
                Some(ptr_type),
                call_conv,
            ),
            spawn: declare_fn(
                module,
                "__spark_spawn",
                &[ptr_type, ptr_type, types::I64, types::I8],
                Some(ptr_type),
                call_conv,
            ),
            await_future: declare_fn(
                module,
                "__spark_await",
                &[ptr_type],
                Some(ptr_type),
                call_conv,
            ),
            future_rc_dec: declare_fn(
                module,
                "__spark_future_rc_dec",
                &[ptr_type],
                None,
                call_conv,
            ),
            alloc_state: declare_fn(
                module,
                "__spark_alloc_state",
                &[types::I64],
                Some(ptr_type),
                call_conv,
            ),
            spawn_coop: declare_fn(
                module,
                "__spark_spawn_coop",
                &[ptr_type, ptr_type, types::I64, types::I64, types::I64],
                Some(ptr_type),
                call_conv,
            ),
            try_poll: declare_fn(
                module,
                "__spark_try_poll",
                &[ptr_type],
                Some(ptr_type),
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
