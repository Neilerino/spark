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
//! │   ├── parsing/    # target-directed text parsing
//! │   ├── io/         # io_stderr
//! │   └── multifile/  # multi-file project dirs
//! ├── run-fail/      # Programs that compile and terminate with a runtime diagnostic
//! ├── compile-fail/   # Programs that should fail compilation
//! │   ├── syntax_errors/
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
    run_spark_with_args(fixture, &[])
}

fn run_spark_with_args(fixture: &str, args: &[&str]) -> SparkOutput {
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
        .args(args)
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
    let runtime_dir = Path::new(spark_bin)
        .parent()
        .expect("spark binary has no parent dir");

    let mut child = Command::new(spark_bin)
        .arg("run")
        .arg(&fixture_path)
        .env("SPARK_RUNTIME_DIR", runtime_dir)
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
fn casts_reject_forged_pointers_and_changed_tags() {
    for fixture in [
        "compile-fail/type_errors/cast_forged_string.spark",
        "compile-fail/type_errors/cast_forged_tag.spark",
        "compile-fail/type_errors/cast_erased_tag.spark",
    ] {
        for args in [&[][..], &["--no-opt"][..]] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result.stderr.contains("unsupported cast"),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn casts_validate_specialized_types_and_preserve_borrows() {
    for (fixture, diagnostic) in [
        ("type_errors/cast_generic_source.spark", "unsupported cast"),
        ("type_errors/cast_generic_target.spark", "unsupported cast"),
        ("type_errors/cast_ref_strength.spark", "unsupported cast"),
        (
            "type_errors/cast_nested_ref_strength.spark",
            "unsupported cast",
        ),
        (
            "borrow_errors/cast_ref_escape.spark",
            "cannot return reference to local variable",
        ),
    ] {
        for args in [&[][..], &["--no-opt"][..]] {
            let result = run_spark_with_args(&format!("compile-fail/{fixture}"), args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result.stderr.contains(diagnostic),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn casts_support_generic_numeric_and_borrow_identity() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args("run-pass/generics/cast_specialized.spark", args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout.trim(), "4\n7\nretained\n3\n255", "{args:?}");
    }
}

#[test]
fn casts_preserve_numeric_width_signedness_and_representation() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args("run-pass/basics/numeric_casts.spark", args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(
            result.stdout.trim(),
            "1\n0\n-1\n255\n1\n-1\n255\n65535\n-1\n-2\n255\n7\n-7\n255\n0\n127\n-128\n255\n65535\n-32768\n250\n-2\n0\n65535\n-128\n0",
            "{args:?}"
        );
    }
}

#[test]
fn casts_only_emit_concrete_reachable_specializations() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, expected) in [
            ("cast_unused_template", "hello"),
            ("cast_nested_specialization", "3\n7\n9\n6\n8\n4\n42"),
        ] {
            let result = run_spark_with_args(&format!("run-pass/generics/{fixture}.spark"), args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout.trim(), expected, "{fixture} {args:?}");
        }
        let result = run_spark_with_args(
            "compile-fail/type_errors/cast_nested_generic_invalid.spark",
            args,
        );
        assert_eq!(result.exit_code, 1, "{args:?}: {}", result.stderr);
        assert!(
            result.stderr.contains("unsupported cast")
                && result.stderr.contains("invalid after specialization"),
            "{args:?}: {}",
            result.stderr
        );
        let result = run_spark_with_args(
            "compile-fail/type_errors/generic_expanding_recursion.spark",
            args,
        );
        assert_eq!(result.exit_code, 1, "{args:?}: {}", result.stderr);
        assert!(
            result.stderr.contains("type-changing generic recursion"),
            "{args:?}: {}",
            result.stderr
        );
    }
}

#[test]
fn casts_allow_diverging_operands() {
    assert_builds_in_both_modes("build-pass/cast_diverging_operand.spark");
    assert_builds_in_both_modes("build-pass/cast_generic_diverging.spark");
}

#[test]
fn arithmetic() {
    assert_output("run-pass/basics/arithmetic.spark", "15\n5\n50\n2");
}

#[test]
fn signed_division_remainder() {
    let fixture = "run-pass/basics/signed_division_remainder.spark";
    let literal_results = concat!(
        "-7\n0\n7\n0\n",
        "-3\n-1\n-1\n-3\n-4\n0\n-2\n0\n0\n0\n0\n0\n",
        "-7\n0\n-9223372036854775808\n0\n-2\n0\n2305843009213693951\n3\n",
        "-127\n0\n-63\n-1\n",
    );
    for (argument, runtime_results) in [
        ("x", "-7\n0\n7\n0\n-7\n0\n"),
        ("xx", "-3\n-1\n3\n-1\n-3\n1\n"),
        ("xxx", "-2\n-1\n2\n-1\n-2\n1\n"),
        ("xxxx", "-1\n-3\n1\n-3\n-1\n3\n"),
    ] {
        let expected = format!("{literal_results}{runtime_results}");
        for args in [&["--", argument][..], &["--no-opt", "--", argument][..]] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, expected, "{fixture} {args:?}");
        }
    }
}

#[test]
fn signed_remainder_preserves_division_trap() {
    let fixture = "run-fail/basics/signed_remainder_preserves_division_trap.spark";
    for mode in [&[][..], &["--no-opt"][..]] {
        let valid_args = [mode, &["--", "x"]].concat();
        let valid = run_spark_with_args(fixture, &valid_args);
        assert_eq!(valid.exit_code, 0, "{fixture} {mode:?}: {}", valid.stderr);
        assert_eq!(valid.stdout, "0\n", "{fixture} {mode:?}");

        let invalid_args = [mode, &["--", ""]].concat();
        let invalid = run_spark_with_args(fixture, &invalid_args);
        assert_eq!(
            invalid.exit_code, 1,
            "{fixture} {mode:?}: {}",
            invalid.stderr
        );
        assert!(invalid.stdout.is_empty(), "{fixture} {mode:?}");
    }
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
    assert_output(
        "run-pass/basics/string_interp_field.spark",
        "x=10\nname=origin",
    );
}

