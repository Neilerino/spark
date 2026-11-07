//! Shared `SparkString` conversion helpers.

use crate::{
    value::{retain_rc_elem, value_desc_new},
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
    let bytes = unsafe { spark_str_bytes(text) };
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

/// Splits UTF-8 text into an owned list of lines.
///
/// # Safety
/// `text` must point to a live `SparkString` containing valid UTF-8.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_string_lines(text: *const u8) -> *mut u8 {
    let text = unsafe { &*text.cast::<SparkString>() };
    let bytes = unsafe { spark_str_bytes(text) };
    let text = unsafe { std::str::from_utf8_unchecked(bytes) };
    string_list_from_iter(text.lines())
}

unsafe fn spark_str_bytes(text: &SparkString) -> &[u8] {
    if text.len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(text.data, text.len) }
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
    let slice = unsafe { spark_str_bytes(s) };
    std::str::from_utf8(slice)
        .unwrap_or("<invalid utf-8>")
        .to_owned()
}

#[allow(clippy::cast_possible_wrap)]
pub fn rust_str_to_spark(s: &str) -> *mut u8 {
    let bytes = s.as_bytes();
    unsafe { __spark_string_from_literal(bytes.as_ptr(), bytes.len() as i64) }
}

#[allow(clippy::cast_ptr_alignment)]
unsafe extern "C" fn drop_string_elem(elem_ptr: *mut u8) {
    let string = unsafe { *elem_ptr.cast::<*mut u8>() };
    unsafe { __spark_string_rc_dec(string) };
}

pub fn string_list_from_iter<S: AsRef<str>>(iter: impl Iterator<Item = S>) -> *mut u8 {
    let elem_size = std::mem::size_of::<*mut u8>();
    let elem_align = std::mem::align_of::<*mut u8>();
    let elem_drop = drop_string_elem as *const () as usize;
    let elem_retain = retain_rc_elem as *const () as usize;
    let elem_eq = __spark_eq_string_elem as *const () as usize;
    let elem_desc = value_desc_new(elem_size, elem_align, elem_drop, elem_retain, 0, elem_eq);
    let list = __spark_list_new(elem_desc.cast());
    for s in iter {
        let spark_str = rust_str_to_spark(s.as_ref());
        unsafe { __spark_list_push(list, std::ptr::addr_of!(spark_str).cast::<u8>()) };
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{__spark_list_get, __spark_list_rc_dec, __spark_list_remove, __spark_rc_inc};

    #[test]
    #[allow(clippy::cast_ptr_alignment)]
    fn lines_releases_removed_and_remaining_rows() {
        unsafe {
            let source = rust_str_to_spark("first\nsecond");
            let lines = __spark_string_lines(source);
            __spark_string_rc_dec(source);

            let first = *__spark_list_get(lines, 0).cast::<*mut u8>();
            let second = *__spark_list_get(lines, 1).cast::<*mut u8>();
            __spark_rc_inc(first);
            __spark_rc_inc(second);

            __spark_list_remove(lines, 0);
            let first_rc = (*first.cast::<SparkString>()).rc;
            __spark_list_rc_dec(lines);
            let second_rc = (*second.cast::<SparkString>()).rc;

            __spark_string_rc_dec(first);
            __spark_string_rc_dec(second);
            assert_eq!(first_rc, 1, "removing a row must release it");
            assert_eq!(second_rc, 1, "dropping the list must release its rows");
        }
    }
}
