//! Spark runtime library.
//!
//! Provides `extern "C"` functions that are called by compiled Spark programs.
//! This crate is compiled as a static library (`libspark_runtime.a`) and linked
//! into every Spark executable.
//!
//! All heap-allocated types (`SparkString`, `SparkList`, `SparkMap`) start with
//! a `rc: usize` field for reference counting. The generic `__spark_rc_inc`
//! function works on any heap object (rc is always at offset 0). Type-specific
//! `_rc_dec` functions handle deallocation when the count reaches zero.

// Safety: This crate is an FFI boundary; raw pointers and `#[no_mangle]`
// are part of the runtime ABI.
#![allow(unsafe_code)]
#![allow(clippy::missing_const_for_fn)] // extern "C" FFI stubs are not const

mod callback;
mod channel;
mod collections;
mod error;
mod future;
mod handles;
mod heap;
mod http;
mod io;
mod json;
mod math;
mod net;
mod parallel;
mod string;
mod sys;
mod time;
mod value;

pub use collections::{
    SparkList, SparkMap, SparkSet, __spark_list_contains, __spark_list_cow,
    __spark_list_from_range, __spark_list_get, __spark_list_len, __spark_list_new,
    __spark_list_push, __spark_list_rc_dec, __spark_list_remove, __spark_list_replace,
    __spark_list_reverse, __spark_list_sort, __spark_list_take, __spark_list_try_get,
    __spark_list_write_slot, __spark_map_contains, __spark_map_cow, __spark_map_get,
    __spark_map_insert, __spark_map_len, __spark_map_new, __spark_map_next, __spark_map_rc_dec,
    __spark_map_remove, __spark_set_contains, __spark_set_cow, __spark_set_insert, __spark_set_len,
    __spark_set_new, __spark_set_next, __spark_set_rc_dec, __spark_set_remove, SORT_TAG_BOOL,
    SORT_TAG_FLOAT, SORT_TAG_INT, SORT_TAG_STRING,
};
pub(crate) use future::tokio_runtime;
pub use future::{
    __spark_alloc_state, __spark_await, __spark_future_rc_dec, __spark_spawn, __spark_spawn_coop,
    __spark_try_poll, __spark_wake_current,
};
pub use heap::{__spark_env_alloc, __spark_env_rc_dec, __spark_rc_inc, __spark_rc_release};
pub use parallel::{
    __spark_parallel_foreach, __spark_parallel_map, __spark_parallel_map_reduce,
    __spark_parallel_range_foreach, __spark_parallel_range_map, __spark_parallel_range_map_reduce,
    __spark_range_len,
};
use value::{fix_hash, fnv1a_hash, libc_memcmp};

// =============================================================================
// Print functions
// =============================================================================

/// Prints an integer value followed by a newline.
#[no_mangle]
pub extern "C" fn __spark_print_int(value: i64) {
    println!("{value}");
}

/// Prints a boolean value (`true` or `false`) followed by a newline.
#[no_mangle]
pub extern "C" fn __spark_print_bool(value: i8) {
    if value != 0 {
        println!("true");
    } else {
        println!("false");
    }
}

/// Prints a float value followed by a newline.
#[no_mangle]
pub extern "C" fn __spark_print_float(value: f64) {
    println!("{value}");
}

/// Prints a string value followed by a newline.
///
/// # Safety
///
/// `ptr` must be a valid pointer returned by `__spark_string_new` or
/// `__spark_string_from_literal`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_print_string(ptr: *mut u8) {
    let s = unsafe { &*(ptr.cast::<SparkString>()) };
    if s.len == 0 {
        println!();
        return;
    }
    let slice = unsafe { std::slice::from_raw_parts(s.data, s.len) };
    let text = std::str::from_utf8(slice).unwrap_or("<invalid utf-8>");
    println!("{text}");
}

/// Prints an empty newline.
#[no_mangle]
pub extern "C" fn __spark_println() {
    println!();
}

// =============================================================================
// String — heap-allocated UTF-8 string with reference counting
// =============================================================================

/// A heap-allocated UTF-8 string with a reference count.
///
/// The `rc` field is always at offset 0, allowing generic RC operations.
#[repr(C)]
struct SparkString {
    /// Reference count.
    rc: usize,
    /// Pointer to the UTF-8 byte buffer.
    data: *mut u8,
    /// Number of valid bytes.
    len: usize,
    /// Allocated capacity in bytes.
    cap: usize,
}

/// Creates a new empty heap-allocated string (rc = 1).
#[no_mangle]
pub extern "C" fn __spark_string_new() -> *mut u8 {
    let s = Box::new(SparkString {
        rc: 1,
        data: std::ptr::null_mut(),
        len: 0,
        cap: 0,
    });
    Box::into_raw(s).cast::<u8>()
}

/// Creates a heap-allocated string from a UTF-8 literal (pointer + length).
/// The returned string has rc = 1.
///
/// # Safety
///
/// `data` must be a valid pointer to `len` bytes of UTF-8 data.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_string_from_literal(data: *const u8, len: i64) -> *mut u8 {
    let byte_len = len as usize;
    let align = std::mem::align_of::<u64>();

    if byte_len == 0 {
        return __spark_string_new();
    }

    let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(byte_len, align) };
    let buf = unsafe { std::alloc::alloc(layout) };
    if buf.is_null() {
        eprintln!("spark: out of memory allocating string");
        std::process::abort();
    }
    unsafe { std::ptr::copy_nonoverlapping(data, buf, byte_len) };

    let s = Box::new(SparkString {
        rc: 1,
        data: buf,
        len: byte_len,
        cap: byte_len,
    });
    Box::into_raw(s).cast::<u8>()
}

/// Returns the length in bytes of a string.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_string_new` or
/// `__spark_string_from_literal`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_wrap)]
pub unsafe extern "C" fn __spark_string_len(ptr: *mut u8) -> i64 {
    let s = unsafe { &*(ptr.cast::<SparkString>()) };
    s.len as i64
}