#[test]
fn tuple_field_access() {
    assert_output(
        "run-pass/basics/tuple_field_access.spark",
        "10\n20\nalice\n30\ntrue",
    );
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
    assert_output(
        "run-pass/basics/kitchen_sink.spark",
        "5\n4\n3\n2\n1\n9\n18\n27\n3\n13\n3",
    );
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
    assert_output(
        "run-pass/collections/list_range_each.spark",
        "0\n1\n2\n3\n4",
    );
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
fn collection_argument_types() {
    let map_key = "map key type mismatch";
    let set_element = "set element type mismatch";
    let string_key = (map_key, Some(("String", "Int64")));
    let string_element = (set_element, Some(("String", "Int64")));
    let non_indexable = ("type is not indexable", None);

    for (name, (diagnostic, types)) in [
        ("map_index_key", string_key),
        ("map_assign_key", string_key),
        ("map_compound_assign_key", string_key),
        ("map_contains_key", string_key),
        ("map_remove_key", string_key),
        ("map_key_width", (map_key, Some(("Int8", "Int64")))),
        ("set_contains_element", string_element),
        ("set_add_element", string_element),
        ("set_remove_element", string_element),
        ("set_index", non_indexable),
        ("set_index_assign", non_indexable),
    ] {
        let fixture = format!("compile-fail/type_errors/{name}.spark");
        for args in [&[][..], &["--no-opt"][..]] {
            let result = run_spark_with_args(&fixture, args);
            assert_ne!(result.exit_code, 0, "{fixture} {args:?}");
            assert!(
                result.stderr.contains(diagnostic),
                "{fixture} {args:?} expected {diagnostic:?}, got:\n{}",
                result.stderr
            );
            if let Some((expected, found)) = types {
                let label = format!("expected `{expected}`, found `{found}`");
                assert!(
                    result.stderr.contains(&label),
                    "{fixture} {args:?} expected {label:?}, got:\n{}",
                    result.stderr
                );
            }
        }
    }
}

#[test]
fn collection_argument_types_accept_mutable_values() {
    let fixture = "run-pass/collections/keyed_collection_methods.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout.trim(), "true\nfalse");
    }
}

#[test]
fn collection_storage_rejects_incompatible_slot_layouts() {
    for (name, diagnostic) in [
        ("list_retype_element_storage", "type mismatch in `widened`"),
        ("map_retype_key_storage", "argument type mismatch"),
        ("map_retype_value_storage", "return type mismatch"),
        ("set_retype_element_storage", "return type mismatch"),
        (
            "map_cast_key_storage",
            "cast would change collection storage type",
        ),
        (
            "list_cast_element_storage",
            "cast would change collection storage type",
        ),
        (
            "list_cast_noncollection",
            "cast would change collection storage type",
        ),
        (
            "set_cast_element_storage",
            "cast would change collection storage type",
        ),
        (
            "generic_collection_cast",
            "cast would change collection storage type",
        ),
        (
            "ref_collection_cast",
            "cast would change collection storage type",
        ),
    ] {
        let fixture = format!("compile-fail/type_errors/{name}.spark");
        for args in [&[][..], &["--no-opt"][..]] {
            let result = run_spark_with_args(&fixture, args);
            assert_ne!(result.exit_code, 0, "{fixture} {args:?}");
            assert!(
                result.stderr.contains(diagnostic),
                "{fixture} {args:?} expected {diagnostic:?}, got:\n{}",
                result.stderr
            );
        }
    }
}

#[test]
fn list_lookup_reports_untyped_empty_lists() {
    for name in [
        "list_untyped_empty_lookup",
        "list_untyped_empty_block_lookup",
    ] {
        let fixture = format!("compile-fail/type_errors/{name}.spark");
        for args in [&[][..], &["--no-opt"][..]] {
            let result = run_spark_with_args(&fixture, args);
            assert_ne!(result.exit_code, 0, "{fixture} {args:?}");
            assert!(
                result
                    .stderr
                    .contains("cannot infer element type for list lookup"),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn list_lookup_checks_signed_and_unsigned_index_bounds() {
    let fixture = "run-pass/collections/list_lookup_bounds.spark";
    let expected = "0\n7\nfalse\n255\n".to_owned() + &"fallback\n99\n".repeat(7) + "1\n";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, expected);
    }
}

#[test]
fn list_lookup_supports_narrowing_and_propagating_mutable_optionals() {
    let fixture = "run-pass/collections/list_lookup_mutable_optional.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout.trim(), "7\n9\ncontinued\n1\n99\n9\n2\n9");
    }
}

#[test]
fn optional_narrowing_rejects_non_bool_guards() {
    assert_compile_error(
        "compile-fail/type_errors/optional_non_bool_guard.spark",
        "pattern guard must be Bool",
    );
}

#[test]
fn list_index_write_preserves_copy() {
    let fixture = "run-pass/collections/list_index_write.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "3\n4\n9\n6\n3\n4\n5\n6\n", "{args:?}");
    }
}

#[test]
fn list_index_write_preserves_nested_copies_and_evaluation_order() {
    let fixture = "run-pass/collections/list_index_write_nested.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "value\nouter\ninner\n4\n9\n8\n5\n6\n5\n");
    }
}

#[test]
fn list_index_write_handles_tagged_values_and_await() {
    for (fixture, expected) in [
        (
            "run-pass/collections/list_index_write_tagged.spark",
            "9\n-1\nchanged\n",
        ),
        ("run-pass/collections/list_index_write_async.spark", "9\n"),
    ] {
        for args in [&[][..], &["--no-opt"][..]] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, expected);
        }
    }
}

#[test]
fn list_index_write_preserves_closure_capture() {
    let fixture = "run-pass/collections/list_index_write_capture.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "4\n1\n4\n1\n");
    }
}

#[test]
fn list_index_write_balances_ownership_on_replacement_and_early_exit() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         int64_t probe_list_rc(const void *const *slot) { return *(const size_t *)*slot; }\n",
        include_str!("run-pass/collections/list_index_write_ownership.spark"),
        "1\n0\n1\n1\n9\n0\nearly exit\n0",
    );
}

#[test]
fn list_index_write_reports_bounds() {
    for (name, index_and_length) in [
        ("negative", "index -2, length 1"),
        ("unsigned", "index 18446744073709551615, length 1"),
        ("empty", "index 0, length 0"),
        ("empty_null", "index 0, length 0"),
        ("outer", "index 1, length 1"),
        ("inner", "index 1, length 1"),
    ] {
        let fixture = format!("run-fail/collections/list_index_write_{name}.spark");
        for args in [&[][..], &["--no-opt"][..]] {
            let result = run_spark_with_args(&fixture, args);
            assert_ne!(result.exit_code, 0, "{fixture} {args:?}");
            assert!(
                result.stderr.contains(&format!(
                    "list write index out of bounds: {index_and_length}"
                )),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn list_index_write_rejects_invalid_targets_and_values() {
    for (fixture, expected) in [
        (
            "type_errors/list_index_write_immutable.spark",
            "cannot assign to immutable variable",
        ),
        (
            "type_errors/list_index_write_invalid_index.spark",
            "list index must be an integer",
        ),
        (
            "type_errors/list_index_write_invalid_value.spark",
            "assignment type mismatch",
        ),
        (
            "type_errors/list_index_write_projected_root.spark",
            "indexed assignment requires a mutable binding",
        ),
        (
            "type_errors/list_index_write_compound.spark",
            "indexed compound assignment is unsupported",
        ),
        (
            "borrow_errors/list_index_write_borrowed_value.spark",
            "indexed assignment requires an owned value type",
        ),
        (
            "borrow_errors/list_index_write_borrowed_escape.spark",
            "indexed assignment requires an owned value type",
        ),
        (
            "borrow_errors/list_index_write_live_loan.spark",
            "cannot write to a borrowed collection",
        ),
    ] {
        assert_compile_error(&format!("compile-fail/{fixture}"), expected);
    }
}

#[test]
fn list_lookup_owns_results_from_temporary_collections() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         int64_t probe_list_rc(const void *const *slot) { return *(const size_t *)*slot; }\n",
        include_str!("run-pass/collections/list_lookup_ownership.spark"),
        "list\nindex\ncontinued\n1\n1\n0\nlist\nindex\nmissing\n0",
    );
}

#[test]
fn list_lookup_distinguishes_present_null_payloads_from_misses() {
    let fixture = "run-pass/collections/list_lookup_nested_optional.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout.trim(), "7\n10\nmissing");
    }
}

#[test]
fn set_empty_integer() {
    let fixture = "run-pass/collections/set_empty_integer.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "0\nfalse\n1\ntrue\n0\n0\nfalse\ntrue\n");
    }
}

