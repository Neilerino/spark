//! End-to-end tests for the Spark compiler.
//!
//! Each test compiles and runs a `.spark` fixture file via the `spark` binary,
//! then asserts against stdout, stderr, and exit code. This provides full-pipeline
//! regression coverage from source code to runtime output.
//!
//! Tests run in parallel — each `spark run` invocation uses a per-process temp
//! directory for its compiled executable, so concurrent tests don't collide.
//!
//! ## Test layout
//!
//! ```text
//! tests/
//! ├── run-pass/       # Programs that should compile and produce expected output
//! │   ├── basics/     # hello, arithmetic, if_else, structs, loops
//! │   ├── collections/# list_*, nested_*
//! │   ├── generics/   # generic_struct_init, generic_identity, constraints
//! │   ├── lambdas/    # lambda_*, fn_reference, higher_order, closure_*
//! │   ├── each/       # each_*, map_*
//! │   ├── parallel/   # parallel_each_*
//! │   ├── to/         # to_*
//! │   ├── async/      # async_*, spawn_*, coop_*
//! │   ├── references/ # ref_*
//! │   ├── stdlib/     # stdlib_*
//! │   ├── channels/   # channel_*
//! │   ├── type_narrowing/ # to_*, propagate_*
//! │   ├── protocols/  # displayable, equatable, comparable, iterable
//! │   ├── io/         # io_stderr
//! │   └── multifile/  # multi-file project dirs
//! ├── compile-fail/   # Programs that should fail compilation
//! │   ├── type_errors/
//! │   └── borrow_errors/
//! └── ui/             # Error message snapshots (future use)
//! ```

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Captured output from running a Spark program.
struct SparkOutput {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

/// Runs `spark run <fixture>` and captures output.
///
/// For single-file tests, pass a relative path like `"run-pass/basics/hello.spark"`.
/// For multi-file project tests, use [`run_spark_project`] instead.
fn run_spark(fixture: &str) -> SparkOutput {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(fixture);

    assert!(
        fixture_path.exists(),
        "fixture not found: {}",
        fixture_path.display()
    );

    let spark_bin = env!("CARGO_BIN_EXE_spark");

    // Set SPARK_RUNTIME_DIR so the driver finds the correct libspark_runtime.a
    // in the same directory as the spark binary (avoids stale copies in worktrees).
    let runtime_dir = Path::new(spark_bin)
        .parent()
        .expect("spark binary has no parent dir");

    let output = Command::new(spark_bin)
        .arg("run")
        .arg(&fixture_path)
        .env("SPARK_RUNTIME_DIR", runtime_dir)
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
/// Projects live under `tests/run-pass/multifile/<name>/` and are self-contained
/// directories with their own module structure.
fn run_spark_project(project: &str) -> SparkOutput {
    let project_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/run-pass/multifile")
        .join(project)
        .join("main.spark");

    assert!(
        project_path.exists(),
        "project main not found: {}",
        project_path.display()
    );

    let spark_bin = env!("CARGO_BIN_EXE_spark");

    let runtime_dir = Path::new(spark_bin)
        .parent()
        .expect("spark binary has no parent dir");

    let output = Command::new(spark_bin)
        .arg("run")
        .arg(&project_path)
        .env("SPARK_RUNTIME_DIR", runtime_dir)
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
        .join("tests")
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

/// Runs `spark run <fixture>` with extra environment variables set.
fn run_spark_with_env(fixture: &str, env: &[(&str, &str)]) -> SparkOutput {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(fixture);

    assert!(
        fixture_path.exists(),
        "fixture not found: {}",
        fixture_path.display()
    );

    let spark_bin = env!("CARGO_BIN_EXE_spark");

    let runtime_dir = Path::new(spark_bin)
        .parent()
        .expect("spark binary has no parent dir");

    let mut cmd = Command::new(spark_bin);
    cmd.arg("run")
        .arg(&fixture_path)
        .env("SPARK_RUNTIME_DIR", runtime_dir);

    for (k, v) in env {
        cmd.env(k, v);
    }

    let output = cmd.output().expect("failed to run spark binary");

    SparkOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into(),
        stderr: String::from_utf8_lossy(&output.stderr).into(),
        exit_code: output.status.code().unwrap_or(-1),
    }
}

// ============================================================================
// Basic functionality
// ============================================================================

#[test]
fn hello_world() {
    assert_output("run-pass/basics/hello.spark", "Hello, Spark!");
}

#[test]
fn arithmetic() {
    assert_output("run-pass/basics/arithmetic.spark", "15\n5\n50\n2");
}

#[test]
fn if_else() {
    assert_output("run-pass/basics/if_else.spark", "bigger");
}

#[test]
fn to_bool() {
    assert_output("run-pass/basics/to_bool.spark", "yes");
}

#[test]
fn to_string() {
    assert_output("run-pass/basics/to_string.spark", "correct");
}

#[test]
fn to_float() {
    assert_output("run-pass/basics/to_float.spark", "correct");
}

#[test]
fn to_int() {
    assert_output("run-pass/basics/to_int.spark", "correct");
}

#[test]
fn struct_field_access() {
    assert_output("run-pass/basics/struct_field.spark", "10\n20");
}

#[test]
fn callable_field() {
    assert_output("run-pass/basics/callable_field.spark", "10");
}

#[test]
fn string_interp_struct_field() {
    assert_output("run-pass/basics/string_interp_field.spark", "x=10\nname=origin");
}

#[test]
fn tuple_field_access() {
    assert_output("run-pass/basics/tuple_field_access.spark", "10\n20\nalice\n30\ntrue");
}

#[test]
fn struct_param() {
    assert_output("run-pass/basics/struct_param.spark", "25");
}

#[test]
fn struct_field_order() {
    assert_output("run-pass/basics/struct_field_order.spark", "1\n2\n3");
}

#[test]
fn loop_break() {
    assert_output("run-pass/basics/loop_break.spark", "4");
}

#[test]
fn kitchen_sink() {
    assert_output("run-pass/basics/kitchen_sink.spark", "5\n4\n3\n2\n1\n9\n18\n27\n3\n13\n3");
}

#[test]
fn string_len() {
    assert_output("run-pass/basics/string_len.spark", "5");
}

// ============================================================================
// Collection methods
// ============================================================================

#[test]
fn list_len() {
    assert_output("run-pass/collections/list_len.spark", "3");
}

#[test]
fn list_append() {
    assert_output("run-pass/collections/list_append.spark", "3");
}

#[test]
fn list_contains() {
    assert_output("run-pass/collections/list_contains.spark", "true\nfalse");
}

#[test]
fn nested_list_string() {
    assert_output("run-pass/collections/nested_list_string.spark", "2");
}

#[test]
fn nested_map_string_list() {
    assert_output("run-pass/collections/nested_map_string_list.spark", "2");
}

#[test]
fn nested_list_mutate() {
    assert_output("run-pass/collections/nested_list_mutate.spark", "2");
}

#[test]
fn map_string_keys() {
    assert_output(
        "run-pass/collections/map_string_keys.spark",
        "3\ntrue\n2\nfalse",
    );
}

#[test]
fn nested_list_deep() {
    assert_output("run-pass/collections/nested_list_deep.spark", "1");
}

#[test]
fn list_range() {
    assert_output("run-pass/collections/list_range.spark", "5\n6\n5\n11\n5");
}

#[test]
fn list_range_each() {
    assert_output("run-pass/collections/list_range_each.spark", "0\n1\n2\n3\n4");
}

#[test]
fn map_insert_get() {
    assert_output("run-pass/collections/map_insert_get.spark", "3");
}

#[test]
fn map_multiple_sizes() {
    assert_output("run-pass/collections/map_overwrite.spark", "1\n3");
}

#[test]
fn map_get_optional() {
    assert_output("run-pass/collections/map_get_optional.spark", "found");
}

#[test]
fn map_get_optional_list() {
    assert_output("run-pass/collections/map_get_optional_list.spark", "found\na");
}

#[test]
fn list_index_optional() {
    assert_output("run-pass/collections/list_index_optional.spark", "found");
}

#[test]
fn map_get_missing_key() {
    assert_output(
        "run-pass/collections/map_get_missing_key.spark",
        "x: found\nnope: null",
    );
}

// ============================================================================
// Generics
// ============================================================================

#[test]
fn generic_struct_init() {
    assert_output("run-pass/generics/generic_struct_init.spark", "3\n4");
}

#[test]
fn generic_identity() {
    assert_output("run-pass/generics/generic_identity.spark", "42");
}

#[test]
fn generic_constraint_interface() {
    assert_output("run-pass/generics/generic_constraint_interface.spark", "ok");
}

#[test]
fn generic_constraint_structural() {
    assert_output("run-pass/generics/generic_constraint_structural.spark", "1");
}

// ============================================================================
// Lambdas & function references
// ============================================================================

#[test]
fn lambda_call() {
    assert_output("run-pass/lambdas/lambda_call.spark", "10");
}

#[test]
fn lambda_implicit_return() {
    assert_output("run-pass/lambdas/lambda_return.spark", "10");
}

#[test]
fn fn_reference() {
    assert_output("run-pass/lambdas/fn_reference.spark", "14");
}

#[test]
fn higher_order() {
    assert_output("run-pass/lambdas/higher_order.spark", "15");
}

#[test]
fn closure_capture() {
    assert_output("run-pass/lambdas/closure_capture.spark", "15");
}

#[test]
fn closure_multi_capture() {
    assert_output("run-pass/lambdas/closure_multi_capture.spark", "20");
}

#[test]
fn closure_snapshot() {
    assert_output("run-pass/lambdas/closure_snapshot.spark", "10\n42");
}

#[test]
fn closure_capture_string() {
    assert_output("run-pass/lambdas/closure_capture_string.spark", "hello");
}

#[test]
fn closure_capture_list() {
    assert_output("run-pass/lambdas/closure_capture_list.spark", "3");
}

#[test]
fn closure_capture_mutable_list() {
    assert_output("run-pass/lambdas/closure_capture_mutable_list.spark", "3");
}

#[test]
fn closure_capture_closure() {
    assert_output("run-pass/lambdas/closure_capture_closure.spark", "42");
}

#[test]
fn to_inside_each() {
    assert_output("run-pass/lambdas/to_inside_each.spark", "ok\nok\nok");
}

// ============================================================================
// Each / Each Via
// ============================================================================

#[test]
fn each_print() {
    assert_output("run-pass/each/each_print.spark", "1\n2\n3");
}

#[test]
fn each_fn_ref() {
    assert_output("run-pass/each/each_fn_ref.spark", "1\n2\n3");
}

#[test]
fn each_lambda_print() {
    assert_output("run-pass/each/each_lambda_print.spark", "10\n20\n30");
}

#[test]
fn map_double() {
    assert_output("run-pass/each/map_double.spark", "3");
}

#[test]
fn map_fn_ref() {
    assert_output("run-pass/each/map_fn_ref.spark", "2\n4\n6");
}

#[test]
fn map_partial() {
    assert_output("run-pass/each/map_partial.spark", "11\n12\n13");
}

#[test]
fn standalone_partial() {
    assert_output("run-pass/partial/standalone_partial.spark", "15\n30\n50\n100");
}

#[test]
fn map_lambda_values() {
    assert_output("run-pass/each/map_lambda_values.spark", "20\n40\n60");
}

#[test]
fn each_cow() {
    assert_output("run-pass/each/each_cow.spark", "0");
}

#[test]
fn filter_lambda() {
    assert_output("run-pass/each/filter_lambda.spark", "3\n4\n5");
}

#[test]
fn filter_fn_ref() {
    assert_output("run-pass/each/filter_fn_ref.spark", "2\n4\n6");
}

#[test]
fn find_lambda() {
    assert_output("run-pass/each/find_lambda.spark", "found");
}

#[test]
fn find_not_found() {
    assert_output("run-pass/each/find_not_found.spark", "not found");
}

// ============================================================================
// Parallel Each / Parallel Map
// ============================================================================

#[test]
fn parallel_map_double() {
    assert_output("run-pass/parallel/parallel_map_double.spark", "2\n4\n6");
}

#[test]
fn parallel_map_fn_ref() {
    assert_output("run-pass/parallel/parallel_map_fn_ref.spark", "2\n4\n6");
}

#[test]
fn parallel_map_closure() {
    assert_output("run-pass/parallel/parallel_map_closure.spark", "10\n20\n30");
}

#[test]
fn parallel_map_len() {
    assert_output("run-pass/parallel/parallel_map_len.spark", "5");
}

#[test]
fn parallel_each_cow() {
    assert_output("run-pass/parallel/parallel_each_cow.spark", "0");
}

#[test]
fn parallel_each_single() {
    assert_output("run-pass/parallel/parallel_each_single.spark", "42");
}

// ============================================================================
// To (pipeline operator)
// ============================================================================

#[test]
fn to_sort() {
    assert_output("run-pass/to/to_sort.spark", "1\n2\n3");
}

#[test]
fn to_sort_float() {
    assert_output("run-pass/to/to_sort_float.spark", "-4\n-1.2\n0\n2.7\n3.5");
}

#[test]
fn to_sort_string() {
    assert_output("run-pass/to/to_sort_string.spark", "apple\nbanana\ncherry");
}

#[test]
fn to_sort_bool() {
    assert_output("run-pass/to/to_sort_bool.spark", "false\nfalse\ntrue\ntrue");
}

#[test]
fn to_take() {
    assert_output("run-pass/to/to_take.spark", "3");
}

#[test]
fn to_reverse() {
    assert_output("run-pass/to/to_reverse.spark", "3\n2\n1");
}

#[test]
fn to_chain() {
    assert_output("run-pass/to/to_chain.spark", "1\n2\n3");
}

#[test]
fn to_user_fn() {
    assert_output("run-pass/to/to_user_fn.spark", "11\n12\n13");
}

#[test]
fn to_reverse_string() {
    assert_output("run-pass/to/to_reverse_string.spark", "charlie\nbob\nalice");
}

#[test]
fn to_take_string() {
    assert_output("run-pass/to/to_take_string.spark", "2");
}

#[test]
fn to_union_inside_each() {
    assert_output("run-pass/to/to_union_inside_each.spark", "test\nhello");
}

#[test]
fn to_ref_union() {
    assert_output("run-pass/to/to_ref_union.spark", "ok");
}

#[test]
fn to_ref_union_inside_each() {
    assert_output("run-pass/to/to_ref_union_inside_each.spark", "test\nok");
}

#[test]
fn to_union_all_handled() {
    assert_output(
        "run-pass/to/to_union_all_handled.spark",
        "got int\ngot string",
    );
}

#[test]
fn to_union_assigned_local() {
    assert_output(
        "run-pass/to/to_union_assigned_local.spark",
        "got int\ngot string",
    );
}

#[test]
fn to_union_bare_type() {
    assert_output(
        "run-pass/to/to_union_bare_type.spark",
        "got int\ngot string",
    );
}

// ============================================================================
// Pipeline (complex multi-stage)
// ============================================================================

#[test]
fn pipeline_complex() {
    assert_output(
        "run-pass/pipeline/pipeline_complex.spark",
        "4\n534\n109\n125\n150\n150\n534\n184\n4\n-1\n35\n1001\n1002\n1003\n4665600",
    );
}

#[test]
fn pipeline_filter_find() {
    assert_output(
        "run-pass/pipeline/pipeline_filter_find.spark",
        "4\n8\n12\n16\n20\nfound\n30",
    );
}

// ============================================================================
// Async / Await / Spawn
// ============================================================================

#[test]
fn async_basic() {
    assert_output("run-pass/async/async_basic.spark", "42");
}

#[test]
fn async_args() {
    assert_output("run-pass/async/async_args.spark", "10");
}

#[test]
fn async_spawn() {
    assert_output("run-pass/async/async_spawn.spark", "42");
}

#[test]
fn async_void() {
    assert_output("run-pass/async/async_void.spark", "hello");
}

#[test]
fn async_stdin() {
    assert_output_with_stdin("run-pass/async/async_stdin.spark", "hello\n", "42\nhello");
}

#[test]
fn spawn_basic() {
    assert_output("run-pass/async/spawn_basic.spark", "42");
}

#[test]
fn spawn_parallel() {
    assert_output("run-pass/async/spawn_parallel.spark", "10\n15");
}

#[test]
fn spawn_closure() {
    assert_output("run-pass/async/spawn_closure.spark", "123");
}

// ============================================================================
// Cooperative async (Phase 3)
// ============================================================================

#[test]
fn coop_basic() {
    assert_output("run-pass/async/coop_basic.spark", "42");
}

#[test]
fn coop_chain() {
    assert_output("run-pass/async/coop_chain.spark", "15");
}

#[test]
fn coop_parallel() {
    assert_output("run-pass/async/coop_parallel.spark", "30");
}

// ============================================================================
// References
// ============================================================================

#[test]
fn ref_param() {
    assert_output("run-pass/references/ref_param.spark", "20");
}

#[test]
fn ref_local() {
    assert_output("run-pass/references/ref_local.spark", "42");
}

#[test]
fn ref_literal() {
    assert_output("run-pass/references/ref_literal.spark", "11");
}

#[test]
fn ref_mutparam() {
    assert_output("run-pass/references/ref_mutparam.spark", "10");
}

#[test]
fn ref_closure() {
    assert_output("run-pass/references/ref_closure.spark", "42");
}

#[test]
fn ref_list_coerce() {
    assert_output("run-pass/references/ref_list_coerce.spark", "2");
}

// ============================================================================
// Standard library
// ============================================================================

#[test]
fn stdlib_math() {
    assert_output("run-pass/stdlib/stdlib_math.spark", "4\n1024\n5\n3\n4\n1\n2\n4");
}

#[test]
fn stdlib_json_parse() {
    assert_output("run-pass/stdlib/stdlib_json_parse.spark", "Alice\n30");
}

#[test]
fn stdlib_json_stringify() {
    assert_output(
        "run-pass/stdlib/stdlib_json_stringify.spark",
        "{\"greeting\":\"hello\",\"count\":42}",
    );
}

#[test]
fn stdlib_json_error() {
    assert_output("run-pass/stdlib/stdlib_json_error.spark", "got error");
}

#[test]
fn stdlib_json_array() {
    assert_output("run-pass/stdlib/stdlib_json_array.spark", "3\n10\n30");
}

#[test]
fn stdlib_json_nested() {
    assert_output("run-pass/stdlib/stdlib_json_nested.spark", "2\nfirst\nsecond");
}

#[test]
fn stdlib_io_write_read() {
    assert_output("run-pass/stdlib/stdlib_io_write_read.spark", "hello from spark");
}

#[test]
fn stdlib_io_exists() {
    assert_output("run-pass/stdlib/stdlib_io_exists.spark", "false\ntrue");
}

#[test]
fn stdlib_io_error() {
    assert_output("run-pass/stdlib/stdlib_io_error.spark", "got error");
}

#[test]
fn stdlib_io_dir() {
    // Clean up from any previous failed run
    let _ = std::fs::remove_dir_all("/tmp/spark_e2e_io_dir_test");
    assert_output("run-pass/stdlib/stdlib_io_dir.spark", "a.txt\nb.txt");
    // Clean up the directory after test
    let _ = std::fs::remove_dir_all("/tmp/spark_e2e_io_dir_test");
}

#[test]
fn stdlib_sys_args() {
    // args() returns at least 1 element (the program name)
    let result = run_spark("run-pass/stdlib/stdlib_sys_args.spark");
    assert_eq!(
        result.exit_code, 0,
        "stdlib_sys_args exited with code {}:\n{}",
        result.exit_code, result.stderr
    );
    // The output should be "1" (just the program name, no extra args)
    let count: i64 = result.stdout.trim().parse().expect("expected integer output");
    assert!(count >= 1, "args() should return at least the program name, got {count}");
}

#[test]
fn stdlib_sys_env() {
    let result = run_spark_with_env(
        "run-pass/stdlib/stdlib_sys_env.spark",
        &[("SPARK_TEST_ENV_VAR", "test_value_123")],
    );
    assert_eq!(
        result.exit_code, 0,
        "stdlib_sys_env exited with code {}:\n{}",
        result.exit_code, result.stderr
    );
    assert_eq!(
        result.stdout.trim(),
        "test_value_123\ntrue\ndynamic_value\nfalse",
        "stdout mismatch for stdlib_sys_env"
    );
}

// ============================================================================
// Channels
// ============================================================================

#[test]
fn channel_basic() {
    assert_output("run-pass/channels/channel_basic.spark", "42");
}

#[test]
fn channel_try_recv() {
    assert_output("run-pass/channels/channel_try_recv.spark", "88");
}

#[test]
fn channel_async() {
    assert_output("run-pass/channels/channel_async.spark", "30");
}

// ============================================================================
// Type narrowing (to keyword, error propagation)
// ============================================================================

#[test]
fn to_pipeline() {
    assert_output("run-pass/type_narrowing/to_pipeline.spark", "10\n16");
}

#[test]
fn to_narrowing() {
    assert_output("run-pass/type_narrowing/to_narrowing.spark", "7");
}

#[test]
fn to_narrowing_error() {
    assert_output("run-pass/type_narrowing/to_narrowing_error.spark", "-1");
}

#[test]
fn propagate_union() {
    assert_output("run-pass/type_narrowing/propagate_union.spark", "5");
}

#[test]
fn propagate_union_error() {
    assert_output("run-pass/type_narrowing/propagate_union_error.spark", "error");
}

#[test]
fn propagate_optional() {
    assert_output("run-pass/type_narrowing/propagate_optional.spark", "42");
}

#[test]
fn propagate_optional_null() {
    assert_output("run-pass/type_narrowing/propagate_optional_null.spark", "0");
}

#[test]
fn is_null_continue() {
    assert_output("run-pass/type_narrowing/is_null_continue.spark", "9");
}

#[test]
fn is_null_return() {
    assert_output("run-pass/type_narrowing/is_null_return.spark", "5\n0");
}

#[test]
fn is_positive_narrowing() {
    assert_output("run-pass/type_narrowing/is_positive.spark", "42");
}

#[test]
fn mutable_optional_narrowing() {
    assert_output("run-pass/type_narrowing/mutable_optional_narrowing.spark", "10");
}

#[test]
fn optional_reassign() {
    assert_output("run-pass/type_narrowing/optional_reassign.spark", "found");
}

#[test]
fn is_union() {
    assert_output("run-pass/type_narrowing/is_union.spark", "int\nstring");
}

#[test]
fn is_union_else() {
    assert_output("run-pass/type_narrowing/is_union_else.spark", "int\nother");
}

// ============================================================================
// Stdin / Stderr I/O
// ============================================================================

#[test]
fn io_stderr() {

    let result = run_spark("run-pass/io/io_stderr.spark");
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

// ============================================================================
// Multi-file compilation
// ============================================================================

#[test]
fn multifile_import() {

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
// Standard protocol interfaces
// ============================================================================

#[test]
fn displayable_print() {
    assert_output("run-pass/protocols/displayable_print.spark", "hello");
}

#[test]
fn equatable() {
    assert_output("run-pass/protocols/equatable.spark", "true\ntrue");
}

#[test]
fn comparable() {
    assert_output("run-pass/protocols/comparable.spark", "true\ntrue\ntrue\nfalse");
}

#[test]
fn iterable_each() {
    assert_output("run-pass/protocols/iterable.spark", "10\n20");
}

// ============================================================================
// Compile-fail: type errors
// ============================================================================

#[test]
fn immutable_append_error() {
    assert_compile_error("compile-fail/type_errors/immutable_append.spark", "not mutable");
}

#[test]
fn to_nonexhaustive_bool() {
    assert_compile_error("compile-fail/type_errors/to_nonexhaustive_bool.spark", "non-exhaustive");
}

#[test]
fn to_nonexhaustive_int() {
    assert_compile_error("compile-fail/type_errors/to_nonexhaustive_int.spark", "non-exhaustive");
}

#[test]
fn await_non_future_error() {
    assert_compile_error("compile-fail/type_errors/await_non_future_error.spark", "cannot `await`");
}

#[test]
fn spawn_non_async_error() {
    assert_compile_error("compile-fail/type_errors/spawn_non_async_error.spark", "cannot `spawn`");
}

#[test]
fn spawn_mutable_error() {
    assert_compile_error("compile-fail/type_errors/spawn_mutable_error.spark", "mutable binding");
}

#[test]
fn spawn_ref_error() {
    assert_compile_error("compile-fail/type_errors/spawn_ref_error.spark", "not thread-safe");
}

#[test]
fn parallel_each_nonlist_error() {
    assert_compile_error("compile-fail/type_errors/parallel_each_nonlist_error.spark", "requires a List");
}

#[test]
fn parallel_each_ref_mutation_error() {
    assert_compile_error("compile-fail/type_errors/parallel_each_ref_mutation.spark", "not thread-safe");
}

#[test]
fn generic_constraint_fail() {
    assert_compile_error("compile-fail/type_errors/generic_constraint_fail.spark", "does not satisfy constraint");
}

#[test]
fn generic_constraint_structural_fail() {
    assert_compile_error("compile-fail/type_errors/generic_constraint_structural_fail.spark", "missing required field");
}

#[test]
fn to_non_union_error() {
    assert_compile_error("compile-fail/type_errors/to_non_union_error.spark", "non-exhaustive");
}

#[test]
fn immutable_field_assign() {
    assert_compile_error("compile-fail/type_errors/immutable_field_assign.spark", "immutable");
}

#[test]
fn immutable_mutable_borrow() {
    assert_compile_error("compile-fail/type_errors/immutable_mutable_borrow.spark", "cannot pass immutable value as mutable reference");
}

#[test]
fn propagate_return_mismatch() {
    assert_compile_error("compile-fail/type_errors/propagate_return_mismatch.spark", "does not include `Null`");
}

#[test]
fn propagate_union_return_mismatch() {
    assert_compile_error("compile-fail/type_errors/propagate_union_return_mismatch.spark", "does not include it");
}

#[test]
fn tuple_field_oob() {
    assert_compile_error("compile-fail/type_errors/tuple_field_oob.spark", "out of range");
}

#[test]
fn list_string_index_error() {
    assert_compile_error(
        "compile-fail/type_errors/list_string_index.spark",
        "list index must be an integer",
    );
}

#[test]
fn continue_in_closure_error() {
    assert_compile_error(
        "compile-fail/type_errors/continue_in_closure.spark",
        "not inside a loop",
    );
}

#[test]
fn interface_missing_method() {
    assert_compile_error("compile-fail/type_errors/interface_missing_method.spark", "missing required method");
}

#[test]
fn interface_signature_mismatch() {
    assert_compile_error("compile-fail/type_errors/interface_signature_mismatch.spark", "wrong number of parameters");
}

// ============================================================================
// Compile-fail: borrow errors
// ============================================================================

#[test]
fn borrow_double_mut() {
    assert_compile_error("compile-fail/borrow_errors/borrow_double_mut.spark", "cannot borrow value as mutable more than once");
}

#[test]
fn borrow_mut_immut() {
    assert_compile_error("compile-fail/borrow_errors/borrow_mut_immut.spark", "cannot borrow value as mutable because it is also borrowed as immutable");
}

#[test]
fn borrow_immut_assign() {
    assert_compile_error("compile-fail/borrow_errors/borrow_immut_assign.spark", "cannot assign to immutable variable");
}

#[test]
fn borrow_dangling_return() {
    assert_compile_error("compile-fail/borrow_errors/borrow_dangling_return.spark", "cannot return reference to local variable");
}

#[test]
fn borrow_alias_call() {
    assert_compile_error("compile-fail/borrow_errors/borrow_alias_call.spark", "cannot pass");
}

// ============================================================================
// FFI / link directive / CPtr
// ============================================================================

#[test]
fn path_dep_import() {
    let result = run_spark_project("path_dep");
    assert_eq!(
        result.exit_code, 0,
        "path_dep project exited with code {}:\n{}",
        result.exit_code, result.stderr
    );
    assert_eq!(
        result.stdout.trim(),
        "7\n30",
        "stdout mismatch for path_dep project"
    );
}

#[test]
fn link_libm() {
    assert_output("run-pass/ffi/link_libm.spark", "5");
}

#[test]
fn extern_cptr() {
    assert_output("run-pass/ffi/extern_cptr.spark", "ok");
}

#[test]
fn link_from_path() {
    assert_output("run-pass/ffi/link_from_path.spark", "4");
}

// ============================================================================
// Package management: init, transitive deps
// ============================================================================

#[test]
fn spark_init() {
    let tmp = tempfile::tempdir().unwrap();
    let spark_bin = env!("CARGO_BIN_EXE_spark");

    let output = Command::new(spark_bin)
        .args(["init", "--name", "test_project"])
        .current_dir(tmp.path())
        .output()
        .expect("failed to run spark init");

    assert!(
        output.status.success(),
        "spark init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let manifest_path = tmp.path().join("Spark.toml");
    assert!(manifest_path.exists(), "Spark.toml should be created");

    let contents = std::fs::read_to_string(&manifest_path).unwrap();
    assert!(contents.contains("test_project"), "manifest should contain project name");

    // Running init again should fail.
    let output2 = Command::new(spark_bin)
        .args(["init"])
        .current_dir(tmp.path())
        .output()
        .expect("failed to run spark init");

    assert!(
        !output2.status.success(),
        "spark init should fail when Spark.toml already exists"
    );
}

#[test]
fn transitive_path_deps() {
    let result = run_spark_project("transitive_deps");
    assert_eq!(
        result.exit_code, 0,
        "transitive_deps project exited with code {}:\n{}",
        result.exit_code, result.stderr
    );
    assert_eq!(
        result.stdout.trim(),
        "20",
        "stdout mismatch for transitive_deps project"
    );
}
