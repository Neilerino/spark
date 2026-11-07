//! Shared `SparkString` conversion helpers.

use crate::{
    value::{retain_rc_elem, value_desc_new},
    SparkString, __spark_eq_string_elem, __spark_list_new, __spark_list_push, __spark_list_rc_dec,
    __spark_string_from_literal, __spark_string_rc_dec,
};

/// Parses one signed decimal integer without copying the source string.
/// Returns zero on success, or a nonzero parse error code.
///
/// # Safety
/// `text` must point to a slot holding a live UTF-8 `SparkString` pointer.
/// `value` must point to a writable `i64` that does not alias the input.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_text_parse_int(text: *const *const u8, value: *mut i64) -> i64 {
    let text = unsafe { spark_str(&*(*text).cast::<SparkString>()) };
    match parse_int(text) {
        Ok(parsed) => {
            unsafe { *value = parsed };
            0
        }
        Err(code) => code,
    }
}

fn parse_int(text: &str) -> Result<i64, i64> {
    use std::num::IntErrorKind::{Empty, NegOverflow, PosOverflow};

    text.parse::<i64>().map_err(|error| match error.kind() {
        Empty => 1,
        PosOverflow | NegOverflow => {
            let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
            if digits.bytes().all(|byte| byte.is_ascii_digit()) {
                3
            } else {
                2
            }
        }
        _ => 2,
    })
}

/// Parses whitespace-delimited signed integers directly into an owned list.
/// On failure, releases the partial list, reports the token span, and returns
/// an empty list for the Spark wrapper to release.
///
/// # Safety
/// `text` must point to a slot holding a live UTF-8 `SparkString` pointer.
/// The output pointers must point to distinct writable `i64` slots that do
/// not alias the input.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_wrap)]
pub unsafe extern "C" fn __spark_text_parse_ints(
    text: *const *const u8,
    code: *mut i64,
    start: *mut i64,
    end: *mut i64,
) -> *mut u8 {
    let text = unsafe { spark_str(&*(*text).cast::<SparkString>()) };
    let new_list = || {
        let desc = value_desc_new(size_of::<i64>(), align_of::<i64>(), 0, 0, 0, 0);
        __spark_list_new(desc.cast())
    };
    let list = new_list();
    for token in text.split_whitespace() {
        let value = match parse_int(token) {
            Ok(value) => value,
            Err(reason) => {
                let offset = token.as_ptr().addr() - text.as_ptr().addr();
                unsafe {
                    *code = reason;
                    *start = offset as i64;
                    *end = (offset + token.len()) as i64;
                    __spark_list_rc_dec(list);
                }
                return new_list();
            }
        };
        unsafe { __spark_list_push(list, std::ptr::addr_of!(value).cast()) };
    }
    unsafe { *code = 0 };
    list
}

/// Splits UTF-8 text into an owned list of lines.
///
/// # Safety
/// `text` must point to a live `SparkString` containing valid UTF-8.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_string_lines(text: *const u8) -> *mut u8 {
    let text = unsafe { spark_str(&*text.cast::<SparkString>()) };
    string_list_from_iter(text.lines())
}

/// Splits UTF-8 text at literal delimiters.
///
/// # Safety
/// Both pointers must refer to live UTF-8 `SparkString` values.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_string_split(text: *const u8, delimiter: *const u8) -> *mut u8 {
    let text = unsafe { spark_str(&*text.cast::<SparkString>()) };
    let delimiter = unsafe { spark_str(&*delimiter.cast::<SparkString>()) };
    if delimiter.is_empty() {
        string_list_from_iter(
            text.char_indices()
                .map(|(start, ch)| &text[start..start + ch.len_utf8()]),
        )
    } else {
        string_list_from_iter(text.split(delimiter))
    }
}

/// Splits UTF-8 text into nonempty Unicode whitespace-delimited fields.
///
/// # Safety
/// `text` must point to a live UTF-8 `SparkString`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_string_split_whitespace(text: *const u8) -> *mut u8 {
    let text = unsafe { spark_str(&*text.cast::<SparkString>()) };
    string_list_from_iter(text.split_whitespace())
}

/// Removes leading and trailing Unicode whitespace.
///
/// # Safety
/// `text` must point to a live UTF-8 `SparkString`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_string_trim(text: *const u8) -> *mut u8 {
    let text = unsafe { spark_str(&*text.cast::<SparkString>()) };
    rust_str_to_spark(text.trim())
}

/// Splits UTF-8 text into nonempty sections separated by blank lines.
///
/// # Safety
/// `text` must point to a live UTF-8 `SparkString`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_string_sections(text: *const u8) -> *mut u8 {
    let text = unsafe { spark_str(&*text.cast::<SparkString>()) };
    let base = text.as_ptr().addr();
    let mut lines = text.lines();
    string_list_from_iter(std::iter::from_fn(|| {
        let first = lines.find(|line| !line.is_empty())?;
        let last = lines
            .by_ref()
            .take_while(|line| !line.is_empty())
            .last()
            .unwrap_or(first);
        let start = first.as_ptr().addr() - base;
        let end = last.as_ptr().addr() - base + last.len();
        Some(&text[start..end])
    }))
}

/// Returns the Unicode scalar values in a UTF-8 string.
///
/// # Safety
/// `text` must point to a live UTF-8 `SparkString`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_string_chars(text: *const u8) -> *mut u8 {
    let text = unsafe { spark_str(&*text.cast::<SparkString>()) };
    let desc = value_desc_new(size_of::<u32>(), align_of::<u32>(), 0, 0, 0, 0);
    let list = __spark_list_new(desc.cast());
    for ch in text.chars() {
        let scalar = u32::from(ch);
        unsafe { __spark_list_push(list, std::ptr::addr_of!(scalar).cast()) };
    }
    list
}

unsafe fn spark_str(text: &SparkString) -> &str {
    unsafe { std::str::from_utf8_unchecked(spark_str_bytes(text)) }
}

/// Copies a checked UTF-8 byte range and writes whether the range was valid.
///
/// # Safety
/// `text` must point to a slot holding a live UTF-8 `SparkString` pointer.
/// `valid` must point to a writable `i64` that does not alias the input.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_text_substring(
    text: *const *const u8,
    start: i64,
    end: i64,
    valid: *mut i64,
) -> *mut u8 {
    let text = unsafe { spark_str(&*(*text).cast::<SparkString>()) };
    let slice = usize::try_from(start)
        .ok()
        .zip(usize::try_from(end).ok())
        .and_then(|(start, end)| text.get(start..end));
    unsafe { *valid = i64::from(slice.is_some()) };
    rust_str_to_spark(slice.unwrap_or_default())
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
    use crate::{__spark_list_get, __spark_list_remove, __spark_rc_inc};

    #[test]
    fn bulk_integer_error_after_many_tokens_exposes_no_partial_list() {
        let mut text = "1 ".repeat(16_384);
        text.push_str("bad 7");
        let source = rust_str_to_spark(&text).cast_const();
        let (mut code, mut start, mut end) = (0, 0, 0);
        unsafe {
            let result = __spark_text_parse_ints(&source, &mut code, &mut start, &mut end);
            let len = crate::__spark_list_len(result);
            __spark_list_rc_dec(result);
            __spark_string_rc_dec(source.cast_mut());
            assert_eq!(len, 0);
        }
        assert_eq!((code, start, end), (2, 32_768, 32_771));
    }

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
