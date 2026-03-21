//! `spark.sys` runtime implementations.
//!
//! Provides `extern "C"` functions for the `spark.sys` standard library module.
//! Wraps `std::env` operations for command-line arguments and environment
//! variables.

use crate::{
    SparkString, __spark_eq_string_elem, __spark_list_new, __spark_list_push,
    __spark_string_from_literal, __spark_string_rc_dec,
};

/// Reads a `SparkString` pointer, returning the content as a Rust `String`.
///
/// # Safety
///
/// `ptr` must be a valid pointer to a `SparkString`.
#[allow(clippy::cast_ptr_alignment)]
unsafe fn read_spark_str(ptr: *mut u8) -> String {
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

/// Creates a new `SparkString` from a Rust `&str`.
fn rust_str_to_spark(s: &str) -> *mut u8 {
    let bytes = s.as_bytes();
    unsafe { __spark_string_from_literal(bytes.as_ptr(), bytes.len() as i64) }
}

/// Builds a `SparkList` of `SparkString` from an iterator of strings.
fn string_list_from_iter(iter: impl Iterator<Item = String>) -> *mut u8 {
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

// =============================================================================
// FFI functions
// =============================================================================

/// Returns the command-line arguments as a `SparkList` of `SparkString`.
///
/// The first element is the program name.
#[no_mangle]
pub extern "C" fn __spark_sys_args() -> *mut u8 {
    string_list_from_iter(std::env::args())
}

/// Returns the value of an environment variable as a `SparkString`.
///
/// Returns an empty string if the variable is not set.
///
/// # Safety
///
/// `name` must be a valid `SparkString` pointer.
#[no_mangle]
pub unsafe extern "C" fn __spark_sys_get_env(name: *mut u8) -> *mut u8 {
    let name_str = unsafe { read_spark_str(name) };
    match std::env::var(&name_str) {
        Ok(val) => rust_str_to_spark(&val),
        Err(_) => rust_str_to_spark(""),
    }
}

/// Returns 1 if the environment variable exists, 0 otherwise.
///
/// # Safety
///
/// `name` must be a valid `SparkString` pointer.
#[no_mangle]
pub unsafe extern "C" fn __spark_sys_has_env(name: *mut u8) -> i64 {
    let name_str = unsafe { read_spark_str(name) };
    if std::env::var(&name_str).is_ok() {
        1
    } else {
        0
    }
}

/// Sets an environment variable for the current process.
///
/// # Safety
///
/// `name` and `value` must be valid `SparkString` pointers.
#[no_mangle]
pub unsafe extern "C" fn __spark_sys_set_env(name: *mut u8, value: *mut u8) {
    let name_str = unsafe { read_spark_str(name) };
    let value_str = unsafe { read_spark_str(value) };
    std::env::set_var(&name_str, &value_str);
}

/// Removes an environment variable from the current process.
///
/// # Safety
///
/// `name` must be a valid `SparkString` pointer.
#[no_mangle]
pub unsafe extern "C" fn __spark_sys_remove_env(name: *mut u8) {
    let name_str = unsafe { read_spark_str(name) };
    std::env::remove_var(&name_str);
}

/// Returns all environment variable names as a `SparkList` of `SparkString`.
#[no_mangle]
pub extern "C" fn __spark_sys_env_keys() -> *mut u8 {
    string_list_from_iter(std::env::vars().map(|(k, _)| k))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: read a SparkString result into a Rust String.
    unsafe fn read_result_string(ptr: *mut u8) -> String {
        let s = unsafe { &*(ptr.cast::<SparkString>()) };
        if s.len == 0 {
            return String::new();
        }
        let slice = unsafe { std::slice::from_raw_parts(s.data, s.len) };
        std::str::from_utf8(slice).unwrap().to_owned()
    }

    /// Helper: read elements from a SparkList of SparkString pointers.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe fn read_string_list(ptr: *mut u8) -> Vec<String> {
        let list = unsafe { &*(ptr.cast::<crate::SparkList>()) };
        let mut result = Vec::new();
        for i in 0..list.len {
            let elem_ptr = unsafe { list.data.add(i * list.elem_size) };
            let str_ptr = unsafe { *(elem_ptr.cast::<*mut u8>()) };
            result.push(unsafe { read_result_string(str_ptr) });
        }
        result
    }

    #[test]
    fn args_returns_non_empty_list() {
        let list_ptr = __spark_sys_args();
        let args = unsafe { read_string_list(list_ptr) };
        // When running under cargo test, there should be at least one arg (the test binary).
        assert!(
            !args.is_empty(),
            "args() should return at least the program name"
        );
    }

    #[test]
    fn get_env_existing() {
        // PATH should always exist on any system.
        let name = rust_str_to_spark("PATH");
        let result = unsafe { __spark_sys_get_env(name) };
        let value = unsafe { read_result_string(result) };
        assert!(!value.is_empty(), "PATH should not be empty");
    }

    #[test]
    fn get_env_missing() {
        let name = rust_str_to_spark("SPARK_TEST_NONEXISTENT_VAR_12345");
        let result = unsafe { __spark_sys_get_env(name) };
        let value = unsafe { read_result_string(result) };
        assert!(
            value.is_empty(),
            "missing env var should return empty string"
        );
    }

    #[test]
    fn has_env_existing() {
        let name = rust_str_to_spark("PATH");
        let result = unsafe { __spark_sys_has_env(name) };
        assert_eq!(result, 1, "PATH should exist");
    }

    #[test]
    fn has_env_missing() {
        let name = rust_str_to_spark("SPARK_TEST_NONEXISTENT_VAR_12345");
        let result = unsafe { __spark_sys_has_env(name) };
        assert_eq!(result, 0, "missing env var should return 0");
    }

    #[test]
    fn set_and_get_env() {
        let name = rust_str_to_spark("SPARK_TEST_SYS_SET_ENV");
        let value = rust_str_to_spark("hello_spark");
        unsafe { __spark_sys_set_env(name, value) };

        let name2 = rust_str_to_spark("SPARK_TEST_SYS_SET_ENV");
        let result = unsafe { __spark_sys_get_env(name2) };
        let got = unsafe { read_result_string(result) };
        assert_eq!(got, "hello_spark");

        // Clean up
        let name3 = rust_str_to_spark("SPARK_TEST_SYS_SET_ENV");
        unsafe { __spark_sys_remove_env(name3) };
    }

    #[test]
    fn remove_env() {
        let name = rust_str_to_spark("SPARK_TEST_SYS_REMOVE_ENV");
        let value = rust_str_to_spark("to_delete");
        unsafe { __spark_sys_set_env(name, value) };

        let name2 = rust_str_to_spark("SPARK_TEST_SYS_REMOVE_ENV");
        assert_eq!(unsafe { __spark_sys_has_env(name2) }, 1);

        let name3 = rust_str_to_spark("SPARK_TEST_SYS_REMOVE_ENV");
        unsafe { __spark_sys_remove_env(name3) };

        let name4 = rust_str_to_spark("SPARK_TEST_SYS_REMOVE_ENV");
        assert_eq!(unsafe { __spark_sys_has_env(name4) }, 0);
    }

    #[test]
    fn env_keys_contains_path() {
        let list_ptr = __spark_sys_env_keys();
        let keys = unsafe { read_string_list(list_ptr) };
        assert!(
            keys.contains(&"PATH".to_owned()),
            "envKeys() should contain PATH"
        );
    }
}