/// Compares two strings for content equality.
///
/// Returns `1` if the strings have the same length and identical bytes,
/// `0` otherwise.
///
/// # Safety
///
/// Both `a` and `b` must be valid pointers from `__spark_string_new` or
/// `__spark_string_from_literal`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_string_eq(a: *mut u8, b: *mut u8) -> i8 {
    let sa = unsafe { &*(a.cast::<SparkString>()) };
    let sb = unsafe { &*(b.cast::<SparkString>()) };
    if sa.len != sb.len {
        return 0;
    }
    if sa.len == 0 {
        return 1;
    }
    let slice_a = unsafe { std::slice::from_raw_parts(sa.data, sa.len) };
    let slice_b = unsafe { std::slice::from_raw_parts(sb.data, sb.len) };
    i8::from(slice_a == slice_b)
}

// =============================================================================
// String interpolation — concat and to_string conversions
// =============================================================================

/// Concatenates two strings, returning a new string (rc = 1).
///
/// Does NOT consume `a` or `b` — the caller manages their reference counts.
///
/// # Safety
///
/// Both pointers must be valid `SparkString` pointers.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_string_concat(a: *mut u8, b: *mut u8) -> *mut u8 {
    let sa = unsafe { &*(a.cast::<SparkString>()) };
    let sb = unsafe { &*(b.cast::<SparkString>()) };

    let new_len = sa.len + sb.len;

    if new_len == 0 {
        return __spark_string_new();
    }

    let align = std::mem::align_of::<u64>();
    let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(new_len, align) };
    let buf = unsafe { std::alloc::alloc(layout) };
    if buf.is_null() {
        eprintln!("spark: out of memory allocating string");
        std::process::abort();
    }

    if sa.len > 0 {
        unsafe { std::ptr::copy_nonoverlapping(sa.data, buf, sa.len) };
    }
    if sb.len > 0 {
        unsafe { std::ptr::copy_nonoverlapping(sb.data, buf.add(sa.len), sb.len) };
    }

    let s = Box::new(SparkString {
        rc: 1,
        data: buf,
        len: new_len,
        cap: new_len,
    });
    Box::into_raw(s).cast::<u8>()
}

/// Converts an integer to a string (rc = 1).
#[no_mangle]
#[allow(clippy::cast_possible_wrap)] // String lengths won't exceed i64::MAX
pub extern "C" fn __spark_int_to_string(val: i64) -> *mut u8 {
    let text = val.to_string();
    let bytes = text.as_bytes();
    unsafe { __spark_string_from_literal(bytes.as_ptr(), bytes.len() as i64) }
}

/// Converts a float to a string (rc = 1).
#[no_mangle]
#[allow(clippy::cast_possible_wrap)] // String lengths won't exceed i64::MAX
pub extern "C" fn __spark_float_to_string(val: f64) -> *mut u8 {
    let text = val.to_string();
    let bytes = text.as_bytes();
    unsafe { __spark_string_from_literal(bytes.as_ptr(), bytes.len() as i64) }
}

/// Converts a boolean to a string (rc = 1).
///
/// Returns `"true"` for non-zero, `"false"` for zero.
#[no_mangle]
#[allow(clippy::cast_possible_wrap)] // String lengths won't exceed i64::MAX
pub extern "C" fn __spark_bool_to_string(val: i8) -> *mut u8 {
    let text = if val != 0 { "true" } else { "false" };
    let bytes = text.as_bytes();
    unsafe { __spark_string_from_literal(bytes.as_ptr(), bytes.len() as i64) }
}

/// Hashes a set/map element containing a `SparkString*` by string contents.
///
/// The input points to an element slot whose bytes are a `*mut u8` string pointer.
///
/// # Safety
///
/// `elem_ptr` must point to at least `size_of::<*mut u8>()` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_hash_string_elem(elem_ptr: *const u8) -> u64 {
    let str_ptr = unsafe { *(elem_ptr.cast::<*mut u8>()) };
    if str_ptr.is_null() {
        return fix_hash(unsafe { fnv1a_hash(std::ptr::null(), 0) });
    }
    let s = unsafe { &*(str_ptr.cast::<SparkString>()) };
    fix_hash(unsafe { fnv1a_hash(s.data, s.len) })
}

/// Compares two set/map elements containing `SparkString*` values by contents.
///
/// The inputs point to element/key slots whose bytes are `*mut u8` string pointers.
///
/// # Safety
///
/// `a` and `b` must each point to at least `size_of::<*mut u8>()` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_eq_string_elem(a: *const u8, b: *const u8) -> i8 {
    let pa = unsafe { *(a.cast::<*mut u8>()) };
    let pb = unsafe { *(b.cast::<*mut u8>()) };
    if pa == pb {
        return 1;
    }
    if pa.is_null() || pb.is_null() {
        return 0;
    }
    let sa = unsafe { &*(pa.cast::<SparkString>()) };
    let sb = unsafe { &*(pb.cast::<SparkString>()) };
    if sa.len != sb.len {
        return 0;
    }
    i8::from(unsafe { libc_memcmp(sa.data, sb.data, sa.len) })
}

// =============================================================================
// Reference counting
// =============================================================================

/// Decrements the reference count of a `SparkString`.
/// Frees the string when the count reaches zero.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_string_new` or
/// `__spark_string_from_literal`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_string_rc_dec(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let s = unsafe { &mut *(ptr.cast::<SparkString>()) };
    s.rc -= 1;
    if s.rc == 0 {
        // Free the data buffer.
        if !s.data.is_null() && s.cap > 0 {
            let align = std::mem::align_of::<u64>();
            let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(s.cap, align) };
            unsafe { std::alloc::dealloc(s.data, layout) };
        }
        // Free the SparkString struct itself.
        drop(unsafe { Box::from_raw(ptr.cast::<SparkString>()) });
    }
}

// =============================================================================
// COW (Copy-on-Write) helpers — prepared for mutation operations
// =============================================================================

