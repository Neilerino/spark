//! Runtime function stubs for features that need runtime support.
//!
//! Declares external functions that will be provided by the `spark_runtime`
//! library at link time. Each stub is declared from its canonical
//! [`spark_abi::RuntimeFn`] signature, ensuring that codegen and runtime
//! always agree on parameter types and return values.

use cranelift_codegen::ir::{types, AbiParam, Signature};
use cranelift_codegen::isa::CallConv;
use cranelift_module::{FuncId, Linkage, Module};
use spark_abi::{AbiType, RuntimeFn};

/// Collection of runtime function declarations.
#[derive(Debug)]
pub struct RuntimeStubs {
    // ── String ──────────────────────────────────────────────────────
    /// `__spark_string_new`
    pub string_new: FuncId,
    /// `__spark_string_from_literal`
    pub string_from_literal: FuncId,
    /// `__spark_string_rc_dec`
    pub string_rc_dec: FuncId,
    /// `__spark_string_cow`
    pub string_cow: FuncId,
    /// `__spark_string_len`
    pub string_len: FuncId,
    /// `__spark_string_lines`
    pub string_lines: FuncId,
    /// `__spark_string_split`
    pub string_split: FuncId,
    /// `__spark_string_split_whitespace`
    pub string_split_whitespace: FuncId,
    /// `__spark_string_trim`
    pub string_trim: FuncId,
    /// `__spark_string_sections`
    pub string_sections: FuncId,
    /// `__spark_string_chars`
    pub string_chars: FuncId,
    /// `__spark_string_concat`
    pub string_concat: FuncId,
    /// `__spark_string_eq`
    pub string_eq: FuncId,
    /// `__spark_hash_string_elem`
    pub hash_string_elem: FuncId,
    /// `__spark_eq_string_elem`
    pub eq_string_elem: FuncId,

    // ── Runtime value descriptors ───────────────────────────────────────
    /// `__spark_value_desc_new`
    pub value_desc_new: FuncId,

    // ── Conversion ──────────────────────────────────────────────────
    /// `__spark_int_to_string`
    pub int_to_string: FuncId,
    /// `__spark_float_to_string`
    pub float_to_string: FuncId,
    /// `__spark_bool_to_string`
    pub bool_to_string: FuncId,

    // ── Print ───────────────────────────────────────────────────────
    /// `__spark_print_int`
    pub print_int: FuncId,
    /// `__spark_print_string`
    pub print_string: FuncId,
    /// `__spark_print_bool`
    pub print_bool: FuncId,
    /// `__spark_print_float`
    pub print_float: FuncId,

    // ── List ────────────────────────────────────────────────────────
    /// `__spark_list_new`
    pub list_new: FuncId,
    /// `__spark_list_push`
    pub list_push: FuncId,
    /// `__spark_list_get`
    pub list_get: FuncId,
    /// `__spark_list_try_get`
    pub list_try_get: FuncId,
    /// `__spark_list_write_slot`
    pub list_write_slot: FuncId,
    /// `__spark_list_replace`
    pub list_replace: FuncId,
    /// `__spark_list_rc_dec`
    pub list_rc_dec: FuncId,
    /// `__spark_list_cow`
    pub list_cow: FuncId,
    /// `__spark_list_len`
    pub list_len: FuncId,
    /// `__spark_list_contains`
    pub list_contains: FuncId,
    /// `__spark_list_remove`
    pub list_remove: FuncId,
    /// `__spark_list_sort`
    pub list_sort: FuncId,
    /// `__spark_list_take`
    pub list_take: FuncId,
    /// `__spark_list_reverse`
    pub list_reverse: FuncId,
    /// `__spark_list_from_range`
    pub list_from_range: FuncId,

    // ── Map ─────────────────────────────────────────────────────────
    /// `__spark_map_new`
    pub map_new: FuncId,
    /// `__spark_map_insert`
    pub map_insert: FuncId,
    /// `__spark_map_get`
    pub map_get: FuncId,
    /// `__spark_map_rc_dec`
    pub map_rc_dec: FuncId,
    /// `__spark_map_cow`
    pub map_cow: FuncId,
    /// `__spark_map_len`
    pub map_len: FuncId,
    /// `__spark_map_contains`
    pub map_contains: FuncId,
    /// `__spark_map_remove`
    pub map_remove: FuncId,

