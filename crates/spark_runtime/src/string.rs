//! Shared `SparkString` conversion helpers.

use crate::{
    value::{__spark_value_desc_new, retain_rc_elem},
    SparkString, __spark_eq_string_elem, __spark_list_new, __spark_list_push,
    __spark_string_from_literal, __spark_string_rc_dec,
};

/// Parses one signed decimal integer without copying the source string.
/// Returns zero on success, or a nonzero parse error code.
///
/// # Safety
/// `text` must point to a live `SparkString`; `value` must be writable as an
/// `i64` and must not alias the source.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_text_parse_int(text: *const u8, value: *mut i64) -> i64 {
    let text = unsafe { &*text.cast::<SparkString>() };
    let bytes = if text.len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(text.data, text.len) }
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return 2;
    };
    match text.parse::<i64>() {
        Ok(parsed) => {
            unsafe { *value = parsed };
            0
        }
        Err(error) => match error.kind() {
            std::num::IntErrorKind::Empty => 1,
            std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => 3,
            _ => 2,
        },
    }
}

/// Reads a `SparkString` pointer into an owned Rust `String`.
///
/// # Safety
///
/// `ptr` must be null or a valid pointer to a `SparkString`.
#[allow(clippy::cast_ptr_alignment)]
pub unsafe fn read_spark_str(ptr: *mut u8) -> String {
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
pub fn rust_str_to_spark(s: &str) -> *mut u8 {
    let bytes = s.as_bytes();
    unsafe { __spark_string_from_literal(bytes.as_ptr(), bytes.len() as i64) }
}

#[allow(clippy::cast_possible_wrap)]
pub fn string_list_from_iter(iter: impl Iterator<Item = String>) -> *mut u8 {
    let elem_size = std::mem::size_of::<*mut u8>() as i64;
    let elem_align = std::mem::align_of::<*mut u8>() as i64;
    let elem_drop = __spark_string_rc_dec as *const () as usize;
    let elem_retain = retain_rc_elem as *const () as usize;
    let elem_eq = __spark_eq_string_elem as *const () as usize;
    let elem_desc =
        __spark_value_desc_new(elem_size, elem_align, elem_drop, elem_retain, 0, elem_eq);
    let list = __spark_list_new(elem_desc);
    for s in iter {
        let spark_str = rust_str_to_spark(&s);
        unsafe { __spark_list_push(list, std::ptr::addr_of!(spark_str).cast::<u8>()) };
    }
    list
}