#[test]
fn set_empty_integer_capture_and_parameter_isolation() {
    let fixture = "run-pass/collections/set_empty_integer_capture.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "true\ntrue\ntrue\nfalse\n");
    }
}

#[test]
fn set_empty_integer_rejects_invalid_mutation_and_storage() {
    for (fixture, diagnostic) in [
        (
            "borrow_errors/set_empty_integer_loan.spark",
            "cannot write to a borrowed collection",
        ),
        (
            "type_errors/set_empty_integer_projected.spark",
            "set mutation requires a mutable binding",
        ),
        (
            "type_errors/set_empty_integer_narrowed.spark",
            "set mutation requires a mutable binding",
        ),
        (
            "type_errors/set_empty_integer_wrong_width.spark",
            "set element type mismatch",
        ),
        (
            "type_errors/set_empty_integer_immutable.spark",
            "cannot mutate immutable Set",
        ),
        (
            "type_errors/set_empty_integer_unsupported.spark",
            "unsupported set element type",
        ),
    ] {
        assert_compile_error(&format!("compile-fail/{fixture}"), diagnostic);
    }
}

#[test]
fn set_literal_integer() {
    let fixture = "run-pass/collections/set_literal_integer.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(
            result.stdout,
            "2\n1\n2\n3\ntrue\nfalse\ntrue\ntrue\ntrue\n1\n6\n7\n0\n9\n"
        );
    }
}

#[test]
fn set_literal_integer_growth_and_early_exit() {
    let fixture = "run-pass/collections/set_literal_integer_growth.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "32\nfalse\ntrue\n32\n42\n");
    }
}

#[test]
fn set_literal_integer_preserves_captured_assignment() {
    let fixture = "run-pass/collections/set_literal_integer_capture_assign.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "true\ntrue\ntrue\ntrue\n");
    }
}

#[test]
fn set_literal_integer_rejects_incompatible_elements() {
    for fixture in ["set_literal_integer_mixed", "set_literal_integer_width"] {
        assert_compile_error(
            &format!("compile-fail/type_errors/{fixture}.spark"),
            "set element type mismatch",
        );
    }
}

#[test]
fn set_string_elements() {
    let fixture = "run-pass/collections/set_string_elements.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(
            result.stdout,
            "0\n1\ntrue\nfalse\nfalse\ntrue\n2\ntrue\ntrue\n1\ntrue\n25\ntrue\nfalse\n24\n"
        );
    }
}

#[test]
fn set_string_elements_balance_ownership() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         static int64_t count(const void *const *slot) { return *(const size_t *)*slot; }\n\
         int64_t probe_string_rc(const void *const *slot) { return count(slot); }\n\
         int64_t probe_set_rc(const void *const *slot) { return count(slot); }\n",
        include_str!("run-pass/collections/set_string_ownership.spark"),
        "1\n0\ntrue\ntrue\n1\n0\nfalse\ntrue\n1\n0\n1\n1\ntrue\n0\n0\n0\n1\n1\n0\n0\n0\n99\n0",
    );
}

#[test]
fn string_value_ordering() {
    let fixture = "run-pass/collections/string_value_ordering.spark";
    let expected = concat!(
        "true\nfalse\nfalse\nfalse\ntrue\ntrue\n",
        "true\ntrue\ntrue\ntrue\ntrue\ntrue\nfalse\n",
    );
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, expected, "{args:?}");
    }
}

#[test]
fn string_value_comparison_balances_owners() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\n         int64_t probe_string_rc(const void *const *slot) { return *(const size_t *)*slot; }\n",
        include_str!("run-pass/collections/string_value_comparison_ownership.spark"),
        "true\nfalse\nfalse\ntrue\ntrue\ntrue\n0\n0",
    );
}

#[test]
fn string_value_hash_keys_include_empty_strings() {
    let fixture = "run-pass/collections/string_value_hash_keys.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "9\n42\ntrue\ntrue\ntrue\ntrue\n2\n");
    }
}

#[test]
fn string_value_comparison_rejects_incompatible_layout() {
    assert_compile_error(
        "compile-fail/type_errors/unsupported_string_comparison.spark",
        "String comparison requires two String operands",
    );
}

#[test]
fn tuple_list_contains_agrees_with_equality() {
    let fixture = "run-pass/collections/tuple_list_contains.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "true\ntrue\ntrue\nfalse\ntrue\n");
    }
}

#[test]
fn tuple_list_contains_balances_temporary_owners() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\n         int64_t probe_string_rc(const void *const *slot) { return *(const size_t *)*slot; }\n",
        include_str!("run-pass/collections/tuple_list_contains_ownership.spark"),
        "true\ntrue\ntrue\ntrue\nfalse\nfalse\ntrue\n0\n0",
    );
}

#[test]
fn list_contains_uses_zero_sized_slot_marker() {
    assert_native_ownership_fixture(
        r#"#include <stdint.h>
extern void *__spark_value_desc_new(int64_t, int64_t, uintptr_t, uintptr_t, uintptr_t, uintptr_t);
extern void *__spark_list_new(void *);
extern void __spark_list_push(void *, const unsigned char *);
void *give(void) {
    void *list = __spark_list_new(__spark_value_desc_new(0, 1, 0, 0, 0, 0));
    unsigned char zero = 0;
    __spark_list_push(list, &zero);
    return list;
}
"#,
        include_str!("run-pass/collections/list_contains_zero_sized.spark"),
        "true",
    );
}

#[test]
fn collection_equality_uses_float_values_and_integer_keys() {
    let fixture = "run-pass/collections/collection_equality_boundaries.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(
            result.stdout,
            "true\nfalse\ntrue\n7\nfalse\ntrue\nfalse\n0\n0\n0\n"
        );
    }
}

#[test]
fn collection_equality_rejects_unsupported_slots() {
    for (fixture, diagnostic) in [
        ("unsupported_hash_slot_types", "unsupported map key type"),
        (
            "unsupported_hash_slot_types",
            "unsupported set element type",
        ),
        (
            "unsupported_specialized_hash_slot",
            "expected an integer or String after specialization",
        ),
        (
            "unsupported_tuple_list_contains",
            "List.contains requires supported tuple field types",
        ),
        (
            "unsupported_specialized_tuple_contains",
            "List.contains requires supported tuple field types",
        ),
        (
            "unsupported_specialized_mutable_tuple_contains",
            "List.contains requires supported tuple field types",
        ),
        (
            "unsupported_specialized_tuple_eq",
            "tuple equality requires matching supported field types",
        ),
    ] {
        assert_compile_error(
            &format!("compile-fail/type_errors/{fixture}.spark"),
            diagnostic,
        );
    }
}

