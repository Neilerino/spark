//! # `spark_abi`
//!
//! Shared ABI definitions between the Spark codegen and runtime.
//!
//! This crate is the single source of truth for:
//! - Closure environment header layout constants
//! - Runtime function signature declarations
//!
//! Both `spark_codegen` and `spark_runtime` depend on this crate, ensuring
//! they always agree on struct layouts and function signatures. Changing
//! a constant or signature here will surface as compile errors in both
//! consumers rather than silent runtime corruption.

#![forbid(unsafe_code)]

// ── Closure Environment Header ──────────────────────────────────────────
//
// The closure env is a heap-allocated block with a fixed 24-byte header
// followed by captured variables:
//
//   [rc: usize]  [alloc_size: usize]  [dtor: fn(*mut u8)]  [captures...]
//    offset 0       offset 8              offset 16           offset 24
//

/// Byte offset of the reference count in the closure env header.
pub const ENV_RC_OFFSET: u32 = 0;

/// Byte offset of the allocation size in the closure env header.
pub const ENV_ALLOC_SIZE_OFFSET: u32 = 8;

/// Byte offset of the destructor function pointer in the closure env header.
pub const ENV_DTOR_OFFSET: u32 = 16;

/// Total size of the closure env header in bytes.
pub const ENV_HEADER_SIZE: u32 = 24;

/// Byte offset where captured variables begin (immediately after the header).
pub const ENV_CAPTURES_OFFSET: u32 = ENV_HEADER_SIZE;

// ── Runtime Function Signature Registry ─────────────────────────────────

/// ABI-level type classification for runtime function parameters/returns.
///
/// These map to Cranelift types in codegen and to Rust FFI types in the
/// runtime. The mapping is:
/// - `I8`  -> `cranelift::types::I8`  / `i8`
/// - `I64` -> `cranelift::types::I64` / `i64` or `usize`
/// - `F32` -> `cranelift::types::F32` / `f32`
/// - `F64` -> `cranelift::types::F64` / `f64`
/// - `Ptr` -> target pointer type     / `*mut u8`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbiType {
    /// 8-bit integer (booleans, small flags).
    I8,
    /// 64-bit integer (sizes, indices, integer values).
    I64,
    /// 32-bit float.
    F32,
    /// 64-bit float.
    F64,
    /// Pointer-sized value (pointers, function pointers).
    Ptr,
}

/// A runtime function signature: name, parameters, and optional return type.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeFn {
    /// The `#[no_mangle]` symbol name (e.g. `"__spark_list_new"`).
    pub name: &'static str,
    /// Parameter types in order.
    pub params: &'static [AbiType],
    /// Return type, or `None` for void functions.
    pub ret: Option<AbiType>,
}

use AbiType::{Ptr, F64, I64, I8};

// ── String ──────────────────────────────────────────────────────────────

/// `__spark_string_new() -> ptr`
pub const STRING_NEW: RuntimeFn = RuntimeFn {
    name: "__spark_string_new",
    params: &[],
    ret: Some(Ptr),
};

/// `__spark_string_from_literal(data: ptr, len: i64) -> ptr`
pub const STRING_FROM_LITERAL: RuntimeFn = RuntimeFn {
    name: "__spark_string_from_literal",
    params: &[Ptr, I64],
    ret: Some(Ptr),
};

/// `__spark_string_rc_dec(ptr) -> void`
pub const STRING_RC_DEC: RuntimeFn = RuntimeFn {
    name: "__spark_string_rc_dec",
    params: &[Ptr],
    ret: None,
};

/// `__spark_string_cow(ptr) -> ptr`
pub const STRING_COW: RuntimeFn = RuntimeFn {
    name: "__spark_string_cow",
    params: &[Ptr],
    ret: Some(Ptr),
};

/// `__spark_string_len(ptr) -> i64`
pub const STRING_LEN: RuntimeFn = RuntimeFn {
    name: "__spark_string_len",
    params: &[Ptr],
    ret: Some(I64),
};