    // ── Set ─────────────────────────────────────────────────────────
    /// `__spark_set_new`
    pub set_new: FuncId,
    /// `__spark_set_insert`
    pub set_insert: FuncId,
    /// `__spark_set_rc_dec`
    pub set_rc_dec: FuncId,
    /// `__spark_set_cow`
    pub set_cow: FuncId,
    /// `__spark_set_len`
    pub set_len: FuncId,
    /// `__spark_set_contains`
    pub set_contains: FuncId,
    /// `__spark_set_remove`
    pub set_remove: FuncId,

    // ── Reference Counting ──────────────────────────────────────────
    /// `__spark_rc_release`
    pub rc_release: FuncId,
    /// `__spark_rc_inc`
    pub rc_inc: FuncId,

    // ── Closure Environment ─────────────────────────────────────────
    /// `__spark_env_alloc`
    pub env_alloc: FuncId,
    /// `__spark_env_rc_dec`
    pub env_rc_dec: FuncId,

    // ── Concurrency ─────────────────────────────────────────────────
    /// `__spark_spawn`
    pub spawn: FuncId,
    /// `__spark_await`
    pub await_future: FuncId,
    /// `__spark_future_rc_dec`
    pub future_rc_dec: FuncId,
    /// `__spark_alloc_state`
    pub alloc_state: FuncId,
    /// `__spark_spawn_coop`
    pub spawn_coop: FuncId,
    /// `__spark_try_poll`
    pub try_poll: FuncId,

    // ── Channels ────────────────────────────────────────────────────
    /// `__spark_channel_new`
    pub channel_new: FuncId,
    /// `__spark_channel_send`
    pub channel_send: FuncId,
    /// `__spark_channel_recv`
    pub channel_recv: FuncId,
    /// `__spark_channel_try_recv`
    pub channel_try_recv: FuncId,
    /// `__spark_channel_close`
    pub channel_close: FuncId,
    /// `__spark_channel_rc_dec`
    pub channel_rc_dec: FuncId,

    // ── Range / Parallel ────────────────────────────────────────────
    /// `__spark_range_len`
    pub range_len: FuncId,
    /// `__spark_parallel_foreach`
    pub parallel_foreach: FuncId,
    /// `__spark_parallel_map`
    pub parallel_map: FuncId,
    /// `__spark_parallel_map_reduce`
    pub parallel_map_reduce: FuncId,
    /// `__spark_parallel_range_foreach`
    pub parallel_range_foreach: FuncId,
    /// `__spark_parallel_range_map`
    pub parallel_range_map: FuncId,
    /// `__spark_parallel_range_map_reduce`
    pub parallel_range_map_reduce: FuncId,
}