#[test]
fn tuple_value_equality() {
    let fixture = "run-pass/collections/tuple_value_equality.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(
            result.stdout, "true\nfalse\nfalse\ntrue\ntrue\ntrue\ntrue\ntrue\n4\n",
            "{args:?}"
        );
    }
}

#[test]
fn tuple_value_equality_balances_nested_owners() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\n         int64_t probe_string_rc(const void *const *slot) { return *(const size_t *)*slot; }\n",
        include_str!("run-pass/collections/tuple_value_equality_ownership.spark"),
        "true\nfalse\ntrue\nfalse\nfalse\ntrue\n0\n0",
    );
}

#[test]
fn tuple_value_equality_rejects_unsupported_fields_and_ordering() {
    let fixture = "compile-fail/type_errors/unsupported_tuple_comparison.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_ne!(result.exit_code, 0, "{fixture} {args:?}");
        for diagnostic in [
            "tuple ordering is not supported",
            "tuple equality requires matching supported field types",
        ] {
            assert!(
                result.stderr.contains(diagnostic),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn tuple_ownership_balances_nested_fields() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         int64_t probe_string_rc(const void *const *slot) { return *(const size_t *)*slot; }\n",
        include_str!("run-pass/collections/tuple_ownership.spark"),
        "2\n3\n3\n4\n0\n6\n3\n3\n0",
    );
}

#[test]
fn map_iterable_rejects_callback_layout_conversions() {
    for (fixture, diagnostic) in [
        (
            "map_iterable_reduce_callback_layout",
            "`reduce` element type mismatch",
        ),
        (
            "map_iterable_partial_callback_layout",
            "`each` element type mismatch",
        ),
        (
            "map_iterable_partial_bound_layout",
            "argument type mismatch",
        ),
    ] {
        assert_compile_error(
            &format!("compile-fail/type_errors/{fixture}.spark"),
            diagnostic,
        );
    }
}

#[test]
fn map_iterable_frequency() {
    let fixture = "run-pass/collections/map_iterable_frequency.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        let mut lines: Vec<_> = result.stdout.lines().collect();
        assert_eq!(lines.pop(), Some("171"));
        assert_eq!(lines.pop(), Some("18"));
        assert_eq!(lines.pop(), Some("2"));
        assert_eq!(lines.pop(), Some("3"));
        assert_eq!(lines.len(), 4);
        let mut pairs: Vec<_> = lines
            .chunks_exact(2)
            .map(|pair| (pair[0], pair[1]))
            .collect();
        pairs.sort_unstable();
        assert_eq!(pairs, [("blue", "1"), ("red", "2")]);
    }
}

#[test]
fn set_iterable_each_and_filter() {
    let fixture = "run-pass/collections/set_iterable.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        let lines: Vec<_> = result.stdout.lines().collect();
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[0], "0");
        let mut visited = [lines[1], lines[2]];
        visited.sort_unstable();
        assert_eq!(visited, ["blue", "red"]);
        assert_eq!(&lines[3..], &["blue", "5", "18", "171"]);
    }
}

#[test]
fn set_iterable_snapshot_owns_string_elements() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         int64_t probe_string_rc(const void *const *slot) { return *(const size_t *)*slot; }\n",
        include_str!("run-pass/collections/set_iterable_ownership.spark"),
        "4\n2\n1\n7\n0",
    );
}

#[test]
fn hash_collection_pipeline_is_sequential() {
    assert_compile_error(
        "compile-fail/type_errors/hash_collection_parallel.spark",
        "parallel iteration does not support hash collections",
    );
}

#[test]
fn map_index_write_inserts_and_replaces() {
    let fixture = "run-pass/collections/map_index_write.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "7\n1\n9\n-1\n1\n-1\n0\n456\n1\n");
    }
}

#[test]
fn map_index_write_preserves_entries_across_growth() {
    let fixture = "run-pass/collections/map_index_write_growth.spark";
    let mut expected = vec![32];
    expected.extend(100..132);
    expected[4] = 999;
    expected.extend([1, 100, -1]);
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        let values: Vec<i32> = result
            .stdout
            .lines()
            .map(|line| line.parse().unwrap())
            .collect();
        assert_eq!(values, expected);
    }
}

#[test]
fn map_index_write_handles_list_paths_and_tagged_values() {
    for (fixture, expected) in [
        (
            "run-pass/collections/map_index_write_nested.spark",
            "value\nrow\nkey\n9\n7\n1\n-1\n1\n-1\n",
        ),
        (
            "run-pass/collections/map_index_write_tagged.spark",
            "-1\n9\nmissing\n8\n",
        ),
    ] {
        for args in [&[][..], &["--no-opt"][..]] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, expected);
        }
    }
}

#[test]
fn map_index_write_balances_key_and_value_ownership() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         static int64_t count(const void *const *slot) { return *(const size_t *)*slot; }\n\
         int64_t probe_list_rc(const void *const *slot) { return count(slot); }\n\
         int64_t probe_string_rc(const void *const *slot) { return count(slot); }\n",
        include_str!("run-pass/collections/map_index_write_ownership.spark"),
        "1\n1\n2\n0\n1\n1\n0\n1\n1\n4\n9\n0\n0\n0\n0\n-1\n0",
    );
}

#[test]
fn map_index_write_rejects_invalid_targets_and_values() {
    for (name, expected) in [
        ("immutable", "cannot assign to immutable variable"),
        ("wrong_value", "assignment type mismatch"),
        ("key_type", "map write keys must be integers or strings"),
        ("zero_storage", "map value type has no storage"),
        (
            "intermediate",
            "map entry must end an indexed assignment path",
        ),
        ("compound", "indexed compound assignment is unsupported"),
    ] {
        assert_compile_error(
            &format!("compile-fail/type_errors/map_index_write_{name}.spark"),
            expected,
        );
    }
}

#[test]
fn map_index_write_rejects_borrowed_capture_storage() {
    assert_compile_error(
        "compile-fail/borrow_errors/map_index_write_borrowed_capture.spark",
        "indexed assignment requires an owned value type",
    );
}

#[test]
fn map_lookup_default_is_lazy_and_owns_payload() {
    let fixture = "run-pass/collections/map_lookup_default.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(
            result.stdout.trim(),
            "map\nkey\nfound\nmap\nkey\nfallback\ndefault"
        );
    }
}

#[test]
fn map_lookup_balances_payload_and_input_ownership() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         static int64_t count(const void *const *slot) { return *(const size_t *)*slot; }\n\
         int64_t probe_list_rc(const void *const *slot) { return count(slot); }\n\
         int64_t probe_string_rc(const void *const *slot) { return count(slot); }\n\
         int64_t probe_map_rc(const void *const *slot) { return count(slot); }\n",
        include_str!("run-pass/collections/map_lookup_ownership.spark"),
        "1\n0\n0\n1\n1\n1\n0\n1\n0\n0\n1\n1\n0",
    );
}

