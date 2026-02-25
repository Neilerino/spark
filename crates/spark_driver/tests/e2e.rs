//! End-to-end tests for the Spark compiler.
//!
//! Each test compiles and runs a `.spark` fixture file via the `spark` binary,
//! then asserts against stdout, stderr, and exit code. This provides full-pipeline
//! regression coverage from source code to runtime output.
//!
//! Tests are serialized via a mutex because the driver uses a shared temp path
//! (`/tmp/spark_output.o`) during linking.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

/// Serialize all E2E tests to avoid races on the shared `/tmp/spark_output.o`.
static LOCK: Mutex<()> = Mutex::new(());

/// Captured output from running a Spark program.
struct SparkOutput {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

/// Runs `spark run <fixture>` and captures output.
///
/// For single-file tests, pass a file name like `"hello.spark"`.
/// For multi-file project tests, use [`run_spark_project`] instead.
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

/// Runs `spark run <project/main.spark>` for multi-file project tests.
///
/// Projects live under `tests/projects/<name>/` and are self-contained
/// directories with their own module structure.
fn run_spark_project(project: &str) -> SparkOutput {
    let project_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/projects")
        .join(project)
        .join("main.spark");

    assert!(
        project_path.exists(),
        "project main not found: {}",
        project_path.display()
    );

    let spark_bin = env!("CARGO_BIN_EXE_spark");

    let output = Command::new(spark_bin)
        .arg("run")
        .arg(&project_path)
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

/// Runs `spark run <fixture>` with piped stdin and captures output.
///
/// Writes `stdin_input` to the child's stdin, then waits up to 5 seconds for
/// the process to exit. Panics on timeout to prevent CI deadlocks.
fn run_spark_with_stdin(fixture: &str, stdin_input: &str) -> SparkOutput {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);

    assert!(
        fixture_path.exists(),
        "fixture not found: {}",
        fixture_path.display()
    );

    let spark_bin = env!("CARGO_BIN_EXE_spark");

    let mut child = Command::new(spark_bin)
        .arg("run")
        .arg(&fixture_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn spark binary");

    // Write stdin and drop handle to signal EOF.
    {
        use std::io::Write;
        let stdin_handle = child.stdin.take().expect("failed to open stdin");
        let mut writer = std::io::BufWriter::new(stdin_handle);
        writer
            .write_all(stdin_input.as_bytes())
            .expect("failed to write to stdin");
        // Handle dropped here, closing the pipe.
    }

    // Wait with a 5-second timeout.
    let timeout = Duration::from_secs(5);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child.wait_with_output().expect("failed to read output");
                return SparkOutput {
                    stdout: String::from_utf8_lossy(&output.stdout).into(),
                    stderr: String::from_utf8_lossy(&output.stderr).into(),
                    exit_code: status.code().unwrap_or(-1),
                };
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    panic!("spark process timed out after {timeout:?} for fixture {fixture}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("error waiting for spark process: {e}"),
        }
    }
}

/// Runs a fixture with piped stdin and asserts stdout matches expected output.
fn assert_output_with_stdin(fixture: &str, stdin_input: &str, expected_stdout: &str) {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let result = run_spark_with_stdin(fixture, stdin_input);
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

#[test]
fn list_range() {
    assert_output("list_range.spark", "5\n6\n5\n11\n5");
}

#[test]
fn list_range_each() {
    assert_output("list_range_each.spark", "0\n1\n2\n3\n4");
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

#[test]
fn closure_snapshot() {
    assert_output("closure_snapshot.spark", "10\n42");
}

#[test]
fn closure_capture_string() {
    assert_output("closure_capture_string.spark", "hello");
}

#[test]
fn closure_capture_list() {
    assert_output("closure_capture_list.spark", "3");
}

#[test]
fn closure_capture_closure() {
    assert_output("closure_capture_closure.spark", "42");
}

// ============================================================================
// Each / Each Via
// ============================================================================

#[test]
fn each_print() {
    assert_output("each_print.spark", "1\n2\n3");
}

#[test]
fn each_via_double() {
    assert_output("each_via_double.spark", "3");
}

#[test]
fn each_fn_ref() {
    assert_output("each_fn_ref.spark", "1\n2\n3");
}

#[test]
fn each_via_fn_ref() {
    assert_output("each_via_fn_ref.spark", "2\n4\n6");
}

#[test]
fn each_cow() {
    assert_output("each_cow.spark", "0");
}

#[test]
fn each_lambda_print() {
    assert_output("each_lambda_print.spark", "10\n20\n30");
}

#[test]
fn each_via_lambda_values() {
    assert_output("each_via_lambda_values.spark", "20\n40\n60");
}

#[test]
fn lambda_implicit_return() {
    assert_output("lambda_return.spark", "10");
}

// ============================================================================
// Parallel Each / Parallel Each Via
// ============================================================================

#[test]
fn parallel_each_via_double() {
    assert_output("parallel_each_via_double.spark", "2\n4\n6");
}

#[test]
fn parallel_each_via_fn_ref() {
    assert_output("parallel_each_via_fn_ref.spark", "2\n4\n6");
}

#[test]
fn parallel_each_via_closure() {
    assert_output("parallel_each_via_closure.spark", "10\n20\n30");
}

#[test]
fn parallel_each_via_len() {
    assert_output("parallel_each_via_len.spark", "5");
}

#[test]
fn parallel_each_cow() {
    assert_output("parallel_each_cow.spark", "0");
}

#[test]
fn parallel_each_single() {
    assert_output("parallel_each_single.spark", "42");
}

#[test]
fn parallel_each_nonlist_error() {
    assert_compile_error("parallel_each_nonlist_error.spark", "requires a List");
}

#[test]
fn parallel_each_ref_mutation_error() {
    assert_compile_error("parallel_each_ref_mutation.spark", "not thread-safe");
}

// ============================================================================
// Via (pipeline operator)
// ============================================================================

#[test]
fn via_sort() {
    assert_output("via_sort.spark", "1\n2\n3");
}

#[test]
fn via_take() {
    assert_output("via_take.spark", "3");
}

#[test]
fn via_reverse() {
    assert_output("via_reverse.spark", "3\n2\n1");
}

#[test]
fn via_chain() {
    assert_output("via_chain.spark", "1\n2\n3");
}

#[test]
fn via_user_fn() {
    assert_output("via_user_fn.spark", "11\n12\n13");
}

// ============================================================================
// Integration (multi-feature)
// ============================================================================

#[test]
fn kitchen_sink() {
    assert_output("kitchen_sink.spark", "5\n4\n3\n2\n1\n9\n18\n27\n3\n13\n3");
}

// ============================================================================
// Async / Await / Spawn
// ============================================================================

#[test]
fn async_basic() {
    assert_output("async_basic.spark", "42");
}

#[test]
fn async_args() {
    assert_output("async_args.spark", "10");
}

#[test]
fn async_spawn() {
    assert_output("async_spawn.spark", "42");
}

#[test]
fn async_void() {
    assert_output("async_void.spark", "hello");
}

#[test]
fn await_non_future_error() {
    assert_compile_error("await_non_future_error.spark", "cannot `await`");
}

#[test]
fn spawn_non_async_error() {
    assert_compile_error("spawn_non_async_error.spark", "cannot `spawn`");
}

#[test]
fn spawn_basic() {
    assert_output("spawn_basic.spark", "42");
}

#[test]
fn spawn_parallel() {
    assert_output("spawn_parallel.spark", "10\n15");
}

#[test]
fn spawn_closure() {
    assert_output("spawn_closure.spark", "123");
}

#[test]
fn spawn_mutable_error() {
    assert_compile_error("spawn_mutable_error.spark", "mutable binding");
}

#[test]
fn spawn_ref_error() {
    assert_compile_error("spawn_ref_error.spark", "not thread-safe");
}

// ============================================================================
// Cooperative async (Phase 3)
// ============================================================================

#[test]
fn coop_basic() {
    assert_output("coop_basic.spark", "42");
}

#[test]
fn coop_chain() {
    assert_output("coop_chain.spark", "15");
}

#[test]
fn coop_parallel() {
    assert_output("coop_parallel.spark", "30");
}

// Standard library
// ============================================================================

#[test]
fn stdlib_math() {
    assert_output("stdlib_math.spark", "4\n1024\n5\n3\n4\n1\n2\n4");
}

#[test]
fn stdlib_json_parse() {
    assert_output("stdlib_json_parse.spark", "Alice\n30");
}

#[test]
fn stdlib_json_stringify() {
    assert_output(
        "stdlib_json_stringify.spark",
        "{\"greeting\":\"hello\",\"count\":42}",
    );
}

#[test]
fn stdlib_json_error() {
    assert_output("stdlib_json_error.spark", "got error");
}

#[test]
fn stdlib_json_array() {
    assert_output("stdlib_json_array.spark", "3\n10\n30");
}

#[test]
fn stdlib_json_nested() {
    assert_output("stdlib_json_nested.spark", "2\nfirst\nsecond");
}

#[test]
fn stdlib_io_write_read() {
    assert_output("stdlib_io_write_read.spark", "hello from spark");
}

#[test]
fn stdlib_io_exists() {
    assert_output("stdlib_io_exists.spark", "false\ntrue");
}

#[test]
fn stdlib_io_error() {
    assert_output("stdlib_io_error.spark", "got error");
}

#[test]
fn stdlib_io_dir() {
    // Clean up from any previous failed run
    let _ = std::fs::remove_dir_all("/tmp/spark_e2e_io_dir_test");
    assert_output("stdlib_io_dir.spark", "a.txt\nb.txt");
    // Clean up the directory after test
    let _ = std::fs::remove_dir_all("/tmp/spark_e2e_io_dir_test");
}

// Multi-file compilation
// ============================================================================
#[test]
fn ref_param() {
    assert_output("ref_param.spark", "20");
}

#[test]
fn ref_local() {
    assert_output("ref_local.spark", "42");
}

#[test]
fn ref_literal() {
    assert_output("ref_literal.spark", "11");
}

#[test]
fn ref_closure() {
    assert_output("ref_closure.spark", "42");
}

#[test]
fn multifile_import() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let result = run_spark_project("multifile");
    assert_eq!(
        result.exit_code, 0,
        "multifile project exited with code {}:\n{}",
        result.exit_code, result.stderr
    );
    assert_eq!(
        result.stdout.trim(),
        "Hello from module!",
        "stdout mismatch for multifile project"
    );
}

#[test]
fn multimod_using_import() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let result = run_spark_project("multimod");
    assert_eq!(
        result.exit_code, 0,
        "multimod project exited with code {}:\n{}",
        result.exit_code, result.stderr
    );
    assert_eq!(
        result.stdout.trim(),
        "hello\nhelper",
        "stdout mismatch for multimod project"
    );
}

// ============================================================================
// Stdin / Stderr I/O
// ============================================================================

#[test]
fn async_stdin() {
    assert_output_with_stdin("async_stdin.spark", "hello\n", "42\nhello");
}

#[test]
fn io_stderr() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let result = run_spark("io_stderr.spark");
    assert_eq!(
        result.exit_code, 0,
        "fixture io_stderr.spark exited with code {}:\n{}",
        result.exit_code, result.stderr
    );
    assert_eq!(
        result.stdout.trim(),
        "normal output",
        "stdout mismatch for io_stderr.spark"
    );
    assert!(
        result.stderr.contains("error output"),
        "stderr should contain 'error output', got:\n{}",
        result.stderr
    );
}

// ── Channel tests ───────────────────────────────────────────────────────

#[test]
fn channel_basic() {
    assert_output("channel_basic.spark", "42");
}

#[test]
fn channel_try_recv() {
    assert_output("channel_try_recv.spark", "88");
}

#[test]
fn channel_async() {
    assert_output("channel_async.spark", "30");
}

// ── Generic constraint tests ────────────────────────────────────────────

#[test]
fn generic_constraint_interface() {
    assert_output("generic_constraint_interface.spark", "ok");
}

#[test]
fn generic_constraint_structural() {
    assert_output("generic_constraint_structural.spark", "1");
}

#[test]
fn generic_constraint_fail() {
    assert_compile_error("generic_constraint_fail.spark", "does not satisfy constraint");
}

#[test]
fn generic_constraint_structural_fail() {
    assert_compile_error("generic_constraint_structural_fail.spark", "missing required field");
}