/// Returns the pointer unchanged if rc == 1 (unique owner).
/// If rc > 1, copies the string buffer, decrements the old rc, and returns
/// a new `SparkString` with rc = 1.
///
/// # Safety
///
/// `ptr` must be a valid `SparkString` pointer.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_string_cow(ptr: *mut u8) -> *mut u8 {
    if ptr.is_null() {
        return ptr;
    }
    let s = unsafe { &*(ptr.cast::<SparkString>()) };
    if s.rc == 1 {
        return ptr;
    }
    // Clone the buffer.
    let new_ptr = if s.len > 0 && !s.data.is_null() {
        let align = std::mem::align_of::<u64>();
        let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(s.len, align) };
        let buf = unsafe { std::alloc::alloc(layout) };
        if buf.is_null() {
            eprintln!("spark: out of memory in string COW copy");
            std::process::abort();
        }
        unsafe { std::ptr::copy_nonoverlapping(s.data, buf, s.len) };
        buf
    } else {
        std::ptr::null_mut()
    };
    let new = Box::new(SparkString {
        rc: 1,
        data: new_ptr,
        len: s.len,
        cap: s.len,
    });
    // Decrement old rc.
    let old = unsafe { &mut *(ptr.cast::<SparkString>()) };
    old.rc -= 1;
    Box::into_raw(new).cast::<u8>()
}

#[cfg(test)]
#[allow(
    clippy::cast_ptr_alignment,
    clippy::cast_possible_wrap,
    clippy::approx_constant
)]
mod tests {
    use super::*;
    use crate::value::__spark_value_desc_new;

    fn test_desc(size: usize, drop_fn: usize, hash_fn: usize, eq_fn: usize) -> *mut u8 {
        test_desc_with_retain(size, drop_fn, 0, hash_fn, eq_fn)
    }

    fn test_desc_with_retain(
        size: usize,
        drop_fn: usize,
        retain_fn: usize,
        hash_fn: usize,
        eq_fn: usize,
    ) -> *mut u8 {
        __spark_value_desc_new(
            size as i64,
            std::mem::align_of::<u64>() as i64,
            drop_fn,
            retain_fn,
            hash_fn,
            eq_fn,
        )
    }

    fn test_list_new(size: usize, drop_fn: usize, eq_fn: usize) -> *mut u8 {
        __spark_list_new(test_desc(size, drop_fn, 0, eq_fn))
    }

    fn test_map_new(
        key_size: usize,
        value_size: usize,
        key_drop: usize,
        value_drop: usize,
        key_hash: usize,
        key_eq: usize,
    ) -> *mut u8 {
        __spark_map_new(
            test_desc(key_size, key_drop, key_hash, key_eq),
            test_desc(value_size, value_drop, 0, 0),
        )
    }

    fn test_set_new(size: usize, drop_fn: usize, hash_fn: usize, eq_fn: usize) -> *mut u8 {
        __spark_set_new(test_desc(size, drop_fn, hash_fn, eq_fn))
    }

    /// Helper: read the string content from a `SparkString` pointer.
    unsafe fn read_spark_string(ptr: *mut u8) -> String {
        let s = unsafe { &*(ptr.cast::<SparkString>()) };
        if s.len == 0 {
            return String::new();
        }
        let slice = unsafe { std::slice::from_raw_parts(s.data, s.len) };
        std::str::from_utf8(slice).unwrap().to_owned()
    }

    #[test]
    fn string_concat_basic() {
        unsafe {
            let a = __spark_string_from_literal("hello ".as_ptr(), 6);
            let b = __spark_string_from_literal("world".as_ptr(), 5);
            let result = __spark_string_concat(a, b);
            assert_eq!(read_spark_string(result), "hello world");
            __spark_string_rc_dec(a);
            __spark_string_rc_dec(b);
            __spark_string_rc_dec(result);
        }
    }

    #[test]
    fn string_concat_empty() {
        unsafe {
            let a = __spark_string_new();
            let b = __spark_string_from_literal("text".as_ptr(), 4);
            let result = __spark_string_concat(a, b);
            assert_eq!(read_spark_string(result), "text");
            __spark_string_rc_dec(a);
            __spark_string_rc_dec(b);
            __spark_string_rc_dec(result);
        }
    }

    #[test]
    fn string_concat_both_empty() {
        unsafe {
            let a = __spark_string_new();
            let b = __spark_string_new();
            let result = __spark_string_concat(a, b);
            assert_eq!(read_spark_string(result), "");
            __spark_string_rc_dec(a);
            __spark_string_rc_dec(b);
            __spark_string_rc_dec(result);
        }
    }

    #[test]
    fn int_to_string() {
        unsafe {
            let result = __spark_int_to_string(42);
            assert_eq!(read_spark_string(result), "42");
            __spark_string_rc_dec(result);

            let neg = __spark_int_to_string(-7);
            assert_eq!(read_spark_string(neg), "-7");
            __spark_string_rc_dec(neg);

            let zero = __spark_int_to_string(0);
            assert_eq!(read_spark_string(zero), "0");
            __spark_string_rc_dec(zero);
        }
    }

    #[test]
    fn float_to_string() {
        unsafe {
            let result = __spark_float_to_string(3.14);
            let text = read_spark_string(result);
            assert!(text.starts_with("3.14"), "expected 3.14..., got {text}");
            __spark_string_rc_dec(result);
        }
    }

    #[test]
    fn bool_to_string() {
        unsafe {
            let t = __spark_bool_to_string(1);
            assert_eq!(read_spark_string(t), "true");
            __spark_string_rc_dec(t);

            let f = __spark_bool_to_string(0);
            assert_eq!(read_spark_string(f), "false");
            __spark_string_rc_dec(f);
        }
    }

    #[test]
    fn string_eq_same_content() {
        unsafe {
            let a = __spark_string_from_literal("hello".as_ptr(), 5);
            let b = __spark_string_from_literal("hello".as_ptr(), 5);
            assert_eq!(__spark_string_eq(a, b), 1);
            __spark_string_rc_dec(a);
            __spark_string_rc_dec(b);
        }
    }

    #[test]
    fn string_eq_different_content() {
        unsafe {
            let a = __spark_string_from_literal("hello".as_ptr(), 5);
            let b = __spark_string_from_literal("world".as_ptr(), 5);
            assert_eq!(__spark_string_eq(a, b), 0);
            __spark_string_rc_dec(a);
            __spark_string_rc_dec(b);
        }
    }