#[test]
fn map_lookup_works_with_optional_consumers() {
    let fixture = "run-pass/collections/map_lookup_consumers.spark";
    let expected = "continued\npresent\nmissing\npresent\n7\n3\ntagged\n99\nfalse\ntrue\n0";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout.trim(), expected);
    }
}

#[test]
fn map_lookup_retags_nested_coalesced_payloads() {
    let fixture = "run-pass/collections/map_lookup_nested_union.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout.trim(), "nested\nfalse");
    }
}

#[test]
fn list_literal_preserves_declared_slot_layouts() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args("run-pass/collections/list_literal_slots.spark", args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(
            result.stdout, "42\n-1\n7\n4\n1\n-2\n8\n10\n11\n11\n42\n1\n",
            "{args:?}"
        );
    }
}

#[test]
fn list_literal_rejects_incompatible_elements() {
    for fixture in [
        "list_literal_wrong_first",
        "list_literal_wrong_later",
        "list_literal_inferred",
        "list_literal_width",
    ] {
        for args in [&[][..], &["--no-opt"][..]] {
            let result =
                run_spark_with_args(&format!("compile-fail/type_errors/{fixture}.spark"), args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result.stderr.contains("list element type mismatch"),
                "{fixture} {args:?}: {}",
                result.stderr
            );
            assert!(
                !result.stderr.contains("codegen error"),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn list_literal_rejects_zero_sized_elements() {
    for fixture in ["list_literal_zero_sized", "list_literal_zero_sized_generic"] {
        for args in [&[][..], &["--no-opt"][..]] {
            let result =
                run_spark_with_args(&format!("compile-fail/type_errors/{fixture}.spark"), args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result.stderr.contains("list element type has no storage"),
                "{fixture} {args:?}: {}",
                result.stderr
            );
            assert!(
                !result.stderr.contains("codegen error"),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn list_literal_balances_element_ownership() {
    assert_native_ownership_fixture(
        r#"#include <stddef.h>
#include <stdint.h>
typedef struct { size_t rc; void *desc; void *data; size_t len; size_t cap; } List;
static int64_t count(const void *const *slot) { return *(const size_t *)*slot; }
static int64_t first(const List *const *slot) { return **(const size_t *const *)(*slot)->data; }
int64_t probe_string_rc(const void *const *slot) { return count(slot); }
int64_t probe_list_rc(const void *const *slot) { return count(slot); }
int64_t first_map_rc(const List *const *slot) { return first(slot); }
int64_t first_list_rc(const List *const *slot) { return first(slot); }
int64_t first_string_rc(const List *const *slot) { return first(slot); }
"#,
        include_str!("run-pass/collections/list_literal_ownership.spark"),
        "1\n1\n1\n2\n2\n4\n0\n0\n4\n6\n0\n0\n0\n7\n0\n8\n0",
    );
}

#[test]
fn map_literal_empty_uses_expected_type() {
    let fixture = "run-pass/collections/map_literal_empty.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout.trim(), "0\n0\n0\n0");
    }
}

#[test]
fn map_literal_computed_keys_survive_growth() {
    let fixture = "run-pass/collections/map_literal_computed_keys.spark";
    let expected = "13\n0\n101\n7\n12\n99\n42\n81\n8\n7\n42\n42\n5";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout.trim(), expected);
    }
}

#[test]
fn map_literal_rejects_incompatible_entries() {
    for (name, diagnostic, label) in [
        (
            "map_literal_mixed_keys",
            "map key type mismatch",
            "expected `String`, found `Int64`",
        ),
        (
            "map_literal_wrong_first_key",
            "map key type mismatch",
            "expected `String`, found `Int64`",
        ),
        (
            "map_literal_unsupported_key",
            "map literal keys must be integers or strings",
            "unsupported map key type",
        ),
        (
            "map_literal_wrong_later_value",
            "map value type mismatch",
            "expected `Int64`, found `String`",
        ),
        (
            "map_literal_zero_sized_value",
            "map value type has no storage",
            "use a value type with a stored representation",
        ),
    ] {
        let fixture = format!("compile-fail/type_errors/{name}.spark");
        for args in [&[][..], &["--no-opt"][..]] {
            let result = run_spark_with_args(&fixture, args);
            assert_ne!(result.exit_code, 0, "{fixture} {args:?}");
            assert!(
                result.stderr.contains(diagnostic),
                "{fixture} {args:?}: {}",
                result.stderr
            );
            assert!(
                result.stderr.contains(label),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn map_literal_snapshots_slots_and_packs_optional_values() {
    let fixture = "run-pass/collections/map_literal_snapshots.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(
            result.stdout.trim(),
            "key\nvalue\n10\n20\n11\nstored\ninner null\nmissing\n6\nunion"
        );
    }
}

#[test]
fn map_literal_balances_ownership_on_growth_replacement_and_early_return() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         int64_t probe_list_rc(const void *const *slot) { return *(const size_t *)*slot; }\n\
         int64_t probe_string_rc(const void *const *slot) { return *(const size_t *)*slot; }\n\
         static int64_t number = 73;\n\
         static int64_t *number_ref = &number;\n\
         int64_t **nested_int(void) { return &number_ref; }\n",
        include_str!("run-pass/collections/map_literal_ownership.spark"),
        "13\n1\n13\n0\n0\n99\n0\n0\n81\n0\n0\n82\n84\n73",
    );
}

#[test]
fn map_get_optional() {
    assert_output("run-pass/collections/map_get_optional.spark", "found");
}

#[test]
fn map_get_optional_list() {
    assert_output(
        "run-pass/collections/map_get_optional_list.spark",
        "found\na",
    );
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
fn captured_list_writes_preserve_snapshots_across_calls() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args("run-pass/lambdas/captured_list_writes.spark", args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(
            result.stdout, "4\n3\n4\n3\n2\n3\n2\n3\n2\n3\n3\n",
            "{args:?}"
        );
    }
}

#[test]
fn captured_list_writes_balance_invocation_owners() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         int64_t probe_list_rc(const void *const *slot) { return *(const size_t *)*slot; }\n\
         int64_t probe_string_rc(const void *const *slot) { return *(const size_t *)*slot; }\n",
        include_str!("run-pass/lambdas/captured_list_ownership.spark"),
        "2\n0\n2\n2\n0\n1\n0\n0\n0",
    );
}

#[test]
fn captured_hash_writes_balance_invocation_owners_and_preserve_snapshots() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         static int64_t count(const void *const *slot) { return *(const size_t *)*slot; }\n\
         int64_t probe_map_rc(const void *const *slot) { return count(slot); }\n\
         int64_t probe_set_rc(const void *const *slot) { return count(slot); }\n\
         int64_t probe_string_rc(const void *const *slot) { return count(slot); }\n",
        include_str!("run-pass/lambdas/captured_hash_ownership.spark"),
        "0\ntrue\n2\n0\ntrue\n0\n2\n0\n0\n0\n1\ntrue\n1\nfalse\n0\n0\n0\n0\n0\n0",
    );
}

#[test]
fn captured_environment_owners_balance_across_invocations() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         int64_t probe_closure_rc(const void *const *value) {\n\
             return value[1] ? *(const size_t *)value[1] : 0;\n\
         }\n\
         int64_t probe_list_rc(const void *const *slot) { return *(const size_t *)*slot; }\n",
        include_str!("run-pass/lambdas/captured_environment_ownership.spark"),
        "1\n1\n0\n1\n0\n1\n9\n0\n1\n0\n1\n0\n1\n7\n0\n3\n0\n7\n0\n2\n1\n0",
    );
}