/// `__spark_string_lines(ptr) -> ptr`
pub const STRING_LINES: RuntimeFn = RuntimeFn {
    name: "__spark_string_lines",
    params: &[Ptr],
    ret: Some(Ptr),
};

/// `__spark_string_split(ptr, ptr) -> ptr`
pub const STRING_SPLIT: RuntimeFn = RuntimeFn {
    name: "__spark_string_split",
    params: &[Ptr, Ptr],
    ret: Some(Ptr),
};

/// `__spark_string_split_whitespace(ptr) -> ptr`
pub const STRING_SPLIT_WHITESPACE: RuntimeFn = RuntimeFn {
    name: "__spark_string_split_whitespace",
    params: &[Ptr],
    ret: Some(Ptr),
};

/// `__spark_string_trim(ptr) -> ptr`
pub const STRING_TRIM: RuntimeFn = RuntimeFn {
    name: "__spark_string_trim",
    params: &[Ptr],
    ret: Some(Ptr),
};

/// `__spark_string_sections(ptr) -> ptr`
pub const STRING_SECTIONS: RuntimeFn = RuntimeFn {
    name: "__spark_string_sections",
    params: &[Ptr],
    ret: Some(Ptr),
};

/// `__spark_string_chars(ptr) -> ptr`
pub const STRING_CHARS: RuntimeFn = RuntimeFn {
    name: "__spark_string_chars",
    params: &[Ptr],
    ret: Some(Ptr),
};

/// `__spark_string_concat(ptr, ptr) -> ptr`
pub const STRING_CONCAT: RuntimeFn = RuntimeFn {
    name: "__spark_string_concat",
    params: &[Ptr, Ptr],
    ret: Some(Ptr),
};

/// `__spark_string_eq(ptr, ptr) -> i8`
pub const STRING_EQ: RuntimeFn = RuntimeFn {
    name: "__spark_string_eq",
    params: &[Ptr, Ptr],
    ret: Some(I8),
};

/// `__spark_hash_string_elem(elem_ptr: ptr) -> i64`
pub const HASH_STRING_ELEM: RuntimeFn = RuntimeFn {
    name: "__spark_hash_string_elem",
    params: &[Ptr],
    ret: Some(I64),
};

/// `__spark_eq_string_elem(a: ptr, b: ptr) -> i8`
pub const EQ_STRING_ELEM: RuntimeFn = RuntimeFn {
    name: "__spark_eq_string_elem",
    params: &[Ptr, Ptr],
    ret: Some(I8),
};

// ── Conversion ──────────────────────────────────────────────────────────

/// `__spark_int_to_string(i64) -> ptr`
pub const INT_TO_STRING: RuntimeFn = RuntimeFn {
    name: "__spark_int_to_string",
    params: &[I64],
    ret: Some(Ptr),
};

/// `__spark_float_to_string(f64) -> ptr`
pub const FLOAT_TO_STRING: RuntimeFn = RuntimeFn {
    name: "__spark_float_to_string",
    params: &[F64],
    ret: Some(Ptr),
};

/// `__spark_bool_to_string(i8) -> ptr`
pub const BOOL_TO_STRING: RuntimeFn = RuntimeFn {
    name: "__spark_bool_to_string",
    params: &[I8],
    ret: Some(Ptr),
};

// ── Print ───────────────────────────────────────────────────────────────

/// `__spark_print_int(i64) -> void`
pub const PRINT_INT: RuntimeFn = RuntimeFn {
    name: "__spark_print_int",
    params: &[I64],
    ret: None,
};

/// `__spark_print_string(ptr) -> void`
pub const PRINT_STRING: RuntimeFn = RuntimeFn {
    name: "__spark_print_string",
    params: &[Ptr],
    ret: None,
};

/// `__spark_print_bool(i8) -> void`
pub const PRINT_BOOL: RuntimeFn = RuntimeFn {
    name: "__spark_print_bool",
    params: &[I8],
    ret: None,
};

/// `__spark_print_float(f64) -> void`
pub const PRINT_FLOAT: RuntimeFn = RuntimeFn {
    name: "__spark_print_float",
    params: &[F64],
    ret: None,
};

