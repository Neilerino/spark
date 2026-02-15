//! `spark.io` runtime implementations.
//!
//! Provides `extern "C"` functions for the `spark.io` standard library module.
//! Wraps `std::fs` operations with a thread-local error message for the last
//! IO error.
//!
//! ## Error Model
//!
//! Functions that can fail store the error message in a thread-local string
//! and return an integer error code:
//! - 0 = success
//! - 1 = NotFound
//! - 2 = PermissionDenied
//! - 3 = AlreadyExists
//! - 4 = Other

use std::cell::RefCell;
use std::io::Write;

use crate::{__spark_string_from_literal, SparkString};

thread_local! {
    static LAST_IO_ERROR: RefCell<String> = RefCell::new(String::new());
}

/// Sets the last IO error message.
fn set_error(msg: String) {
    LAST_IO_ERROR.with(|e| {
        *e.borrow_mut() = msg;
    });
}

/// Clears the last IO error message.
fn clear_error() {
    LAST_IO_ERROR.with(|e| {
        e.borrow_mut().clear();
    });
}

/// Maps a `std::io::ErrorKind` to a Spark IO error code.
fn error_code(err: &std::io::Error) -> i64 {
    match err.kind() {
        std::io::ErrorKind::NotFound => 1,
        std::io::ErrorKind::PermissionDenied => 2,
        std::io::ErrorKind::AlreadyExists => 3,
        _ => 4,
    }
}

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

// =============================================================================
// FFI functions
// =============================================================================

/// Reads the entire contents of a file as a string.
///
/// Returns the file contents as a `SparkString`, or an empty string on error
/// (with the error message stored in `LAST_IO_ERROR`).
///
/// # Safety
///
/// `path` must be a valid `SparkString` pointer.
#[no_mangle]
pub unsafe extern "C" fn __spark_io_read_file(path: *mut u8) -> *mut u8 {
    let path_str = unsafe { read_spark_str(path) };
    match std::fs::read_to_string(&path_str) {
        Ok(contents) => {
            clear_error();
            rust_str_to_spark(&contents)
        }
        Err(e) => {
            set_error(format!("{e}"));
            rust_str_to_spark("")
        }
    }
}

/// Writes content to a file, creating it if it doesn't exist, overwriting if
/// it does.
///
/// Returns 0 on success, or an error code on failure.
///
/// # Safety
///
/// `path` and `content` must be valid `SparkString` pointers.
#[no_mangle]
pub unsafe extern "C" fn __spark_io_write_file(path: *mut u8, content: *mut u8) -> i64 {
    let path_str = unsafe { read_spark_str(path) };
    let content_str = unsafe { read_spark_str(content) };
    match std::fs::write(&path_str, content_str.as_bytes()) {
        Ok(()) => {
            clear_error();
            0
        }
        Err(e) => {
            let code = error_code(&e);
            set_error(format!("{e}"));
            code
        }
    }
}

/// Appends content to a file, creating it if it doesn't exist.
///
/// Returns 0 on success, or an error code on failure.
///
/// # Safety
///
/// `path` and `content` must be valid `SparkString` pointers.
#[no_mangle]
pub unsafe extern "C" fn __spark_io_append_file(path: *mut u8, content: *mut u8) -> i64 {
    let path_str = unsafe { read_spark_str(path) };
    let content_str = unsafe { read_spark_str(content) };
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path_str)
        .and_then(|mut f| f.write_all(content_str.as_bytes()));
    match result {
        Ok(()) => {
            clear_error();
            0
        }
        Err(e) => {
            let code = error_code(&e);
            set_error(format!("{e}"));
            code
        }
    }
}

/// Returns 1 if a file or directory exists at the given path, 0 otherwise.
///
/// # Safety
///
/// `path` must be a valid `SparkString` pointer.
#[no_mangle]
pub unsafe extern "C" fn __spark_io_file_exists(path: *mut u8) -> i64 {
    let path_str = unsafe { read_spark_str(path) };
    if std::path::Path::new(&path_str).exists() {
        1
    } else {
        0
    }
}