#[test]
fn closure_capture_mutable_list() {
    let fixture = "run-pass/lambdas/closure_capture_mutable_list.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "1\n3\n");
    }
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
fn standalone_partial_preserves_argument_layouts() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args("run-pass/partial/argument_layouts.spark", args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(
            result.stdout, "102\n7\n10\n4\n5\n43\n7\n8\n6\n4\n3\n5\n15\n10\n",
            "{args:?}"
        );
    }
}

#[test]
fn standalone_partial_balances_tagged_argument_owners() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         int64_t probe_string_rc(const void *const *slot) { return *(const size_t *)*slot; }\n",
        include_str!("run-pass/partial/tagged_ownership.spark"),
        "14\n0\n15\n0\npayload-value",
    );
}

#[test]
fn standalone_partial_diagnoses_unsupported_tagged_reference_coercion() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "compile-fail/type_errors/partial_tagged_reference.spark",
            args,
        );
        assert_eq!(result.exit_code, 1, "{args:?}: {}", result.stderr);
        assert!(
            result
                .stderr
                .contains("cannot materialize tagged call argument"),
            "{args:?}: {}",
            result.stderr
        );
    }
}

#[test]
fn standalone_partial() {
    assert_output(
        "run-pass/partial/standalone_partial.spark",
        "15\n30\n50\n100",
    );
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

#[test]
fn parallel_range_each_single() {
    assert_output("run-pass/parallel/parallel_range_each_single.spark", "42");
}

// ============================================================================
// To (pipeline operator)
// ============================================================================

#[test]
fn to_sort() {
    assert_output("run-pass/to/to_sort.spark", "1\n2\n3");
}

#[test]
fn sort_integer_widths() {
    let fixture = "run-pass/to/sort_integer_widths.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "1\n2\n3\n1\n2\n3\n3\n1\n2\n", "{args:?}");
    }
}

#[test]
fn sort_integer_widths_matrix() {
    let fixture = "run-pass/to/sort_integer_matrix.spark";
    let expected = concat!(
        "-128\n0\n127\n127\n-32768\n0\n32767\n32767\n",
        "-2147483648\n0\n2147483647\n2147483647\n-8\n0\n7\n7\n",
        "0\n1\n128\n255\n255\n0\n1\n32768\n65535\n65535\n",
        "0\n1\n2147483648\n4294967295\n4294967295\n",
        "true\ntrue\ntrue\ntrue\n0\n255\n0\n9\n",
    );
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, expected, "{args:?}");
    }
}

#[test]
fn sort_balances_input_ownership() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\n         int64_t probe_list_rc(const void *const *slot) { return *(const size_t *)*slot; }\n
         int64_t probe_string_rc(const void *const *slot) { return *(const size_t *)*slot; }\n",
        include_str!("run-pass/to/sort_ownership.spark"),
        "3\n3\n0\n1\n0",
    );
}

#[test]
fn sort_rejects_unsupported_element_layouts() {
    let fixture = "compile-fail/type_errors/unsupported_sort_element.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_ne!(result.exit_code, 0, "{fixture} {args:?}");
        assert_eq!(
            result
                .stderr
                .matches("sort does not support list elements of type")
                .count(),
            4,
            "{fixture} {args:?}: {}",
            result.stderr
        );
    }
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
fn pipeline_first_input_preserves_builtin_and_tagged_layouts() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args("run-pass/to/pipeline_first_layouts.spark", args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(
            result.stdout, "1\n128\n255\n128\n2\ntrue\ntrue\n3\n2\n7\n9\n",
            "{args:?}"
        );
    }
}

#[test]
fn pipeline_first_input_rejects_unsupported_reference_coercions() {
    for (fixture, diagnostic) in [
        (
            "pipeline_first_tagged_reference",
            "cannot materialize tagged call argument",
        ),
        (
            "pipeline_first_mutable_reference",
            "cannot pass immutable value as mutable reference",
        ),
    ] {
        for args in [&[][..], &["--no-opt"][..]] {
            let result =
                run_spark_with_args(&format!("compile-fail/type_errors/{fixture}.spark"), args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result.stderr.contains(diagnostic),
                "{fixture} {args:?}: {}",
                result.stderr
            );
            assert!(
                !result.stderr.contains("codegen error"),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn pipeline_first_input_matches_call_reference_coercions() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args("run-pass/to/pipeline_first_input.spark", args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "1\n2\n3\n3\n3\n10\n3\n10\n", "{args:?}");
    }
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
fn tagged_call_arguments_match_the_declared_layout() {
    let fixture = "run-pass/to/tagged_call_arguments.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "absent\n42\ntext-owned\n7\n7\n13\n-1\n");
    }
}

#[test]
fn tagged_call_arguments_cover_specialized_functions_and_methods() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args("run-pass/to/tagged_call_paths.spark", args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "45\n8\n2\n4\n");
    }
}

#[test]
fn tagged_call_arguments_release_staged_payloads() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         int64_t probe_string_rc(const void *const *slot) { return *(const size_t *)*slot; }\n",
        include_str!("run-pass/to/tagged_call_ownership.spark"),
        "13\n0\n7\n0\n10\n0",
    );
}

#[test]
fn tagged_call_arguments_widen_into_nested_layouts() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args("run-pass/to/nested_tagged_call_arguments.spark", args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "7\n10\n-1\n11\n11\n");
    }
}