// ── Runtime value descriptors ───────────────────────────────────────────

/// `__spark_value_desc_new(size: i64, align: i64, drop: ptr, retain: ptr, hash: ptr, eq: ptr) -> ptr`
pub const VALUE_DESC_NEW: RuntimeFn = RuntimeFn {
    name: "__spark_value_desc_new",
    params: &[I64, I64, Ptr, Ptr, Ptr, Ptr],
    ret: Some(Ptr),
};

// ── List ────────────────────────────────────────────────────────────────

/// `__spark_list_new(elem_desc: ptr) -> ptr`
pub const LIST_NEW: RuntimeFn = RuntimeFn {
    name: "__spark_list_new",
    params: &[Ptr],
    ret: Some(Ptr),
};

/// `__spark_list_push(list: ptr, elem_ptr: ptr) -> void`
pub const LIST_PUSH: RuntimeFn = RuntimeFn {
    name: "__spark_list_push",
    params: &[Ptr, Ptr],
    ret: None,
};

/// `__spark_list_get(list: ptr, index: i64) -> ptr`
pub const LIST_GET: RuntimeFn = RuntimeFn {
    name: "__spark_list_get",
    params: &[Ptr, I64],
    ret: Some(Ptr),
};

/// `__spark_list_try_get(list: ptr, index: u64) -> ptr`
pub const LIST_TRY_GET: RuntimeFn = RuntimeFn {
    name: "__spark_list_try_get",
    params: &[Ptr, I64],
    ret: Some(Ptr),
};

/// `__spark_list_write_slot(list: ptr, index: u64, signed: i8) -> ptr`
pub const LIST_WRITE_SLOT: RuntimeFn = RuntimeFn {
    name: "__spark_list_write_slot",
    params: &[Ptr, I64, I8],
    ret: Some(Ptr),
};

/// `__spark_list_replace(list: ptr, slot: ptr, value: ptr) -> void`
pub const LIST_REPLACE: RuntimeFn = RuntimeFn {
    name: "__spark_list_replace",
    params: &[Ptr, Ptr, Ptr],
    ret: None,
};

/// `__spark_list_rc_dec(ptr) -> void`
pub const LIST_RC_DEC: RuntimeFn = RuntimeFn {
    name: "__spark_list_rc_dec",
    params: &[Ptr],
    ret: None,
};

/// `__spark_list_cow(ptr) -> ptr`
pub const LIST_COW: RuntimeFn = RuntimeFn {
    name: "__spark_list_cow",
    params: &[Ptr],
    ret: Some(Ptr),
};

/// `__spark_list_len(ptr) -> i64`
pub const LIST_LEN: RuntimeFn = RuntimeFn {
    name: "__spark_list_len",
    params: &[Ptr],
    ret: Some(I64),
};

/// `__spark_list_contains(ptr, ptr) -> i8`
pub const LIST_CONTAINS: RuntimeFn = RuntimeFn {
    name: "__spark_list_contains",
    params: &[Ptr, Ptr],
    ret: Some(I8),
};

/// `__spark_list_remove(ptr, i64) -> void`
pub const LIST_REMOVE: RuntimeFn = RuntimeFn {
    name: "__spark_list_remove",
    params: &[Ptr, I64],
    ret: None,
};

/// `__spark_list_sort(ptr, sort_kind: i64) -> ptr`
pub const LIST_SORT: RuntimeFn = RuntimeFn {
    name: "__spark_list_sort",
    params: &[Ptr, I64],
    ret: Some(Ptr),
};

/// `__spark_list_take(ptr, count: i64) -> ptr`
pub const LIST_TAKE: RuntimeFn = RuntimeFn {
    name: "__spark_list_take",
    params: &[Ptr, I64],
    ret: Some(Ptr),
};

/// `__spark_list_reverse(ptr) -> ptr`
pub const LIST_REVERSE: RuntimeFn = RuntimeFn {
    name: "__spark_list_reverse",
    params: &[Ptr],
    ret: Some(Ptr),
};