    #[test]
    fn string_eq_different_lengths() {
        unsafe {
            let a = __spark_string_from_literal("hi".as_ptr(), 2);
            let b = __spark_string_from_literal("hello".as_ptr(), 5);
            assert_eq!(__spark_string_eq(a, b), 0);
            __spark_string_rc_dec(a);
            __spark_string_rc_dec(b);
        }
    }

    #[test]
    fn string_eq_both_empty() {
        unsafe {
            let a = __spark_string_new();
            let b = __spark_string_new();
            assert_eq!(__spark_string_eq(a, b), 1);
            __spark_string_rc_dec(a);
            __spark_string_rc_dec(b);
        }
    }

    #[test]
    fn env_alloc_zero_returns_null() {
        let ptr = __spark_env_alloc(0);
        assert!(ptr.is_null());
    }

    #[test]
    fn env_alloc_has_rc_header() {
        unsafe {
            let ptr = __spark_env_alloc(24);
            assert!(!ptr.is_null());
            // rc should be 1
            let rc = *(ptr.cast::<usize>());
            assert_eq!(rc, 1);
            // alloc_size should be 24 (header) + 24 (payload) = 48
            let alloc_size = *(ptr.add(8).cast::<usize>());
            assert_eq!(alloc_size, 48);
            // dtor slot at offset 16 should be null (zero-initialized)
            let dtor = *(ptr.add(16).cast::<usize>());
            assert_eq!(dtor, 0);
            // Free it
            __spark_env_rc_dec(ptr);
        }
    }

    #[test]
    fn env_rc_inc_dec_lifecycle() {
        unsafe {
            let ptr = __spark_env_alloc(8);
            assert_eq!(*(ptr.cast::<usize>()), 1);

            // Increment RC
            __spark_rc_inc(ptr);
            assert_eq!(*(ptr.cast::<usize>()), 2);

            // First dec — should not free
            __spark_env_rc_dec(ptr);
            assert_eq!(*(ptr.cast::<usize>()), 1);

            // Second dec — frees (no crash = success)
            __spark_env_rc_dec(ptr);
        }
    }

    #[test]
    fn env_rc_dec_null_is_noop() {
        unsafe {
            __spark_env_rc_dec(std::ptr::null_mut());
        }
    }

    #[test]
    fn env_dtor_called_on_free() {
        use std::sync::atomic::{AtomicBool, Ordering};

        static DTOR_CALLED: AtomicBool = AtomicBool::new(false);

        unsafe extern "C" fn test_dtor(_env: *mut u8) {
            DTOR_CALLED.store(true, Ordering::SeqCst);
        }

        unsafe {
            DTOR_CALLED.store(false, Ordering::SeqCst);
            let ptr = __spark_env_alloc(8);
            // Set dtor fn ptr at offset 16.
            *(ptr.add(16).cast::<usize>()) = test_dtor as *const () as usize;
            assert!(!DTOR_CALLED.load(Ordering::SeqCst));
            // Dropping to zero should call the destructor.
            __spark_env_rc_dec(ptr);
            assert!(DTOR_CALLED.load(Ordering::SeqCst));
        }
    }

    /// Helper: read elements from a `SparkList` of i64.
    unsafe fn read_list_i64(ptr: *mut u8) -> Vec<i64> {
        let list = unsafe { &*(ptr.cast::<SparkList>()) };
        let mut result = Vec::with_capacity(list.len);
        for i in 0..list.len {
            let elem = unsafe { list.data.add(i * list.elem_size()).cast::<i64>() };
            result.push(unsafe { *elem });
        }
        result
    }

    #[test]
    fn checked_list_get_returns_a_slot_or_null() {
        unsafe {
            let list = test_list_new(std::mem::size_of::<i64>(), 0, 0);
            assert!(__spark_list_try_get(list, 0).is_null());

            let value = 42_i64;
            __spark_list_push(list, std::ptr::from_ref(&value).cast());
            assert_eq!(__spark_list_try_get(list, 0), __spark_list_get(list, 0));
            assert!(__spark_list_try_get(list, 1).is_null());
            assert!(__spark_list_try_get(list, u64::MAX).is_null());
            __spark_list_rc_dec(list);
        }
    }

    #[test]
    fn list_range_exclusive() {
        unsafe {
            let ptr = __spark_list_from_range(0, 5, 0, 1);
            assert_eq!(read_list_i64(ptr), vec![0, 1, 2, 3, 4]);
            __spark_list_rc_dec(ptr);
        }
    }

    #[test]
    fn list_range_inclusive() {
        unsafe {
            let ptr = __spark_list_from_range(0, 5, 1, 1);
            assert_eq!(read_list_i64(ptr), vec![0, 1, 2, 3, 4, 5]);
            __spark_list_rc_dec(ptr);
        }
    }

    #[test]
    fn list_range_stepped() {
        unsafe {
            let ptr = __spark_list_from_range(0, 10, 0, 2);
            assert_eq!(read_list_i64(ptr), vec![0, 2, 4, 6, 8]);
            __spark_list_rc_dec(ptr);
        }
    }

    #[test]
    fn list_range_negative_step() {
        unsafe {
            let ptr = __spark_list_from_range(10, 0, 0, -2);
            assert_eq!(read_list_i64(ptr), vec![10, 8, 6, 4, 2]);
            __spark_list_rc_dec(ptr);
        }
    }

    #[test]
    fn list_range_empty() {
        unsafe {
            // start >= end with positive step → empty
            let ptr = __spark_list_from_range(5, 3, 0, 1);
            assert_eq!(read_list_i64(ptr), Vec::<i64>::new());
            __spark_list_rc_dec(ptr);
        }
    }

    #[test]
    fn list_range_zero_step_treated_as_one() {
        unsafe {
            let ptr = __spark_list_from_range(0, 3, 0, 0);
            assert_eq!(read_list_i64(ptr), vec![0, 1, 2]);
            __spark_list_rc_dec(ptr);
        }
    }

    // ========================================================================
    // Sort tests
    // ========================================================================

    /// Helper: push i64 values into a list.
    unsafe fn push_i64_list(values: &[i64]) -> *mut u8 {
        let list = test_list_new(8, 0, 0);
        for val in values {
            let ptr = std::ptr::from_ref::<i64>(val).cast::<u8>();
            unsafe { __spark_list_push(list, ptr) };
        }
        list
    }