/// Deletes a file.
///
/// Returns 0 on success, or an error code on failure.
///
/// # Safety
///
/// `path` must be a valid `SparkString` pointer.
#[no_mangle]
pub unsafe extern "C" fn __spark_io_delete_file(path: *mut u8) -> i64 {
    let path_str = unsafe { read_spark_str(path) };
    match std::fs::remove_file(&path_str) {
        Ok(()) => {
            clear_error();
            0
        }
        Err(e) => {
            let code = error_code(&e);
            set_error(format!("{e}"));
            code
        }
    }
}

/// Creates a directory.
///
/// Returns 0 on success, or an error code on failure (e.g. if it already
/// exists or the parent doesn't exist).
///
/// # Safety
///
/// `path` must be a valid `SparkString` pointer.
#[no_mangle]
pub unsafe extern "C" fn __spark_io_create_dir(path: *mut u8) -> i64 {
    let path_str = unsafe { read_spark_str(path) };
    match std::fs::create_dir(&path_str) {
        Ok(()) => {
            clear_error();
            0
        }
        Err(e) => {
            let code = error_code(&e);
            set_error(format!("{e}"));
            code
        }
    }
}

/// Creates a directory and all parent directories.
///
/// Returns 0 on success, or an error code on failure.
///
/// # Safety
///
/// `path` must be a valid `SparkString` pointer.
#[no_mangle]
pub unsafe extern "C" fn __spark_io_create_dir_all(path: *mut u8) -> i64 {
    let path_str = unsafe { read_spark_str(path) };
    match std::fs::create_dir_all(&path_str) {
        Ok(()) => {
            clear_error();
            0
        }
        Err(e) => {
            let code = error_code(&e);
            set_error(format!("{e}"));
            code
        }
    }
}

/// Lists directory entries as a newline-separated string.
///
/// Returns the entries as a `SparkString`, or an empty string on error
/// (with the error message stored in `LAST_IO_ERROR`).
///
/// # Safety
///
/// `path` must be a valid `SparkString` pointer.
#[no_mangle]
pub unsafe extern "C" fn __spark_io_list_dir(path: *mut u8) -> *mut u8 {
    let path_str = unsafe { read_spark_str(path) };
    match std::fs::read_dir(&path_str) {
        Ok(entries) => {
            clear_error();
            let mut names = Vec::new();
            for entry in entries {
                match entry {
                    Ok(e) => {
                        names.push(e.file_name().to_string_lossy().into_owned());
                    }
                    Err(e) => {
                        set_error(format!("{e}"));
                        return rust_str_to_spark("");
                    }
                }
            }
            names.sort();
            rust_str_to_spark(&names.join("\n"))
        }
        Err(e) => {
            set_error(format!("{e}"));
            rust_str_to_spark("")
        }
    }
}

/// Reads a single line from standard input.
///
/// Returns the line with trailing newline stripped (Python `input()` semantics).
/// Returns an empty string on EOF or error (with error stored in `LAST_IO_ERROR`).
#[no_mangle]
pub extern "C" fn __spark_io_read_line() -> *mut u8 {
    let mut buf = String::new();
    match std::io::stdin().read_line(&mut buf) {
        Ok(0) => {
            // EOF
            clear_error();
            rust_str_to_spark("")
        }
        Ok(_) => {
            clear_error();
            let trimmed = buf.trim_end_matches('\n').trim_end_matches('\r');
            rust_str_to_spark(trimmed)
        }
        Err(e) => {
            set_error(format!("{e}"));
            rust_str_to_spark("")
        }
    }
}

/// Writes a line of text to standard error.
///
/// # Safety
///
/// `text` must be a valid `SparkString` pointer.
#[no_mangle]
pub unsafe extern "C" fn __spark_io_write_stderr(text: *mut u8) {
    let text_str = unsafe { read_spark_str(text) };
    eprintln!("{text_str}");
}