/// `__spark_list_from_range(start: i64, end: i64, inclusive: i8, step: i64) -> ptr`
pub const LIST_FROM_RANGE: RuntimeFn = RuntimeFn {
    name: "__spark_list_from_range",
    params: &[I64, I64, I8, I64],
    ret: Some(Ptr),
};

// ── Map ─────────────────────────────────────────────────────────────────

/// `__spark_map_new(key_desc: ptr, value_desc: ptr) -> ptr`
pub const MAP_NEW: RuntimeFn = RuntimeFn {
    name: "__spark_map_new",
    params: &[Ptr, Ptr],
    ret: Some(Ptr),
};

/// `__spark_map_insert(map: ptr, key_ptr: ptr, value_ptr: ptr) -> void`
pub const MAP_INSERT: RuntimeFn = RuntimeFn {
    name: "__spark_map_insert",
    params: &[Ptr, Ptr, Ptr],
    ret: None,
};

/// `__spark_invalid_collection_visitor() -> void`
pub const INVALID_COLLECTION_VISITOR: RuntimeFn = RuntimeFn {
    name: "__spark_invalid_collection_visitor",
    params: &[],
    ret: None,
};

/// `__spark_map_next(map: ptr, start: u64, key_out: ptr, value_out: ptr) -> u64`
pub const MAP_NEXT: RuntimeFn = RuntimeFn {
    name: "__spark_map_next",
    params: &[Ptr, I64, Ptr, Ptr],
    ret: Some(I64),
};

/// `__spark_map_get(map: ptr, key_ptr: ptr) -> ptr`
pub const MAP_GET: RuntimeFn = RuntimeFn {
    name: "__spark_map_get",
    params: &[Ptr, Ptr],
    ret: Some(Ptr),
};

/// `__spark_map_rc_dec(ptr) -> void`
pub const MAP_RC_DEC: RuntimeFn = RuntimeFn {
    name: "__spark_map_rc_dec",
    params: &[Ptr],
    ret: None,
};

/// `__spark_map_cow(ptr) -> ptr`
pub const MAP_COW: RuntimeFn = RuntimeFn {
    name: "__spark_map_cow",
    params: &[Ptr],
    ret: Some(Ptr),
};

/// `__spark_map_len(ptr) -> i64`
pub const MAP_LEN: RuntimeFn = RuntimeFn {
    name: "__spark_map_len",
    params: &[Ptr],
    ret: Some(I64),
};

/// `__spark_map_contains(ptr, ptr) -> i8`
pub const MAP_CONTAINS: RuntimeFn = RuntimeFn {
    name: "__spark_map_contains",
    params: &[Ptr, Ptr],
    ret: Some(I8),
};

/// `__spark_map_remove(ptr, ptr) -> void`
pub const MAP_REMOVE: RuntimeFn = RuntimeFn {
    name: "__spark_map_remove",
    params: &[Ptr, Ptr],
    ret: None,
};

// ── Set ─────────────────────────────────────────────────────────────────

/// `__spark_set_new(elem_desc: ptr) -> ptr`
pub const SET_NEW: RuntimeFn = RuntimeFn {
    name: "__spark_set_new",
    params: &[Ptr],
    ret: Some(Ptr),
};

/// `__spark_set_insert(set: ptr, elem_ptr: ptr) -> void`
pub const SET_INSERT: RuntimeFn = RuntimeFn {
    name: "__spark_set_insert",
    params: &[Ptr, Ptr],
    ret: None,
};

/// `__spark_set_rc_dec(ptr) -> void`
pub const SET_RC_DEC: RuntimeFn = RuntimeFn {
    name: "__spark_set_rc_dec",
    params: &[Ptr],
    ret: None,
};

/// `__spark_set_cow(ptr) -> ptr`
pub const SET_COW: RuntimeFn = RuntimeFn {
    name: "__spark_set_cow",
    params: &[Ptr],
    ret: Some(Ptr),
};

/// `__spark_set_len(ptr) -> i64`
pub const SET_LEN: RuntimeFn = RuntimeFn {
    name: "__spark_set_len",
    params: &[Ptr],
    ret: Some(I64),
};