impl RuntimeStubs {
    /// Declares all runtime stubs in the given module.
    ///
    /// Each stub is declared from its canonical [`spark_abi::RuntimeFn`]
    /// signature, so any mismatch between codegen and runtime surfaces as
    /// a compile-time error in `spark_abi` rather than a silent runtime crash.
    #[allow(clippy::too_many_lines)]
    pub fn declare_all<M: Module>(module: &mut M, call_conv: CallConv) -> Self {
        use spark_abi as abi;
        let ptr_type = module.target_config().pointer_type();

        Self {
            // String
            string_new: declare_abi(module, &abi::STRING_NEW, ptr_type, call_conv),
            string_from_literal: declare_abi(
                module,
                &abi::STRING_FROM_LITERAL,
                ptr_type,
                call_conv,
            ),
            string_rc_dec: declare_abi(module, &abi::STRING_RC_DEC, ptr_type, call_conv),
            string_cow: declare_abi(module, &abi::STRING_COW, ptr_type, call_conv),
            string_len: declare_abi(module, &abi::STRING_LEN, ptr_type, call_conv),
            string_lines: declare_abi(module, &abi::STRING_LINES, ptr_type, call_conv),
            string_split: declare_abi(module, &abi::STRING_SPLIT, ptr_type, call_conv),
            string_split_whitespace: declare_abi(
                module,
                &abi::STRING_SPLIT_WHITESPACE,
                ptr_type,
                call_conv,
            ),
            string_trim: declare_abi(module, &abi::STRING_TRIM, ptr_type, call_conv),
            string_sections: declare_abi(module, &abi::STRING_SECTIONS, ptr_type, call_conv),
            string_chars: declare_abi(module, &abi::STRING_CHARS, ptr_type, call_conv),
            string_concat: declare_abi(module, &abi::STRING_CONCAT, ptr_type, call_conv),
            string_eq: declare_abi(module, &abi::STRING_EQ, ptr_type, call_conv),
            hash_string_elem: declare_abi(module, &abi::HASH_STRING_ELEM, ptr_type, call_conv),
            eq_string_elem: declare_abi(module, &abi::EQ_STRING_ELEM, ptr_type, call_conv),

            // Runtime value descriptors
            value_desc_new: declare_abi(module, &abi::VALUE_DESC_NEW, ptr_type, call_conv),

            // Conversion
            int_to_string: declare_abi(module, &abi::INT_TO_STRING, ptr_type, call_conv),
            float_to_string: declare_abi(module, &abi::FLOAT_TO_STRING, ptr_type, call_conv),
            bool_to_string: declare_abi(module, &abi::BOOL_TO_STRING, ptr_type, call_conv),

            // Print
            print_int: declare_abi(module, &abi::PRINT_INT, ptr_type, call_conv),
            print_string: declare_abi(module, &abi::PRINT_STRING, ptr_type, call_conv),
            print_bool: declare_abi(module, &abi::PRINT_BOOL, ptr_type, call_conv),
            print_float: declare_abi(module, &abi::PRINT_FLOAT, ptr_type, call_conv),

            // List
            list_new: declare_abi(module, &abi::LIST_NEW, ptr_type, call_conv),
            list_push: declare_abi(module, &abi::LIST_PUSH, ptr_type, call_conv),
            list_get: declare_abi(module, &abi::LIST_GET, ptr_type, call_conv),
            list_try_get: declare_abi(module, &abi::LIST_TRY_GET, ptr_type, call_conv),
            list_write_slot: declare_abi(module, &abi::LIST_WRITE_SLOT, ptr_type, call_conv),
            list_replace: declare_abi(module, &abi::LIST_REPLACE, ptr_type, call_conv),
            list_rc_dec: declare_abi(module, &abi::LIST_RC_DEC, ptr_type, call_conv),
            list_cow: declare_abi(module, &abi::LIST_COW, ptr_type, call_conv),
            list_len: declare_abi(module, &abi::LIST_LEN, ptr_type, call_conv),
            list_contains: declare_abi(module, &abi::LIST_CONTAINS, ptr_type, call_conv),
            list_remove: declare_abi(module, &abi::LIST_REMOVE, ptr_type, call_conv),
            list_sort: declare_abi(module, &abi::LIST_SORT, ptr_type, call_conv),
            list_take: declare_abi(module, &abi::LIST_TAKE, ptr_type, call_conv),
            list_reverse: declare_abi(module, &abi::LIST_REVERSE, ptr_type, call_conv),
            list_from_range: declare_abi(module, &abi::LIST_FROM_RANGE, ptr_type, call_conv),

            // Map
            map_new: declare_abi(module, &abi::MAP_NEW, ptr_type, call_conv),
            map_insert: declare_abi(module, &abi::MAP_INSERT, ptr_type, call_conv),
            map_get: declare_abi(module, &abi::MAP_GET, ptr_type, call_conv),
            map_rc_dec: declare_abi(module, &abi::MAP_RC_DEC, ptr_type, call_conv),
            map_cow: declare_abi(module, &abi::MAP_COW, ptr_type, call_conv),
            map_len: declare_abi(module, &abi::MAP_LEN, ptr_type, call_conv),
            map_contains: declare_abi(module, &abi::MAP_CONTAINS, ptr_type, call_conv),
            map_remove: declare_abi(module, &abi::MAP_REMOVE, ptr_type, call_conv),

            // Set
            set_new: declare_abi(module, &abi::SET_NEW, ptr_type, call_conv),
            set_insert: declare_abi(module, &abi::SET_INSERT, ptr_type, call_conv),
            set_rc_dec: declare_abi(module, &abi::SET_RC_DEC, ptr_type, call_conv),
            set_cow: declare_abi(module, &abi::SET_COW, ptr_type, call_conv),
            set_len: declare_abi(module, &abi::SET_LEN, ptr_type, call_conv),
            set_contains: declare_abi(module, &abi::SET_CONTAINS, ptr_type, call_conv),
            set_remove: declare_abi(module, &abi::SET_REMOVE, ptr_type, call_conv),

            // RC
            rc_release: declare_abi(module, &abi::RC_RELEASE, ptr_type, call_conv),
            rc_inc: declare_abi(module, &abi::RC_INC, ptr_type, call_conv),

            // Env
            env_alloc: declare_abi(module, &abi::ENV_ALLOC, ptr_type, call_conv),
            env_rc_dec: declare_abi(module, &abi::ENV_RC_DEC, ptr_type, call_conv),

            // Concurrency
            spawn: declare_abi(module, &abi::SPAWN, ptr_type, call_conv),
            await_future: declare_abi(module, &abi::AWAIT_FUTURE, ptr_type, call_conv),
            future_rc_dec: declare_abi(module, &abi::FUTURE_RC_DEC, ptr_type, call_conv),
            alloc_state: declare_abi(module, &abi::ALLOC_STATE, ptr_type, call_conv),
            spawn_coop: declare_abi(module, &abi::SPAWN_COOP, ptr_type, call_conv),
            try_poll: declare_abi(module, &abi::TRY_POLL, ptr_type, call_conv),

            // Channels
            channel_new: declare_abi(module, &abi::CHANNEL_NEW, ptr_type, call_conv),
            channel_send: declare_abi(module, &abi::CHANNEL_SEND, ptr_type, call_conv),
            channel_recv: declare_abi(module, &abi::CHANNEL_RECV, ptr_type, call_conv),
            channel_try_recv: declare_abi(module, &abi::CHANNEL_TRY_RECV, ptr_type, call_conv),
            channel_close: declare_abi(module, &abi::CHANNEL_CLOSE, ptr_type, call_conv),
            channel_rc_dec: declare_abi(module, &abi::CHANNEL_RC_DEC, ptr_type, call_conv),

            // Range / Parallel
            range_len: declare_abi(module, &abi::RANGE_LEN, ptr_type, call_conv),
            parallel_foreach: declare_abi(module, &abi::PARALLEL_FOREACH, ptr_type, call_conv),
            parallel_map: declare_abi(module, &abi::PARALLEL_MAP, ptr_type, call_conv),
            parallel_map_reduce: declare_abi(
                module,
                &abi::PARALLEL_MAP_REDUCE,
                ptr_type,
                call_conv,
            ),
            parallel_range_foreach: declare_abi(
                module,
                &abi::PARALLEL_RANGE_FOREACH,
                ptr_type,
                call_conv,
            ),
            parallel_range_map: declare_abi(module, &abi::PARALLEL_RANGE_MAP, ptr_type, call_conv),
            parallel_range_map_reduce: declare_abi(
                module,
                &abi::PARALLEL_RANGE_MAP_REDUCE,
                ptr_type,
                call_conv,
            ),
        }
    }
}

/// Converts an [`AbiType`] to a Cranelift IR type.
const fn abi_to_clif(
    abi: AbiType,
    ptr_type: cranelift_codegen::ir::Type,
) -> cranelift_codegen::ir::Type {
    match abi {
        AbiType::I8 => types::I8,
        AbiType::I64 => types::I64,
        AbiType::F32 => types::F32,
        AbiType::F64 => types::F64,
        AbiType::Ptr => ptr_type,
    }
}

/// Declares a runtime function from its [`RuntimeFn`] ABI definition.
fn declare_abi<M: Module>(
    module: &mut M,
    func: &RuntimeFn,
    ptr_type: cranelift_codegen::ir::Type,
    call_conv: CallConv,
) -> FuncId {
    let mut sig = Signature::new(call_conv);
    for &p in func.params {
        sig.params.push(AbiParam::new(abi_to_clif(p, ptr_type)));
    }
    if let Some(r) = func.ret {
        sig.returns.push(AbiParam::new(abi_to_clif(r, ptr_type)));
    }
    module
        .declare_function(func.name, Linkage::Import, &sig)
        .unwrap_or_else(|e| {
            panic!(
                "ICE: failed to declare runtime stub '{}': {e}; this is a compiler bug",
                func.name
            )
        })
}