    /// Helper: read f64 elements from a `SparkList`.
    unsafe fn read_list_f64(ptr: *mut u8) -> Vec<f64> {
        let list = unsafe { &*(ptr.cast::<SparkList>()) };
        let mut result = Vec::with_capacity(list.len);
        for i in 0..list.len {
            let elem = unsafe { list.data.add(i * list.elem_size()).cast::<f64>() };
            result.push(unsafe { *elem });
        }
        result
    }

    #[test]
    fn sort_int_basic() {
        unsafe {
            let list = push_i64_list(&[3, 1, 2]);
            let sorted = __spark_list_sort(list, SORT_TAG_INT);
            assert_eq!(read_list_i64(sorted), vec![1, 2, 3]);
            __spark_list_rc_dec(list);
            __spark_list_rc_dec(sorted);
        }
    }

    #[test]
    fn sort_int_empty() {
        unsafe {
            let list = push_i64_list(&[]);
            let sorted = __spark_list_sort(list, SORT_TAG_INT);
            assert_eq!(read_list_i64(sorted), Vec::<i64>::new());
            __spark_list_rc_dec(list);
            __spark_list_rc_dec(sorted);
        }
    }

    #[test]
    fn sort_float_with_negatives() {
        unsafe {
            let values: Vec<f64> = vec![3.5, -1.2, 0.0, 2.7, -4.0];
            let list = test_list_new(8, 0, 0);
            for val in &values {
                let ptr = std::ptr::from_ref::<f64>(val).cast::<u8>();
                __spark_list_push(list, ptr);
            }
            let sorted = __spark_list_sort(list, SORT_TAG_FLOAT);
            assert_eq!(read_list_f64(sorted), vec![-4.0, -1.2, 0.0, 2.7, 3.5]);
            __spark_list_rc_dec(list);
            __spark_list_rc_dec(sorted);
        }
    }

    #[test]
    fn sort_float_empty() {
        unsafe {
            let list = test_list_new(8, 0, 0);
            let sorted = __spark_list_sort(list, SORT_TAG_FLOAT);
            assert_eq!(read_list_f64(sorted), Vec::<f64>::new());
            __spark_list_rc_dec(list);
            __spark_list_rc_dec(sorted);
        }
    }

    #[test]
    fn sort_string_lexicographic() {
        unsafe {
            let list = test_list_new(std::mem::size_of::<*mut u8>(), 0, 0);
            let cherry = __spark_string_from_literal("cherry".as_ptr(), 6);
            let apple = __spark_string_from_literal("apple".as_ptr(), 5);
            let banana = __spark_string_from_literal("banana".as_ptr(), 6);
            __spark_list_push(list, std::ptr::addr_of!(cherry).cast::<u8>());
            __spark_list_push(list, std::ptr::addr_of!(apple).cast::<u8>());
            __spark_list_push(list, std::ptr::addr_of!(banana).cast::<u8>());

            let sorted = __spark_list_sort(list, SORT_TAG_STRING);
            let sorted_list = &*(sorted.cast::<SparkList>());
            assert_eq!(sorted_list.len, 3);

            let s0 = *(sorted_list.data.cast::<*mut u8>());
            let s1 = *(sorted_list
                .data
                .add(sorted_list.elem_size())
                .cast::<*mut u8>());
            let s2 = *(sorted_list
                .data
                .add(2 * sorted_list.elem_size())
                .cast::<*mut u8>());
            assert_eq!(read_spark_string(s0), "apple");
            assert_eq!(read_spark_string(s1), "banana");
            assert_eq!(read_spark_string(s2), "cherry");

            __spark_list_rc_dec(sorted);
            __spark_list_rc_dec(list);
            __spark_string_rc_dec(cherry);
            __spark_string_rc_dec(apple);
            __spark_string_rc_dec(banana);
        }
    }

    #[test]
    fn sort_bool() {
        unsafe {
            let list = test_list_new(1, 0, 0);
            let vals: [i8; 4] = [1, 0, 1, 0];
            for val in &vals {
                let ptr = std::ptr::from_ref::<i8>(val).cast::<u8>();
                __spark_list_push(list, ptr);
            }
            let sorted = __spark_list_sort(list, SORT_TAG_BOOL);
            let sorted_list = &*(sorted.cast::<SparkList>());
            assert_eq!(sorted_list.len, 4);
            let mut result = Vec::new();
            for i in 0..4 {
                result.push(
                    *(sorted_list
                        .data
                        .add(i * sorted_list.elem_size())
                        .cast::<i8>()),
                );
            }
            assert_eq!(result, vec![0, 0, 1, 1]);
            __spark_list_rc_dec(list);
            __spark_list_rc_dec(sorted);
        }
    }

    #[test]
    fn list_contains_string_value_semantics_with_callback() {
        unsafe {
            let list = string::string_list_from_iter(std::iter::once("hello"));
            let s2 = __spark_string_from_literal("hello".as_ptr(), 5);

            assert_eq!(
                __spark_list_contains(list, std::ptr::addr_of!(s2).cast::<u8>()),
                1
            );

            __spark_string_rc_dec(s2);
            __spark_list_rc_dec(list);
        }
    }

    #[test]
    fn list_cow_uses_retain_descriptor() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static RETAINS: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn count_retain(_slot: *mut u8) {
            RETAINS.fetch_add(1, Ordering::SeqCst);
        }