/// `__spark_set_contains(ptr, ptr) -> i8`
pub const SET_CONTAINS: RuntimeFn = RuntimeFn {
    name: "__spark_set_contains",
    params: &[Ptr, Ptr],
    ret: Some(I8),
};

/// `__spark_set_remove(ptr, ptr) -> void`
pub const SET_REMOVE: RuntimeFn = RuntimeFn {
    name: "__spark_set_remove",
    params: &[Ptr, Ptr],
    ret: None,
};

// ── Reference Counting ──────────────────────────────────────────────────

/// `__spark_rc_release(ptr) -> void`
pub const RC_RELEASE: RuntimeFn = RuntimeFn {
    name: "__spark_rc_release",
    params: &[Ptr],
    ret: None,
};

/// `__spark_rc_inc(ptr) -> void`
pub const RC_INC: RuntimeFn = RuntimeFn {
    name: "__spark_rc_inc",
    params: &[Ptr],
    ret: None,
};

// ── Closure Environment ─────────────────────────────────────────────────

/// `__spark_env_alloc(size: i64) -> ptr`
pub const ENV_ALLOC: RuntimeFn = RuntimeFn {
    name: "__spark_env_alloc",
    params: &[I64],
    ret: Some(Ptr),
};

/// `__spark_env_rc_dec(ptr) -> void`
pub const ENV_RC_DEC: RuntimeFn = RuntimeFn {
    name: "__spark_env_rc_dec",
    params: &[Ptr],
    ret: None,
};

// ── Concurrency ─────────────────────────────────────────────────────────

/// `__spark_spawn(fn_ptr: ptr, env_ptr: ptr, result_size: i64, is_float: i8) -> ptr`
pub const SPAWN: RuntimeFn = RuntimeFn {
    name: "__spark_spawn",
    params: &[Ptr, Ptr, I64, I8],
    ret: Some(Ptr),
};

/// `__spark_await(future_ptr: ptr) -> ptr`
pub const AWAIT_FUTURE: RuntimeFn = RuntimeFn {
    name: "__spark_await",
    params: &[Ptr],
    ret: Some(Ptr),
};

/// `__spark_future_rc_dec(future_ptr: ptr) -> void`
pub const FUTURE_RC_DEC: RuntimeFn = RuntimeFn {
    name: "__spark_future_rc_dec",
    params: &[Ptr],
    ret: None,
};

/// `__spark_alloc_state(size: i64) -> ptr`
pub const ALLOC_STATE: RuntimeFn = RuntimeFn {
    name: "__spark_alloc_state",
    params: &[I64],
    ret: Some(Ptr),
};

/// `__spark_spawn_coop(poll_fn: ptr, state_ptr: ptr, state_size: i64, result_size: i64, result_offset: i64) -> ptr`
pub const SPAWN_COOP: RuntimeFn = RuntimeFn {
    name: "__spark_spawn_coop",
    params: &[Ptr, Ptr, I64, I64, I64],
    ret: Some(Ptr),
};

/// `__spark_try_poll(future_ptr: ptr) -> ptr`
pub const TRY_POLL: RuntimeFn = RuntimeFn {
    name: "__spark_try_poll",
    params: &[Ptr],
    ret: Some(Ptr),
};

// ── Channels ────────────────────────────────────────────────────────────

/// `__spark_channel_new(elem_size: i64) -> ptr`
pub const CHANNEL_NEW: RuntimeFn = RuntimeFn {
    name: "__spark_channel_new",
    params: &[I64],
    ret: Some(Ptr),
};

/// `__spark_channel_send(ch: ptr, value_ptr: ptr, elem_size: i64) -> void`
pub const CHANNEL_SEND: RuntimeFn = RuntimeFn {
    name: "__spark_channel_send",
    params: &[Ptr, Ptr, I64],
    ret: None,
};

/// `__spark_channel_recv(ch: ptr, elem_size: i64) -> ptr`
pub const CHANNEL_RECV: RuntimeFn = RuntimeFn {
    name: "__spark_channel_recv",
    params: &[Ptr, I64],
    ret: Some(Ptr),
};