/// Returns the last IO error message as a new `SparkString`.
#[no_mangle]
pub extern "C" fn __spark_io_error() -> *mut u8 {
    LAST_IO_ERROR.with(|e| {
        let msg = e.borrow();
        rust_str_to_spark(&msg)
    })
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

    fn make_spark_str(s: &str) -> *mut u8 {
        rust_str_to_spark(s)
    }

    #[test]
    fn write_and_read_file() {
        let path = make_spark_str("/tmp/spark_test_io_write_read.txt");
        let content = make_spark_str("hello spark io");

        unsafe {
            let code = __spark_io_write_file(path, content);
            assert_eq!(code, 0, "write should succeed");

            let result = __spark_io_read_file(path);
            assert_eq!(read_result_string(result), "hello spark io");

            // Clean up
            __spark_io_delete_file(path);
        }
    }

    #[test]
    fn read_nonexistent_file() {
        let path = make_spark_str("/tmp/spark_test_io_nonexistent_file_xyz.txt");

        unsafe {
            let result = __spark_io_read_file(path);
            assert_eq!(read_result_string(result), "");

            let err = __spark_io_error();
            let msg = read_result_string(err);
            assert!(!msg.is_empty(), "error message should not be empty");
        }
    }

    #[test]
    fn file_exists_check() {
        let path = make_spark_str("/tmp/spark_test_io_exists_check.txt");
        let content = make_spark_str("exists");

        unsafe {
            assert_eq!(__spark_io_file_exists(path), 0);

            __spark_io_write_file(path, content);
            assert_eq!(__spark_io_file_exists(path), 1);

            __spark_io_delete_file(path);
            assert_eq!(__spark_io_file_exists(path), 0);
        }
    }

    #[test]
    fn append_file() {
        let path = make_spark_str("/tmp/spark_test_io_append.txt");
        let part1 = make_spark_str("hello ");
        let part2 = make_spark_str("world");

        unsafe {
            // Start clean
            __spark_io_delete_file(path);

            let code1 = __spark_io_append_file(path, part1);
            assert_eq!(code1, 0);
            let code2 = __spark_io_append_file(path, part2);
            assert_eq!(code2, 0);

            let result = __spark_io_read_file(path);
            assert_eq!(read_result_string(result), "hello world");

            __spark_io_delete_file(path);
        }
    }

    #[test]
    fn create_and_list_dir() {
        let dir = make_spark_str("/tmp/spark_test_io_dir");
        let file1_path = make_spark_str("/tmp/spark_test_io_dir/a.txt");
        let file2_path = make_spark_str("/tmp/spark_test_io_dir/b.txt");
        let content = make_spark_str("x");

        unsafe {
            // Clean up from previous runs
            let _ = __spark_io_delete_file(file1_path);
            let _ = __spark_io_delete_file(file2_path);
            let _ = std::fs::remove_dir("/tmp/spark_test_io_dir");

            let code = __spark_io_create_dir(dir);
            assert_eq!(code, 0);

            __spark_io_write_file(file1_path, content);
            __spark_io_write_file(file2_path, content);

            let listing = __spark_io_list_dir(dir);
            let listing_str = read_result_string(listing);
            assert!(listing_str.contains("a.txt"));
            assert!(listing_str.contains("b.txt"));

            // Clean up
            __spark_io_delete_file(file1_path);
            __spark_io_delete_file(file2_path);
            let _ = std::fs::remove_dir("/tmp/spark_test_io_dir");
        }
    }

    #[test]
    fn create_dir_all() {
        let dir = make_spark_str("/tmp/spark_test_io_nested/a/b");

        unsafe {
            let code = __spark_io_create_dir_all(dir);
            assert_eq!(code, 0);
            assert_eq!(__spark_io_file_exists(dir), 1);

            // Clean up
            let _ = std::fs::remove_dir_all("/tmp/spark_test_io_nested");
        }
    }

    #[test]
    fn delete_nonexistent_file() {
        let path = make_spark_str("/tmp/spark_test_io_delete_nonexistent.txt");
        unsafe {
            let code = __spark_io_delete_file(path);
            assert_eq!(code, 1, "should return NotFound error code");
        }
    }

    #[test]
    fn write_stderr_basic() {
        // Smoke test: calling write_stderr with a valid SparkString should not panic.
        let text = make_spark_str("stderr test message");
        unsafe {
            __spark_io_write_stderr(text);
        }
        // If we get here without panicking, the function works.
        // The actual output goes to stderr which is hard to capture in unit tests;
        // the E2E test (io_stderr.spark) validates the full pipeline.
    }

    #[test]
    fn error_returns_empty_on_success() {
        let path = make_spark_str("/tmp/spark_test_io_error_clear.txt");
        let content = make_spark_str("ok");
        unsafe {
            __spark_io_write_file(path, content);
            let err = __spark_io_error();
            let msg = read_result_string(err);
            assert!(msg.is_empty(), "error should be empty after success");

            __spark_io_delete_file(path);
        }
    }
}