        unsafe {
            RETAINS.store(0, Ordering::SeqCst);
            let list = __spark_list_new(test_desc_with_retain(
                8,
                0,
                count_retain as *const () as usize,
                0,
                0,
            ));
            let a: i64 = 1;
            let b: i64 = 2;
            __spark_list_push(list, (&raw const a).cast::<u8>());
            __spark_list_push(list, (&raw const b).cast::<u8>());

            __spark_rc_inc(list);
            let cow = __spark_list_cow(list);

            assert_ne!(cow, list);
            assert_eq!(RETAINS.load(Ordering::SeqCst), 2);

            __spark_list_rc_dec(list);
            __spark_list_rc_dec(cow);
        }
    }

    #[test]
    fn list_take_uses_retain_descriptor() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static RETAINS: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn count_retain(_slot: *mut u8) {
            RETAINS.fetch_add(1, Ordering::SeqCst);
        }

        unsafe {
            RETAINS.store(0, Ordering::SeqCst);
            let list = __spark_list_new(test_desc_with_retain(
                8,
                0,
                count_retain as *const () as usize,
                0,
                0,
            ));
            for value in [1_i64, 2, 3] {
                __spark_list_push(list, (&raw const value).cast::<u8>());
            }

            let taken = __spark_list_take(list, 2);

            assert_eq!(read_list_i64(taken), vec![1, 2]);
            assert_eq!(RETAINS.load(Ordering::SeqCst), 2);

            __spark_list_rc_dec(list);
            __spark_list_rc_dec(taken);
        }
    }

    // ========================================================================
    // Hash infrastructure tests
    // ========================================================================

    #[test]
    fn fnv1a_deterministic() {
        unsafe {
            let data = b"hello";
            let h1 = fnv1a_hash(data.as_ptr(), data.len());
            let h2 = fnv1a_hash(data.as_ptr(), data.len());
            assert_eq!(h1, h2);
        }
    }

    #[test]
    fn fnv1a_different_inputs_differ() {
        unsafe {
            let h1 = fnv1a_hash(b"hello".as_ptr(), 5);
            let h2 = fnv1a_hash(b"world".as_ptr(), 5);
            assert_ne!(h1, h2);
        }
    }

    #[test]
    fn fix_hash_avoids_sentinels() {
        assert_eq!(fix_hash(0), 2);
        assert_eq!(fix_hash(1), 2);
        assert_eq!(fix_hash(2), 2);
        assert_eq!(fix_hash(3), 3);
        assert_eq!(fix_hash(u64::MAX), u64::MAX);
    }

    // ========================================================================
    // Map tests
    // ========================================================================

    #[test]
    fn map_insert_and_get() {
        unsafe {
            let m = test_map_new(8, 8, 0, 0, 0, 0);
            let key: i64 = 42;
            let val: i64 = 100;
            __spark_map_insert(
                m,
                (&raw const key).cast::<u8>(),
                (&raw const val).cast::<u8>(),
            );

            let result = __spark_map_get(m, (&raw const key).cast::<u8>());
            assert!(!result.is_null());
            assert_eq!(*(result.cast::<i64>()), 100);

            __spark_map_rc_dec(m);
        }
    }

    #[test]
    fn map_overwrite() {
        unsafe {
            let m = test_map_new(8, 8, 0, 0, 0, 0);
            let key: i64 = 1;
            let v1: i64 = 10;
            let v2: i64 = 20;
            __spark_map_insert(
                m,
                (&raw const key).cast::<u8>(),
                (&raw const v1).cast::<u8>(),
            );
            __spark_map_insert(
                m,
                (&raw const key).cast::<u8>(),
                (&raw const v2).cast::<u8>(),
            );

            assert_eq!(__spark_map_len(m), 1);
            let result = __spark_map_get(m, (&raw const key).cast::<u8>());
            assert_eq!(*(result.cast::<i64>()), 20);

            __spark_map_rc_dec(m);
        }
    }

    #[test]
    fn map_overwrite_drops_old_value() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static DROPS: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn count_drop(_slot: *mut u8) {
            DROPS.fetch_add(1, Ordering::SeqCst);
        }

        unsafe {
            DROPS.store(0, Ordering::SeqCst);
            let m = test_map_new(8, 8, 0, count_drop as *const () as usize, 0, 0);
            let key: i64 = 1;
            let v1: i64 = 10;
            let v2: i64 = 20;

            __spark_map_insert(
                m,
                (&raw const key).cast::<u8>(),
                (&raw const v1).cast::<u8>(),
            );
            __spark_map_insert(
                m,
                (&raw const key).cast::<u8>(),
                (&raw const v2).cast::<u8>(),
            );

            assert_eq!(DROPS.load(Ordering::SeqCst), 1);
            __spark_map_rc_dec(m);
            assert_eq!(DROPS.load(Ordering::SeqCst), 2);
        }
    }

    #[test]
    fn map_duplicate_insert_drops_unused_key() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static DROPS: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn count_drop(_slot: *mut u8) {
            DROPS.fetch_add(1, Ordering::SeqCst);
        }

        unsafe {
            DROPS.store(0, Ordering::SeqCst);
            let m = test_map_new(8, 8, count_drop as *const () as usize, 0, 0, 0);
            let key: i64 = 1;
            let v1: i64 = 10;
            let v2: i64 = 20;

            __spark_map_insert(
                m,
                (&raw const key).cast::<u8>(),
                (&raw const v1).cast::<u8>(),
            );
            __spark_map_insert(
                m,
                (&raw const key).cast::<u8>(),
                (&raw const v2).cast::<u8>(),
            );

            assert_eq!(DROPS.load(Ordering::SeqCst), 1);
            __spark_map_rc_dec(m);
            assert_eq!(DROPS.load(Ordering::SeqCst), 2);
        }
    }

    #[test]
    fn map_get_missing() {
        unsafe {
            let m = test_map_new(8, 8, 0, 0, 0, 0);
            let key: i64 = 99;
            let result = __spark_map_get(m, (&raw const key).cast::<u8>());
            assert!(result.is_null());

            __spark_map_rc_dec(m);
        }
    }

    #[test]
    fn map_contains() {
        unsafe {
            let m = test_map_new(8, 8, 0, 0, 0, 0);
            let key: i64 = 5;
            let val: i64 = 50;
            assert_eq!(__spark_map_contains(m, (&raw const key).cast::<u8>()), 0);

            __spark_map_insert(
                m,
                (&raw const key).cast::<u8>(),
                (&raw const val).cast::<u8>(),
            );
            assert_eq!(__spark_map_contains(m, (&raw const key).cast::<u8>()), 1);

            __spark_map_rc_dec(m);
        }
    }

    #[test]
    fn map_remove() {
        unsafe {
            let m = test_map_new(8, 8, 0, 0, 0, 0);
            let key: i64 = 7;
            let val: i64 = 77;
            __spark_map_insert(
                m,
                (&raw const key).cast::<u8>(),
                (&raw const val).cast::<u8>(),
            );
            assert_eq!(__spark_map_len(m), 1);

            __spark_map_remove(m, (&raw const key).cast::<u8>());
            assert_eq!(__spark_map_len(m), 0);
            assert!(__spark_map_get(m, (&raw const key).cast::<u8>()).is_null());

            __spark_map_rc_dec(m);
        }
    }

    #[test]
    fn map_remove_and_reinsert() {
        unsafe {
            let m = test_map_new(8, 8, 0, 0, 0, 0);
            let key: i64 = 3;
            let v1: i64 = 30;
            let v2: i64 = 31;
            __spark_map_insert(
                m,
                (&raw const key).cast::<u8>(),
                (&raw const v1).cast::<u8>(),
            );
            __spark_map_remove(m, (&raw const key).cast::<u8>());
            __spark_map_insert(
                m,
                (&raw const key).cast::<u8>(),
                (&raw const v2).cast::<u8>(),
            );

            assert_eq!(__spark_map_len(m), 1);
            let result = __spark_map_get(m, (&raw const key).cast::<u8>());
            assert_eq!(*(result.cast::<i64>()), 31);

            __spark_map_rc_dec(m);
        }
    }

    #[test]
    fn map_grow_many_entries() {
        unsafe {
            let m = test_map_new(8, 8, 0, 0, 0, 0);
            for i in 0..100_i64 {
                let val = i * 10;
                __spark_map_insert(
                    m,
                    (&raw const i).cast::<u8>(),
                    (&raw const val).cast::<u8>(),
                );
            }
            assert_eq!(__spark_map_len(m), 100);

            for i in 0..100_i64 {
                let result = __spark_map_get(m, (&raw const i).cast::<u8>());
                assert!(!result.is_null());
                assert_eq!(*(result.cast::<i64>()), i * 10);
            }

            __spark_map_rc_dec(m);
        }
    }

    #[test]
    fn map_empty_ops() {
        unsafe {
            let m = test_map_new(8, 8, 0, 0, 0, 0);
            assert_eq!(__spark_map_len(m), 0);
            let key: i64 = 1;
            assert_eq!(__spark_map_contains(m, (&raw const key).cast::<u8>()), 0);
            assert!(__spark_map_get(m, (&raw const key).cast::<u8>()).is_null());
            __spark_map_remove(m, (&raw const key).cast::<u8>()); // no-op
            __spark_map_rc_dec(m);
        }
    }

    #[test]
    fn map_cow_shared() {
        unsafe {
            let m = test_map_new(8, 8, 0, 0, 0, 0);
            let key: i64 = 1;
            let val: i64 = 10;
            __spark_map_insert(
                m,
                (&raw const key).cast::<u8>(),
                (&raw const val).cast::<u8>(),
            );

            // Simulate shared ownership.
            __spark_rc_inc(m);
            let cow = __spark_map_cow(m);
            assert_ne!(cow, m); // Should be a new allocation.

            // Both should still have the entry.
            let r1 = __spark_map_get(m, (&raw const key).cast::<u8>());
            let r2 = __spark_map_get(cow, (&raw const key).cast::<u8>());
            assert_eq!(*(r1.cast::<i64>()), 10);
            assert_eq!(*(r2.cast::<i64>()), 10);

            __spark_map_rc_dec(m);
            __spark_map_rc_dec(cow);
        }
    }

    #[test]
    fn map_cow_sole_owner() {
        unsafe {
            let m = test_map_new(8, 8, 0, 0, 0, 0);
            let key: i64 = 1;
            let val: i64 = 10;
            __spark_map_insert(
                m,
                (&raw const key).cast::<u8>(),
                (&raw const val).cast::<u8>(),
            );

            // rc == 1 — COW should return same pointer.
            let cow = __spark_map_cow(m);
            assert_eq!(cow, m);

            __spark_map_rc_dec(m);
        }
    }

    #[test]
    fn map_cow_uses_retain_descriptors() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static RETAINS: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn count_retain(_slot: *mut u8) {
            RETAINS.fetch_add(1, Ordering::SeqCst);
        }

        unsafe {
            RETAINS.store(0, Ordering::SeqCst);
            let m = __spark_map_new(
                test_desc_with_retain(8, 0, count_retain as *const () as usize, 0, 0),
                test_desc_with_retain(8, 0, count_retain as *const () as usize, 0, 0),
            );
            let key: i64 = 1;
            let val: i64 = 10;
            __spark_map_insert(
                m,
                (&raw const key).cast::<u8>(),
                (&raw const val).cast::<u8>(),
            );

            __spark_rc_inc(m);
            let cow = __spark_map_cow(m);

            assert_ne!(cow, m);
            assert_eq!(RETAINS.load(Ordering::SeqCst), 2);

            __spark_map_rc_dec(m);
            __spark_map_rc_dec(cow);
        }
    }

    #[test]
    fn map_string_key_value_semantics_with_callbacks() {
        unsafe {
            let m = test_map_new(
                std::mem::size_of::<*mut u8>(),
                std::mem::size_of::<i64>(),
                0,
                0,
                __spark_hash_string_elem as *const () as usize,
                __spark_eq_string_elem as *const () as usize,
            );

            let k1 = __spark_string_from_literal("hello".as_ptr(), 5);
            let k2 = __spark_string_from_literal("hello".as_ptr(), 5);
            let v: i64 = 123;

            __spark_map_insert(m, (&raw const k1).cast::<u8>(), (&raw const v).cast::<u8>());

            let got = __spark_map_get(m, (&raw const k2).cast::<u8>());
            assert!(!got.is_null());
            assert_eq!(*(got.cast::<i64>()), 123);

            __spark_string_rc_dec(k1);
            __spark_string_rc_dec(k2);
            __spark_map_rc_dec(m);
        }
    }

    // ========================================================================
    // Set tests
    // ========================================================================

    #[test]
    fn set_insert_and_contains() {
        unsafe {
            let s = test_set_new(8, 0, 0, 0);
            let val: i64 = 42;
            __spark_set_insert(s, (&raw const val).cast::<u8>());
            assert_eq!(__spark_set_contains(s, (&raw const val).cast::<u8>()), 1);
            assert_eq!(__spark_set_len(s), 1);

            __spark_set_rc_dec(s);
        }
    }

    #[test]
    fn set_dedup() {
        unsafe {
            let s = test_set_new(8, 0, 0, 0);
            let val: i64 = 10;
            __spark_set_insert(s, (&raw const val).cast::<u8>());
            __spark_set_insert(s, (&raw const val).cast::<u8>());
            __spark_set_insert(s, (&raw const val).cast::<u8>());
            assert_eq!(__spark_set_len(s), 1);

            __spark_set_rc_dec(s);
        }
    }

    #[test]
    fn set_duplicate_insert_drops_unused_elem() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static DROPS: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn count_drop(_slot: *mut u8) {
            DROPS.fetch_add(1, Ordering::SeqCst);
        }

        unsafe {
            DROPS.store(0, Ordering::SeqCst);
            let s = test_set_new(8, count_drop as *const () as usize, 0, 0);
            let val: i64 = 10;

            __spark_set_insert(s, (&raw const val).cast::<u8>());
            __spark_set_insert(s, (&raw const val).cast::<u8>());

            assert_eq!(DROPS.load(Ordering::SeqCst), 1);
            __spark_set_rc_dec(s);
            assert_eq!(DROPS.load(Ordering::SeqCst), 2);
        }
    }

    #[test]
    fn set_remove() {
        unsafe {
            let s = test_set_new(8, 0, 0, 0);
            let val: i64 = 7;
            __spark_set_insert(s, (&raw const val).cast::<u8>());
            assert_eq!(__spark_set_len(s), 1);

            __spark_set_remove(s, (&raw const val).cast::<u8>());
            assert_eq!(__spark_set_len(s), 0);
            assert_eq!(__spark_set_contains(s, (&raw const val).cast::<u8>()), 0);

            __spark_set_rc_dec(s);
        }
    }

    #[test]
    fn set_remove_and_reinsert() {
        unsafe {
            let s = test_set_new(8, 0, 0, 0);
            let val: i64 = 3;
            __spark_set_insert(s, (&raw const val).cast::<u8>());
            __spark_set_remove(s, (&raw const val).cast::<u8>());
            __spark_set_insert(s, (&raw const val).cast::<u8>());

            assert_eq!(__spark_set_len(s), 1);
            assert_eq!(__spark_set_contains(s, (&raw const val).cast::<u8>()), 1);

            __spark_set_rc_dec(s);
        }
    }

    #[test]
    fn set_grow_many_elements() {
        unsafe {
            let s = test_set_new(8, 0, 0, 0);
            for i in 0..100_i64 {
                __spark_set_insert(s, (&raw const i).cast::<u8>());
            }
            assert_eq!(__spark_set_len(s), 100);

            for i in 0..100_i64 {
                assert_eq!(__spark_set_contains(s, (&raw const i).cast::<u8>()), 1);
            }

            // Verify element not in the set.
            let missing: i64 = 200;
            assert_eq!(
                __spark_set_contains(s, (&raw const missing).cast::<u8>()),
                0
            );

            __spark_set_rc_dec(s);
        }
    }

    #[test]
    fn set_empty_ops() {
        unsafe {
            let s = test_set_new(8, 0, 0, 0);
            assert_eq!(__spark_set_len(s), 0);
            let val: i64 = 1;
            assert_eq!(__spark_set_contains(s, (&raw const val).cast::<u8>()), 0);
            __spark_set_remove(s, (&raw const val).cast::<u8>()); // no-op
            __spark_set_rc_dec(s);
        }
    }

    #[test]
    fn set_cow_shared() {
        unsafe {
            let s = test_set_new(8, 0, 0, 0);
            let val: i64 = 42;
            __spark_set_insert(s, (&raw const val).cast::<u8>());

            __spark_rc_inc(s);
            let cow = __spark_set_cow(s);
            assert_ne!(cow, s);

            assert_eq!(__spark_set_contains(s, (&raw const val).cast::<u8>()), 1);
            assert_eq!(__spark_set_contains(cow, (&raw const val).cast::<u8>()), 1);

            __spark_set_rc_dec(s);
            __spark_set_rc_dec(cow);
        }
    }

    #[test]
    fn set_cow_sole_owner() {
        unsafe {
            let s = test_set_new(8, 0, 0, 0);
            let val: i64 = 42;
            __spark_set_insert(s, (&raw const val).cast::<u8>());

            let cow = __spark_set_cow(s);
            assert_eq!(cow, s);

            __spark_set_rc_dec(s);
        }
    }

    #[test]
    fn set_cow_uses_retain_descriptor() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static RETAINS: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn count_retain(_slot: *mut u8) {
            RETAINS.fetch_add(1, Ordering::SeqCst);
        }

        unsafe {
            RETAINS.store(0, Ordering::SeqCst);
            let s = __spark_set_new(test_desc_with_retain(
                8,
                0,
                count_retain as *const () as usize,
                0,
                0,
            ));
            let val: i64 = 42;
            __spark_set_insert(s, (&raw const val).cast::<u8>());

            __spark_rc_inc(s);
            let cow = __spark_set_cow(s);

            assert_ne!(cow, s);
            assert_eq!(RETAINS.load(Ordering::SeqCst), 1);

            __spark_set_rc_dec(s);
            __spark_set_rc_dec(cow);
        }
    }

    #[test]
    fn set_string_value_semantics_with_callbacks() {
        unsafe {
            let s = test_set_new(
                std::mem::size_of::<*mut u8>(),
                0,
                __spark_hash_string_elem as *const () as usize,
                __spark_eq_string_elem as *const () as usize,
            );

            let v1 = __spark_string_from_literal("hello".as_ptr(), 5);
            let v2 = __spark_string_from_literal("hello".as_ptr(), 5);

            __spark_set_insert(s, (&raw const v1).cast::<u8>());
            __spark_set_insert(s, (&raw const v2).cast::<u8>());

            assert_eq!(__spark_set_len(s), 1);
            assert_eq!(__spark_set_contains(s, (&raw const v2).cast::<u8>()), 1);

            __spark_string_rc_dec(v1);
            __spark_string_rc_dec(v2);
            __spark_set_rc_dec(s);
        }
    }
}