#[test]
fn tagged_call_arguments_diagnose_unrepresentable_conversions() {
    for (fixture, diagnostic) in [
        (
            "compile-fail/type_errors/tagged_call_unsupported_member_coercion.spark",
            "cannot materialize tagged call argument",
        ),
        (
            "compile-fail/type_errors/tagged_call_ambiguous_member.spark",
            "ambiguous tagged call argument",
        ),
    ] {
        for args in [&[][..], &["--no-opt"][..]] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 1, "{args:?}: {}", result.stderr);
            assert!(
                result.stderr.contains(diagnostic),
                "{args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn tagged_call_arguments_allow_diverging_expressions() {
    assert_builds_in_both_modes("build-pass/tagged_call_diverging_argument.spark");
}

fn assert_builds_in_both_modes(fixture: &str) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(fixture);
    let dir = tempfile::tempdir().expect("create build output directory");
    let spark = Path::new(env!("CARGO_BIN_EXE_spark"));
    for args in [&[][..], &["--no-opt"][..]] {
        let result = Command::new(spark)
            .args(["build", "--single-file"])
            .args(args)
            .arg(&fixture)
            .arg("-o")
            .arg(dir.path().join("diverging"))
            .env(
                "SPARK_RUNTIME_DIR",
                spark.parent().expect("driver directory"),
            )
            .output()
            .expect("build fixture");
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
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

#[test]
fn to_malformed_arms_report_errors_without_hanging() {
    let cases = [
        (
            include_str!("compile-fail/syntax_errors/to_fat_arrow.spark"),
            "expected ->, found =>",
        ),
        (
            "fn main(): Int { return 1 to { _ -> = 0 } }",
            "expected expression, found =",
        ),
        (
            "fn main(): Int { return 1 to { _ =>",
            "expected ->, found =>",
        ),
    ];
    let dir = tempfile::tempdir().expect("failed to create test directory");
    let source = dir.path().join("malformed.spark");
    let binary = dir.path().join("malformed");

    for (input, expected) in cases {
        std::fs::write(&source, input).expect("failed to write test source");
        let mut child = Command::new(env!("CARGO_BIN_EXE_spark"))
            .args(["build", "--single-file"])
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn spark");
        let start = std::time::Instant::now();
        while child.try_wait().expect("failed to poll spark").is_none() {
            if start.elapsed() >= Duration::from_secs(1) {
                let _ = child.kill();
                child.wait().expect("failed to reap spark");
                panic!("spark did not finish parsing malformed input: {input}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child
            .wait_with_output()
            .expect("failed to read spark output");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains(expected),
            "expected {expected:?}, got:\n{stderr}"
        );
    }
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
    assert_output(
        "run-pass/stdlib/stdlib_math.spark",
        "4\n1024\n5\n3\n4\n1\n2\n4",
    );
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
    assert_output(
        "run-pass/stdlib/stdlib_json_nested.spark",
        "2\nfirst\nsecond",
    );
}

#[test]
fn stdlib_io_write_read() {
    assert_output(
        "run-pass/stdlib/stdlib_io_write_read.spark",
        "hello from spark",
    );
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
    let count: i64 = result
        .stdout
        .trim()
        .parse()
        .expect("expected integer output");
    assert!(
        count >= 1,
        "args() should return at least the program name, got {count}"
    );
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
// Target-directed text parsing
// ============================================================================

#[test]
fn target_directed_parse_infers_int_and_propagates_parse_errors() {
    assert_output(
        "run-pass/parsing/parse_inferred_int.spark",
        "42\n-9223372036854775808\nparse error\nparse error\n17\nparse error\n23",
    );
}

#[test]
fn target_directed_parse_dispatches_to_custom_point_binding() {
    assert_output(
        "run-pass/parsing/parse_bound_point.spark",
        "3,0\n-8,0\nparse error",
    );
}

#[test]
fn target_directed_parse_rejects_ambiguous_target() {
    for fixture in ["parse_ambiguous_target", "parse_ambiguous_result"] {
        assert_compile_error(
            &format!("compile-fail/type_errors/{fixture}.spark"),
            "ambiguous target type for `parse`",
        );
    }
}

#[test]
fn target_directed_parse_reports_invalid_calls_and_bindings() {
    for (fixture, expected) in [
        (
            "parse_missing_target",
            "cannot infer target type for `parse`",
        ),
        (
            "parse_unbound_type",
            "type `Point` does not bind `Parseable`",
        ),
        (
            "parse_binding_missing_parser",
            "missing required function `parse` from `Parseable`",
        ),
        ("parse_binding_duplicate", "duplicate `Parseable` binding"),
        (
            "parse_binding_wrong_module",
            "a parser must be bound in the type's defining module",
        ),
        (
            "parse_binding_invalid_signature",
            "invalid parser signature",
        ),
        ("parse_wrong_arity", "expected 1 argument, found 0"),
        ("parse_wrong_argument_type", "expected `String`"),
    ] {
        assert_compile_error(
            &format!("compile-fail/type_errors/{fixture}.spark"),
            expected,
        );
    }
}

#[test]
fn string_lines_preserves_blank_lines_and_utf8() {
    assert_output(
        "run-pass/parsing/string_lines.spark",
        "0\n1\n[]\n4\n[café]\n[]\n[λ]\n[last\r]",
    );
}

#[test]
fn string_lines_rejects_arguments() {
    assert_compile_error(
        "compile-fail/type_errors/string_lines_with_argument.spark",
        "expected 0 arguments, found 1",
    );
}

#[test]
fn string_tokenization_preserves_text_boundaries() {
    let fixture = "run-pass/parsing/string_tokenization.spark";
    let expected = "[a]\n[]\n[b]\n[]\n[]\n[é]\n[λ]\n[a]\n[b]\n[c]\n[x]\n[]\n[y]\n[λ]\n[é]\n[café]\n[]\n[one\r\ntwo]\n[ three ]";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(result.stdout.trim_end(), expected);
    }
}

#[test]
fn string_split_requires_a_string_delimiter() {
    assert_compile_error(
        "compile-fail/type_errors/string_split_non_string_delimiter.spark",
        "expected `String`",
    );
}

#[test]
fn string_chars_returns_unicode_scalars() {
    let fixture = "run-pass/parsing/string_chars.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(result.stdout.trim_end(), "0\n9\n4\ntrue\ntrue\ntrue\ntrue");
    }
}

#[test]
fn string_substring_checks_utf8_byte_ranges() {
    let fixture = "run-pass/parsing/string_substring.spark";
    let expected = "[é]\n[🙂]\n[]\n[]\n[-1,1]\n[3,1]\n[0,8]\n[2,3]\n[1,2]";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(result.stdout.trim_end(), expected);
    }
}

#[test]
fn string_substring_requires_integer_byte_offsets() {
    assert_compile_error(
        "compile-fail/type_errors/string_substring_non_integer.spark",
        "argument type mismatch",
    );
}

#[test]
fn bulk_integer_parsing_owns_values_and_reports_byte_spans() {
    let fixture = "run-pass/parsing/parse_bulk_ints.spark";
    let expected = concat!(
        "4\n42\n-8\n3\n0\n0\n0\n",
        "2\n-9223372036854775808\n9223372036854775807\n",
        "error 2 [3,5)\nerror 3 [2,21)\nerror 2 [2,22)\n",
        "error 3 [2,22)\nerror 2 [2,3)\n",
        "error 1 [0,0)\nerror 2 [0,1)\nerror 2 [0,3)\n",
    );
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(result.stdout, expected);
    }
}

#[test]
fn tagged_union_retains_and_releases_list_payloads() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         int64_t probe_list_rc(const void *const *slot) { return *(const size_t *)*slot; }\n",
        include_str!("run-pass/parsing/owned_union.spark"),
        "1\n1\n3\n1\n1\n1\n1\n0\n1\n1\n1\n2\n2\n1",
    );
}

#[test]
fn parsing_coerces_nested_reference_arguments() {
    assert_native_ownership_fixture(
        include_str!("run-pass/parsing/nested_parse_ref.c"),
        include_str!("run-pass/parsing/nested_parse_ref.spark"),
        "2\nb\n7\n1",
    );
}

#[test]
fn target_directed_parse_supports_mutable_bindings() {
    let fixture = "run-pass/parsing/parse_mutable_target.spark";
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(result.stdout.trim(), "42\nparse error\n43");
    }
}

