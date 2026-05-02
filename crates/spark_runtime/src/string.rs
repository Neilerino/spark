//! Shared `SparkString` conversion helpers.

use crate::{
    SparkString, __spark_eq_string_elem, __spark_list_new, __spark_list_push,
    __spark_string_from_literal, __spark_string_rc_dec,
};

/// Reads a `SparkString` pointer into an owned Rust `String`.
///
/// # Safety
///
/// `ptr` must be null or a valid pointer to a `SparkString`.
#[allow(clippy::cast_ptr_alignment)]
pub(crate) unsafe fn read_spark_str(ptr: *mut u8) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let s = unsafe { &*(ptr.cast::<SparkString>()) };
    if s.len == 0 {
        return String::new();
    }
    let slice = unsafe { std::slice::from_raw_parts(s.data, s.len) };
    std::str::from_utf8(slice)
        .unwrap_or("<invalid utf-8>")
        .to_owned()
}

#[allow(clippy::cast_possible_wrap)]
pub(crate) fn rust_str_to_spark(s: &str) -> *mut u8 {
    let bytes = s.as_bytes();
    unsafe { __spark_string_from_literal(bytes.as_ptr(), bytes.len() as i64) }
}

#[allow(clippy::cast_possible_wrap)]
pub(crate) fn string_list_from_iter(iter: impl Iterator<Item = String>) -> *mut u8 {
    let elem_size = std::mem::size_of::<*mut u8>() as i64;
    let elem_drop = __spark_string_rc_dec as *const () as usize;
    let elem_eq = __spark_eq_string_elem as *const () as usize;
    let list = __spark_list_new(elem_size, elem_drop, elem_eq);
    for s in iter {
        let spark_str = rust_str_to_spark(&s);
        unsafe { __spark_list_push(list, std::ptr::addr_of!(spark_str).cast::<u8>()) };
    }
    list
}