/// `__spark_channel_try_recv(ch: ptr, elem_size: i64, out_ptr: ptr, out_size: i64) -> void`
pub const CHANNEL_TRY_RECV: RuntimeFn = RuntimeFn {
    name: "__spark_channel_try_recv",
    params: &[Ptr, I64, Ptr, I64],
    ret: None,
};

/// `__spark_channel_close(ch: ptr) -> void`
pub const CHANNEL_CLOSE: RuntimeFn = RuntimeFn {
    name: "__spark_channel_close",
    params: &[Ptr],
    ret: None,
};

/// `__spark_channel_rc_dec(ch: ptr) -> void`
pub const CHANNEL_RC_DEC: RuntimeFn = RuntimeFn {
    name: "__spark_channel_rc_dec",
    params: &[Ptr],
    ret: None,
};

// ── Range ───────────────────────────────────────────────────────────────

/// `__spark_range_len(start: i64, end: i64, inclusive: i8, step: i64) -> i64`
pub const RANGE_LEN: RuntimeFn = RuntimeFn {
    name: "__spark_range_len",
    params: &[I64, I64, I8, I64],
    ret: Some(I64),
};

// ── Parallel ────────────────────────────────────────────────────────────

/// `__spark_parallel_foreach(list_ptr: ptr, fn_ptr: ptr, env_ptr: ptr, elem_size: i64, cb_kind: i8) -> void`
pub const PARALLEL_FOREACH: RuntimeFn = RuntimeFn {
    name: "__spark_parallel_foreach",
    params: &[Ptr, Ptr, Ptr, I64, I8],
    ret: None,
};

/// `__spark_parallel_map(list_ptr: ptr, fn_ptr: ptr, env_ptr: ptr, elem_size: i64, result_elem_size: i64, cb_kind: i8) -> ptr`
pub const PARALLEL_MAP: RuntimeFn = RuntimeFn {
    name: "__spark_parallel_map",
    params: &[Ptr, Ptr, Ptr, I64, I64, I8],
    ret: Some(Ptr),
};

/// `__spark_parallel_map_reduce(list_ptr: ptr, map_fn: ptr, map_env: ptr, reduce_fn: ptr, reduce_env: ptr, initial: i64, elem_size: i64, map_cb_kind: i8, reduce_cb_kind: i8) -> i64`
pub const PARALLEL_MAP_REDUCE: RuntimeFn = RuntimeFn {
    name: "__spark_parallel_map_reduce",
    params: &[Ptr, Ptr, Ptr, Ptr, Ptr, I64, I64, I8, I8],
    ret: Some(I64),
};

/// `__spark_parallel_range_foreach(start: i64, end: i64, inclusive: i8, step: i64, fn_ptr: ptr, env_ptr: ptr, cb_kind: i8) -> void`
pub const PARALLEL_RANGE_FOREACH: RuntimeFn = RuntimeFn {
    name: "__spark_parallel_range_foreach",
    params: &[I64, I64, I8, I64, Ptr, Ptr, I8],
    ret: None,
};

/// `__spark_parallel_range_map(start: i64, end: i64, inclusive: i8, step: i64, fn_ptr: ptr, env_ptr: ptr, result_elem_size: i64, cb_kind: i8) -> ptr`
pub const PARALLEL_RANGE_MAP: RuntimeFn = RuntimeFn {
    name: "__spark_parallel_range_map",
    params: &[I64, I64, I8, I64, Ptr, Ptr, I64, I8],
    ret: Some(Ptr),
};

/// `__spark_parallel_range_map_reduce(start: i64, end: i64, inclusive: i8, step: i64, map_fn: ptr, map_env: ptr, reduce_fn: ptr, reduce_env: ptr, initial: i64, map_cb_kind: i8, reduce_cb_kind: i8) -> i64`
pub const PARALLEL_RANGE_MAP_REDUCE: RuntimeFn = RuntimeFn {
    name: "__spark_parallel_range_map_reduce",
    params: &[I64, I64, I8, I64, Ptr, Ptr, Ptr, Ptr, I64, I8, I8],
    ret: Some(I64),
};

