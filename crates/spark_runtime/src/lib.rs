//! Spark runtime library.
//!
//! Provides `extern "C"` functions that are called by compiled Spark programs.
//! This crate is compiled as a static library (`libspark_runtime.a`) and linked
//! into every Spark executable.

// Safety: This crate is an FFI boundary — `#[no_mangle]` and raw pointers
// are required for the C ABI. No actual unsafe blocks are used.
#![allow(unsafe_code)]
#![allow(clippy::missing_const_for_fn)] // extern "C" FFI stubs are not const

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
    let s = unsafe { &*(ptr.cast::<String>()) };
    println!("{s}");
}

/// Prints an empty newline.
#[no_mangle]
pub extern "C" fn __spark_println() {
    println!();
}

// =============================================================================
// String functions
// =============================================================================

/// Creates a new empty heap-allocated string.
#[no_mangle]
pub extern "C" fn __spark_string_new() -> *mut u8 {
    let s = Box::new(String::new());
    Box::into_raw(s).cast::<u8>()
}

/// Creates a heap-allocated string from a UTF-8 literal (pointer + length).
///
/// # Safety
///
/// `data` must be a valid pointer to `len` bytes of UTF-8 data.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_string_from_literal(data: *const u8, len: i64) -> *mut u8 {
    let slice = unsafe { std::slice::from_raw_parts(data, len as usize) };
    let s = Box::new(String::from_utf8_lossy(slice).into_owned());
    Box::into_raw(s).cast::<u8>()
}

// =============================================================================
// Collection stubs (placeholders)
// =============================================================================

/// Creates a new empty list. Returns a null pointer (placeholder).
#[no_mangle]
pub extern "C" fn __spark_list_new() -> *mut u8 {
    std::ptr::null_mut()
}

/// Creates a new empty map. Returns a null pointer (placeholder).
#[no_mangle]
pub extern "C" fn __spark_map_new() -> *mut u8 {
    std::ptr::null_mut()
}

/// Creates a new empty set. Returns a null pointer (placeholder).
#[no_mangle]
pub extern "C" fn __spark_set_new() -> *mut u8 {
    std::ptr::null_mut()
}

// =============================================================================
// Reference counting stubs (placeholders)
// =============================================================================

/// Releases a reference-counted object. No-op (placeholder).
#[no_mangle]
pub extern "C" fn __spark_rc_release(_ptr: *mut u8) {
    // No-op until RC is implemented
}
