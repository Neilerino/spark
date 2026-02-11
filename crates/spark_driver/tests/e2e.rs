//! End-to-end tests for the Spark compiler.
//!
//! Each test compiles and runs a `.spark` fixture file via the `spark` binary,
//! then asserts against stdout, stderr, and exit code. This provides full-pipeline
//! regression coverage from source code to runtime output.
//!
//! Tests are serialized via a mutex because the driver uses a shared temp path
//! (`/tmp/spark_output.o`) during linking.

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

/// Serialize all E2E tests to avoid races on the shared `/tmp/spark_output.o`.
static LOCK: Mutex<()> = Mutex::new(());

/// Captured output from running a Spark program.
struct SparkOutput {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

/// Runs `spark run <fixture>` and captures output.
fn run_spark(fixture: &str) -> SparkOutput {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);

    assert!(
        fixture_path.exists(),
        "fixture not found: {}",
        fixture_path.display()
    );

    let spark_bin = env!("CARGO_BIN_EXE_spark");

    let output = Command::new(spark_bin)
        .arg("run")
        .arg(&fixture_path)
        .output()
        .expect("failed to run spark binary");

    SparkOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into(),
        stderr: String::from_utf8_lossy(&output.stderr).into(),
        exit_code: output.status.code().unwrap_or(-1),
    }
}

/// Runs a fixture and asserts stdout matches expected output (exit code 0).
fn assert_output(fixture: &str, expected_stdout: &str) {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let result = run_spark(fixture);
    assert_eq!(
        result.exit_code, 0,
        "fixture {fixture} exited with code {}:\n{}",
        result.exit_code, result.stderr
    );
    assert_eq!(
        result.stdout.trim(),
        expected_stdout.trim(),
        "stdout mismatch for {fixture}"
    );
}

/// Runs a fixture and asserts compilation fails with a message fragment in stderr.
fn assert_compile_error(fixture: &str, expected_fragment: &str) {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let result = run_spark(fixture);
    assert_ne!(
        result.exit_code, 0,
        "expected compilation failure for {fixture}, but it succeeded"
    );
    assert!(
        result.stderr.contains(expected_fragment),
        "expected stderr to contain '{expected_fragment}' for {fixture}, got:\n{}",
        result.stderr
    );
}

// ============================================================================
// Basic functionality
// ============================================================================

#[test]
fn hello_world() {
    assert_output("hello.spark", "Hello, Spark!");
}

#[test]
fn arithmetic() {
    assert_output("arithmetic.spark", "15\n5\n50\n2");
}

#[test]
fn if_else() {
    assert_output("if_else.spark", "bigger");
}

// ============================================================================
// Collection methods
// ============================================================================

#[test]
fn list_len() {
    assert_output("list_len.spark", "3");
}

#[test]
fn list_append() {
    assert_output("list_append.spark", "3");
}

#[test]
fn list_contains() {
    assert_output("list_contains.spark", "true\nfalse");
}

#[test]
fn string_len() {
    assert_output("string_len.spark", "5");
}

// ============================================================================
// Match expressions
// ============================================================================

#[test]
fn match_bool() {
    assert_output("match_bool.spark", "yes");
}

// ============================================================================
// Error cases
// ============================================================================

#[test]
fn immutable_append_error() {
    assert_compile_error("immutable_append.spark", "not mutable");
}

#[test]
fn match_nonexhaustive_bool() {
    assert_compile_error("match_nonexhaustive_bool.spark", "non-exhaustive");
}

#[test]
fn match_nonexhaustive_int() {
    assert_compile_error("match_nonexhaustive_int.spark", "non-exhaustive");
}

// ============================================================================
// Structs
// ============================================================================

#[test]
fn struct_field_access() {
    assert_output("struct_field.spark", "10\n20");
}

#[test]
fn struct_param() {
    assert_output("struct_param.spark", "25");
}

// ============================================================================
// Loops
// ============================================================================

#[test]
fn loop_break() {
    assert_output("loop_break.spark", "4");
}

// ============================================================================
// Generics
// ============================================================================

#[test]
fn generic_struct_init() {
    assert_output("generic_struct_init.spark", "3\n4");
}

#[test]
fn generic_identity() {
    assert_output("generic_identity.spark", "42");
}

// ============================================================================
// Lambdas & function references
// ============================================================================

#[test]
fn lambda_call() {
    assert_output("lambda_call.spark", "10");
}

#[test]
fn fn_reference() {
    assert_output("fn_reference.spark", "14");
}

#[test]
fn higher_order() {
    assert_output("higher_order.spark", "15");
}

#[test]
fn closure_capture() {
    assert_output("closure_capture.spark", "15");
}

#[test]
fn closure_multi_capture() {
    assert_output("closure_multi_capture.spark", "20");
}