// ── All Functions ───────────────────────────────────────────────────────

/// Complete list of all runtime functions, for validation and iteration.
pub const ALL_RUNTIME_FUNCTIONS: &[&RuntimeFn] = &[
    // String
    &STRING_NEW,
    &STRING_FROM_LITERAL,
    &STRING_RC_DEC,
    &STRING_COW,
    &STRING_LEN,
    &STRING_LINES,
    &STRING_SPLIT,
    &STRING_SPLIT_WHITESPACE,
    &STRING_TRIM,
    &STRING_SECTIONS,
    &STRING_CHARS,
    &STRING_CONCAT,
    &STRING_EQ,
    &HASH_STRING_ELEM,
    &EQ_STRING_ELEM,
    // Conversion
    &INT_TO_STRING,
    &FLOAT_TO_STRING,
    &BOOL_TO_STRING,
    // Print
    &PRINT_INT,
    &PRINT_STRING,
    &PRINT_BOOL,
    &PRINT_FLOAT,
    &VALUE_DESC_NEW,
    // List
    &LIST_NEW,
    &LIST_PUSH,
    &LIST_GET,
    &LIST_TRY_GET,
    &LIST_WRITE_SLOT,
    &LIST_REPLACE,
    &LIST_RC_DEC,
    &LIST_COW,
    &LIST_LEN,
    &LIST_CONTAINS,
    &LIST_REMOVE,
    &LIST_SORT,
    &LIST_TAKE,
    &LIST_REVERSE,
    &LIST_FROM_RANGE,
    // Map
    &MAP_NEW,
    &MAP_INSERT,
    &MAP_NEXT,
    &INVALID_COLLECTION_VISITOR,
    &MAP_GET,
    &MAP_RC_DEC,
    &MAP_COW,
    &MAP_LEN,
    &MAP_CONTAINS,
    &MAP_REMOVE,
    // Set
    &SET_NEW,
    &SET_INSERT,
    &SET_RC_DEC,
    &SET_COW,
    &SET_LEN,
    &SET_CONTAINS,
    &SET_REMOVE,
    // RC
    &RC_RELEASE,
    &RC_INC,
    // Env
    &ENV_ALLOC,
    &ENV_RC_DEC,
    // Concurrency
    &SPAWN,
    &AWAIT_FUTURE,
    &FUTURE_RC_DEC,
    &ALLOC_STATE,
    &SPAWN_COOP,
    &TRY_POLL,
    // Channels
    &CHANNEL_NEW,
    &CHANNEL_SEND,
    &CHANNEL_RECV,
    &CHANNEL_TRY_RECV,
    &CHANNEL_CLOSE,
    &CHANNEL_RC_DEC,
    // Range
    &RANGE_LEN,
    // Parallel
    &PARALLEL_FOREACH,
    &PARALLEL_MAP,
    &PARALLEL_MAP_REDUCE,
    &PARALLEL_RANGE_FOREACH,
    &PARALLEL_RANGE_MAP,
    &PARALLEL_RANGE_MAP_REDUCE,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_functions_have_unique_names() {
        let mut names: Vec<&str> = ALL_RUNTIME_FUNCTIONS.iter().map(|f| f.name).collect();
        names.sort_unstable();
        for pair in names.windows(2) {
            assert_ne!(pair[0], pair[1], "duplicate function name: {}", pair[0]);
        }
    }

    #[test]
    fn all_functions_have_spark_prefix() {
        for f in ALL_RUNTIME_FUNCTIONS {
            assert!(
                f.name.starts_with("__spark_"),
                "runtime function {0} missing __spark_ prefix",
                f.name
            );
        }
    }

    #[test]
    fn env_header_offsets_are_consistent() {
        assert_eq!(ENV_RC_OFFSET, 0);
        assert_eq!(ENV_ALLOC_SIZE_OFFSET, 8);
        assert_eq!(ENV_DTOR_OFFSET, 16);
        assert_eq!(ENV_HEADER_SIZE, 24);
        assert_eq!(ENV_CAPTURES_OFFSET, ENV_HEADER_SIZE);
    }
}