fn assert_native_ownership_fixture(native: &str, fixture: &str, expected: &str) {
    let dir = tempfile::tempdir().expect("create ownership probe directory");
    let probe = dir.path().join("probe.c");
    std::fs::write(&probe, native).expect("write native ownership probe");
    for (tool, args) in [
        ("cc", &["-c", "probe.c", "-o", "probe.o"][..]),
        ("ar", &["rcs", "libunion_probe.a", "probe.o"][..]),
    ] {
        let result = Command::new(tool)
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("build reference count probe");
        assert!(
            result.status.success(),
            "{tool}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    let source = dir.path().join("probe.spark");
    std::fs::write(
        &source,
        fixture.replace(
            "link \"union_probe\"",
            &format!("link \"union_probe\" from \"{}\"", dir.path().display()),
        ),
    )
    .expect("write ownership fixture");

    let spark = Path::new(env!("CARGO_BIN_EXE_spark"));
    for args in [&[][..], &["--no-opt"][..]] {
        let result = Command::new(spark)
            .args(["run", "--single-file"])
            .args(args)
            .arg(&source)
            .env(
                "SPARK_RUNTIME_DIR",
                spark.parent().expect("driver directory"),
            )
            .output()
            .expect("run ownership fixture");
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(result.status.success(), "{stderr}");
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), expected);
    }
}

#[test]
fn text_lines_parses_a_file_to_an_answer() {
    let dir = tempfile::tempdir().expect("create input directory");
    let input = dir.path().join("input.txt");
    let path = input.to_str().expect("UTF-8 input path");
    for (text, expected, exit_code) in [
        ("+42\r\n-8\n3\r\n", "37", 0),
        ("+42\r\n-8x\n3\r\n", "line 2: parse error", 1),
    ] {
        std::fs::write(&input, text).expect("write puzzle input");
        for args in [&["--", path][..], &["--no-opt", "--", path][..]] {
            let result = run_spark_with_args("run-pass/parsing/sum_file_lines.spark", args);
            assert_eq!(result.exit_code, exit_code, "{}", result.stderr);
            assert_eq!(result.stdout.trim(), expected, "{}", result.stderr);
        }
    }
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
    assert_output(
        "run-pass/type_narrowing/propagate_union_error.spark",
        "error",
    );
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
    assert_output(
        "run-pass/type_narrowing/mutable_optional_narrowing.spark",
        "10",
    );
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
    assert_output(
        "run-pass/protocols/comparable.spark",
        "true\ntrue\ntrue\nfalse",
    );
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
    assert_compile_error(
        "compile-fail/type_errors/immutable_append.spark",
        "not mutable",
    );
}

#[test]
fn to_nonexhaustive_bool() {
    assert_compile_error(
        "compile-fail/type_errors/to_nonexhaustive_bool.spark",
        "non-exhaustive",
    );
}

#[test]
fn to_nonexhaustive_int() {
    assert_compile_error(
        "compile-fail/type_errors/to_nonexhaustive_int.spark",
        "non-exhaustive",
    );
}

#[test]
fn await_non_future_error() {
    assert_compile_error(
        "compile-fail/type_errors/await_non_future_error.spark",
        "cannot `await`",
    );
}

#[test]
fn spawn_non_async_error() {
    assert_compile_error(
        "compile-fail/type_errors/spawn_non_async_error.spark",
        "cannot `spawn`",
    );
}

#[test]
fn spawn_mutable_error() {
    assert_compile_error(
        "compile-fail/type_errors/spawn_mutable_error.spark",
        "mutable binding",
    );
}

#[test]
fn spawn_ref_error() {
    assert_compile_error(
        "compile-fail/type_errors/spawn_ref_error.spark",
        "not thread-safe",
    );
}

#[test]
fn parallel_each_nonlist_error() {
    assert_compile_error(
        "compile-fail/type_errors/parallel_each_nonlist_error.spark",
        "requires a List",
    );
}

#[test]
fn parallel_each_ref_mutation_error() {
    assert_compile_error(
        "compile-fail/type_errors/parallel_each_ref_mutation.spark",
        "not thread-safe",
    );
}

#[test]
fn generic_constraint_fail() {
    assert_compile_error(
        "compile-fail/type_errors/generic_constraint_fail.spark",
        "does not satisfy constraint",
    );
}

#[test]
fn generic_constraint_structural_fail() {
    assert_compile_error(
        "compile-fail/type_errors/generic_constraint_structural_fail.spark",
        "missing required field",
    );
}

#[test]
fn to_non_union_error() {
    assert_compile_error(
        "compile-fail/type_errors/to_non_union_error.spark",
        "non-exhaustive",
    );
}

#[test]
fn immutable_field_assign() {
    assert_compile_error(
        "compile-fail/type_errors/immutable_field_assign.spark",
        "immutable",
    );
}

#[test]
fn immutable_mutable_borrow() {
    assert_compile_error(
        "compile-fail/type_errors/immutable_mutable_borrow.spark",
        "cannot pass immutable value as mutable reference",
    );
}

#[test]
fn propagate_return_mismatch() {
    assert_compile_error(
        "compile-fail/type_errors/propagate_return_mismatch.spark",
        "does not include `Null`",
    );
}

#[test]
fn propagate_union_return_mismatch() {
    assert_compile_error(
        "compile-fail/type_errors/propagate_union_return_mismatch.spark",
        "does not include it",
    );
}

#[test]
fn tuple_field_oob() {
    assert_compile_error(
        "compile-fail/type_errors/tuple_field_oob.spark",
        "out of range",
    );
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
    assert_compile_error(
        "compile-fail/type_errors/interface_missing_method.spark",
        "missing required method",
    );
}

#[test]
fn interface_signature_mismatch() {
    assert_compile_error(
        "compile-fail/type_errors/interface_signature_mismatch.spark",
        "wrong number of parameters",
    );
}

// ============================================================================
// Compile-fail: borrow errors
// ============================================================================

#[test]
fn borrow_double_mut() {
    assert_compile_error(
        "compile-fail/borrow_errors/borrow_double_mut.spark",
        "cannot borrow value as mutable more than once",
    );
}

#[test]
fn borrow_mut_immut() {
    assert_compile_error(
        "compile-fail/borrow_errors/borrow_mut_immut.spark",
        "cannot borrow value as mutable because it is also borrowed as immutable",
    );
}

#[test]
fn borrow_immut_assign() {
    assert_compile_error(
        "compile-fail/borrow_errors/borrow_immut_assign.spark",
        "cannot assign to immutable variable",
    );
}

#[test]
fn borrow_dangling_return() {
    assert_compile_error(
        "compile-fail/borrow_errors/borrow_dangling_return.spark",
        "cannot return reference to local variable",
    );
}

#[test]
fn borrow_alias_call() {
    assert_compile_error(
        "compile-fail/borrow_errors/borrow_alias_call.spark",
        "cannot pass",
    );
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
    assert!(
        contents.contains("test_project"),
        "manifest should contain project name"
    );

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
