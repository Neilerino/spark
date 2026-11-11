// TAG [SPARK-114]

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

use std::path::{Path, PathBuf};
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

fn build_spark_with_args(fixture: &str, args: &[&str]) -> SparkOutput {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(fixture);
    let dir = tempfile::tempdir().expect("create build output directory");
    let spark = Path::new(env!("CARGO_BIN_EXE_spark"));
    let output = Command::new(spark)
        .args(["build", "--single-file"])
        .args(args)
        .arg(&fixture)
        .arg("-o")
        .arg(dir.path().join("program"))
        .env(
            "SPARK_RUNTIME_DIR",
            spark.parent().expect("driver directory"),
        )
        .output()
        .expect("build fixture");
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
fn struct_field_reference_coercions_follow_declared_types() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "run-pass/references/struct_field_reference_coercions.spark",
            args,
        );
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(
            result.stdout, "7\n7\n7\n7\n7\nkept\n7\n7\ngeneric-kept\nbinding-kept\n",
            "{args:?}"
        );
    }
}

#[test]
fn struct_field_reference_coercions_reject_incompatible_types() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = build_spark_with_args(
            "compile-fail/type_errors/struct_field_reference_incompatible.spark",
            args,
        );
        assert_eq!(result.exit_code, 1, "{args:?}: {}", result.stderr);
        assert!(
            result.stderr.contains("field `value` type mismatch"),
            "{args:?}: {}",
            result.stderr
        );
        assert!(
            !result.stderr.contains("codegen error"),
            "{args:?}: {}",
            result.stderr
        );
    }
}

#[test]
fn collection_borrow_rejects_local_and_parameter_storage_escape() {
    for fixture in [
        "literal_return",
        "explicit_return",
        "parameter_return",
        "copy_survives_overwrite",
        "branch_return",
        "loop_return",
        "helper_return",
        "recursive_return",
        "struct_return",
        "generic_struct_return",
        "recursive_deref_return",
        "recursive_helper_return",
        "nested_tuple_list_return",
        "nested_tuple_optional_return",
    ] {
        for args in [&[][..], &["--no-opt"][..]] {
            let fixture = format!("compile-fail/borrow_errors/collection_borrow_{fixture}.spark");
            let result = build_spark_with_args(&fixture, args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result
                    .stderr
                    .contains("returned value contains a reference to local storage"),
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
fn collection_borrow_preserves_live_local_and_input_references() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "run-pass/collections/collection_borrow_same_scope.spark",
            args,
        );
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "7\n7\n7\n7\n7\n7\n7\n", "{args:?}");
    }
}

#[test]
fn collection_borrow_recursive_carriers_preserve_payload_lifetimes() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "run-pass/collections/collection_borrow_recursive_payload.spark",
            args,
        );
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "7\n7\n7\n", "{args:?}");
    }
}

#[test]
fn collection_borrow_rejects_owner_drop_before_payload_read() {
    for fixture in [
        "owner_drop_before_read",
        "owner_drop_in_helper",
        "owner_drop_retained_copy",
    ] {
        for args in [&[][..], &["--no-opt"][..]] {
            let fixture = format!("compile-fail/borrow_errors/collection_borrow_{fixture}.spark");
            let result = build_spark_with_args(&fixture, args);
            assert_eq!(result.exit_code, 1, "{args:?}: {}", result.stderr);
            assert!(
                result
                    .stderr
                    .contains("borrowed value does not live long enough"),
                "{args:?}: {}",
                result.stderr
            );
            assert!(
                !result.stderr.contains("codegen error"),
                "{args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn collection_borrow_allows_dead_payloads_without_pointee_reads() {
    for (fixture, expected) in [
        ("dead_payload_drop", "1\n"),
        ("owned_snapshot", "seven\nkept\n"),
    ] {
        for args in [&[][..], &["--no-opt"][..]] {
            let result = run_spark_with_args(
                &format!("run-pass/collections/collection_borrow_{fixture}.spark"),
                args,
            );
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, expected, "{fixture} {args:?}");
        }
    }
}

#[test]
fn collection_borrow_rejects_mutation_while_carrier_reference_is_used() {
    for fixture in [
        "mutation_live_carrier",
        "mutation_retained_copy",
        "mutation_helper_return",
        "mutation_helper_read",
    ] {
        for args in [&[][..], &["--no-opt"][..]] {
            let result = build_spark_with_args(
                &format!("compile-fail/borrow_errors/collection_borrow_{fixture}.spark"),
                args,
            );
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result
                    .stderr
                    .contains("cannot write to a borrowed collection"),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn collection_borrow_allows_mutation_after_last_carrier_is_cleared() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "run-pass/collections/collection_borrow_cleared_carrier_before_write.spark",
            args,
        );
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "9\n", "{args:?}");
        let result = run_spark_with_args(
            "run-pass/collections/collection_borrow_helper_discards_input.spark",
            args,
        );
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "2\n7\n", "{args:?}");
    }
}

#[test]
fn collection_borrow_source_replacement_requires_the_carrier_to_be_dead() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "compile-fail/borrow_errors/collection_borrow_live_source_replacement.spark",
            "compile-fail/borrow_errors/collection_borrow_live_scalar_replacement.spark",
        ] {
            let result = build_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result.stderr.contains("cannot assign to a borrowed value"),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn collection_borrow_allows_source_replacement_after_last_carrier_clear() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "run-pass/collections/collection_borrow_cleared_source_replacement.spark",
            "run-pass/collections/collection_borrow_cleared_scalar_replacement.spark",
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout.trim(), "9", "{fixture} {args:?}");
        }
    }
}

#[test]
fn collection_borrow_external_store_rejects_callee_local_references() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_external_store.spark",
            args,
        );
        assert_eq!(result.exit_code, 1, "{args:?}: {}", result.stderr);
        assert!(
            result
                .stderr
                .contains("stored value contains a reference to local storage"),
            "{args:?}: {}",
            result.stderr
        );
    }
}

#[test]
fn collection_borrow_external_store_preserves_incoming_references() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "run-pass/collections/collection_borrow_external_input_store.spark",
            args,
        );
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "7\n", "{args:?}");
    }
}

#[test]
fn collection_borrow_external_store_clear_preserves_copied_payloads() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "run-pass/collections/collection_borrow_external_store_cleared.spark",
            args,
        );
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "0\n", "{args:?}");
        let result = run_spark_with_args(
            "run-pass/collections/collection_borrow_external_joined_clear.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 0,
            "joined clear {args:?}: {}",
            result.stderr
        );
        assert_eq!(result.stdout, "0\n", "joined clear {args:?}");
        for (fixture, message) in [
            (
                "compile-fail/borrow_errors/collection_borrow_external_partial_replacement.spark",
                "returned value contains a reference to local storage",
            ),
            (
                "compile-fail/borrow_errors/collection_borrow_external_snapshot_escape.spark",
                "returned value contains a reference to local storage",
            ),
            (
                "compile-fail/borrow_errors/collection_borrow_external_early_exit.spark",
                "stored value contains a reference to local storage",
            ),
            (
                "compile-fail/borrow_errors/collection_borrow_external_live_source_write.spark",
                "cannot write to a borrowed collection",
            ),
        ] {
            let result = build_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result.stderr.contains(message),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn collection_borrow_helper_writes_preserve_caller_lifetimes() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "run-pass/collections/collection_borrow_helper_write_input.spark",
            "run-pass/collections/collection_borrow_helper_write_recursive_input.spark",
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, "7\n", "{fixture} {args:?}");
        }
        for fixture in [
            "compile-fail/borrow_errors/collection_borrow_helper_write_escape.spark",
            "compile-fail/borrow_errors/collection_borrow_helper_write_recursive_escape.spark",
        ] {
            let result = build_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result
                    .stderr
                    .contains("returned value contains a reference to local storage"),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn collection_borrow_helper_clear_replaces_caller_contents() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "run-pass/collections/collection_borrow_helper_clears_source.spark",
            "run-pass/collections/collection_borrow_helper_recursive_clear.spark",
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, "0\n", "{fixture} {args:?}");
        }
        let result = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_helper_partial_clear_escape.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 1,
            "partial clear {args:?}: {}",
            result.stderr
        );
        assert!(
            result
                .stderr
                .contains("returned value contains a reference to local storage"),
            "partial clear {args:?}: {}",
            result.stderr
        );
        let result = build_spark_with_args(
            "build-pass/collection_borrow_nonreturning_writer.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 0,
            "nonreturning {args:?}: {}",
            result.stderr
        );
    }
}

#[test]
fn collection_borrow_indirect_writes_preserve_each_target_path() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_indirect_clear_or_keep.spark",
            args,
        );
        assert_eq!(result.exit_code, 1, "{args:?}: {}", result.stderr);
        assert!(
            result
                .stderr
                .contains("cannot write to a borrowed collection"),
            "{args:?}: {}",
            result.stderr
        );
        let result = run_spark_with_args(
            "run-pass/collections/collection_borrow_indirect_all_clear.spark",
            args,
        );
        assert_eq!(result.exit_code, 0, "all clear {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "0\n", "all clear {args:?}");
    }
}

#[test]
fn collection_borrow_imported_writes_preserve_demand_and_prefix_reads() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, message) in [
            (
                "compile-fail/borrow_errors/collection_borrow_helper_write_live_source.spark",
                "cannot write to a borrowed collection",
            ),
            (
                "compile-fail/borrow_errors/collection_borrow_helper_write_live_sibling.spark",
                "cannot write to a borrowed collection",
            ),
            (
                "compile-fail/borrow_errors/collection_borrow_nonreturning_prefix_read.spark",
                "borrowed value does not live long enough",
            ),
        ] {
            let result = build_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result.stderr.contains(message),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
        let result = run_spark_with_args(
            "run-pass/collections/collection_borrow_helper_write_cleared_source.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 0,
            "cleared source {args:?}: {}",
            result.stderr
        );
        assert_eq!(result.stdout, "9\n0\n", "cleared source {args:?}");
    }
}

#[test]
fn collection_borrow_aliased_helper_reads_reject_written_local_references() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "compile-fail/borrow_errors/collection_borrow_unknown_alias_member_write.spark",
            "compile-fail/borrow_errors/collection_borrow_aliased_read_after_write.spark",
            "compile-fail/borrow_errors/collection_borrow_equal_alternative_targets.spark",
            "compile-fail/borrow_errors/collection_borrow_aliased_generic_recursive.spark",
            "compile-fail/borrow_errors/collection_borrow_unused_helper_escape.spark",
        ] {
            let result = build_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result
                    .stderr
                    .contains("returned value contains a reference to local storage"),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn collection_borrow_aliased_helper_reads_preserve_entry_snapshots() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, output) in [
            (
                "run-pass/collections/collection_borrow_disjoint_alternative_targets.spark",
                "9\n",
            ),
            (
                "run-pass/collections/collection_borrow_unknown_alias_incoming.spark",
                "1\n7\n",
            ),
            (
                "run-pass/collections/collection_borrow_exclusive_inputs_are_distinct.spark",
                "9\n",
            ),
            (
                "run-pass/collections/collection_borrow_aliased_write_replaces_old_input.spark",
                "9\n",
            ),
            (
                "run-pass/collections/collection_borrow_aliased_generic_clear.spark",
                "0\n",
            ),
            (
                "run-pass/collections/collection_borrow_aliased_entry_snapshot.spark",
                "1\n",
            ),
            (
                "run-pass/collections/collection_borrow_disjoint_reader.spark",
                "1\n",
            ),
            (
                "run-pass/collections/collection_borrow_aliased_reset.spark",
                "0\n1\n",
            ),
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, output, "{fixture} {args:?}");
        }
    }
}

#[test]
fn collection_borrow_alias_contexts_preserve_prefix_writes_and_distinct_diagnostics() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_aliased_nonreturning_prefix.spark",
            args,
        );
        assert_eq!(result.exit_code, 1, "prefix {args:?}: {}", result.stderr);
        assert!(
            result
                .stderr
                .contains("borrowed value does not live long enough"),
            "prefix {args:?}: {}",
            result.stderr
        );
        for (fixture, count) in [
            (
                "compile-fail/borrow_errors/collection_borrow_alias_duplicate_witness.spark",
                1,
            ),
            (
                "compile-fail/borrow_errors/collection_borrow_alias_distinct_witnesses.spark",
                2,
            ),
        ] {
            let result = build_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(
                result
                    .stderr
                    .matches("returned value contains a reference to local storage")
                    .count(),
                count,
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn collection_borrow_nested_reference_inputs_preserve_alias_relations() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, output) in [
            (
                "run-pass/collections/collection_borrow_disjoint_holder_slot.spark",
                "1\n9\n",
            ),
            (
                "run-pass/collections/collection_borrow_disjoint_pair_slots.spark",
                "9\n",
            ),
            (
                "run-pass/collections/collection_borrow_disjoint_tuple_slots.spark",
                "9\n",
            ),
            (
                "run-pass/collections/collection_borrow_disjoint_capture_slots.spark",
                "9\n",
            ),
            (
                "run-pass/collections/collection_borrow_disjoint_nested_capture_slots.spark",
                "9\n",
            ),
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, output, "{fixture} {args:?}");
        }
        for fixture in [
            "compile-fail/borrow_errors/collection_borrow_unknown_alias_member_write.spark",
            "compile-fail/borrow_errors/collection_borrow_shared_pair_slots.spark",
            "compile-fail/borrow_errors/collection_borrow_shared_tuple_slots.spark",
            "compile-fail/borrow_errors/collection_borrow_shared_capture_slots.spark",
            "compile-fail/borrow_errors/collection_borrow_shared_nested_capture_slots.spark",
        ] {
            let result = build_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result
                    .stderr
                    .contains("returned value contains a reference to local storage"),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn collection_borrow_nested_snapshots_separate_pointer_values_from_pointee_payloads() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "run-pass/collections/collection_borrow_holder_snapshot_after_clear.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 0,
            "holder snapshot {args:?}: {}",
            result.stderr
        );
        assert_eq!(result.stdout, "0\n", "holder snapshot {args:?}");
        let result = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_map_snapshot_before_clear.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 1,
            "map snapshot {args:?}: {}",
            result.stderr
        );
        assert!(
            result
                .stderr
                .contains("returned value contains a reference to local storage"),
            "map snapshot {args:?}: {}",
            result.stderr
        );
    }
}

#[test]
fn collection_borrow_rebinding_holder_preserves_old_pointee_storage() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_rebound_holder_keeps_old_map.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 1,
            "retained old map {args:?}: {}",
            result.stderr
        );
        assert!(
            result
                .stderr
                .contains("stored value contains a reference to local storage"),
            "retained old map {args:?}: {}",
            result.stderr
        );
        let demand = build_spark_with_args("compile-fail/borrow_errors/collection_borrow_rebound_holder_preserves_old_demand.spark", args);
        assert_eq!(
            demand.exit_code, 1,
            "old pointee demand {args:?}: {}",
            demand.stderr
        );
        assert!(
            demand
                .stderr
                .contains("cannot write to a borrowed collection"),
            "old pointee demand {args:?}: {}",
            demand.stderr
        );
        let result = run_spark_with_args(
            "run-pass/collections/collection_borrow_cleared_map_before_holder_rebind.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 0,
            "cleared old map {args:?}: {}",
            result.stderr
        );
        assert_eq!(result.stdout, "0\n", "cleared old map {args:?}");
    }
}

#[test]
fn collection_borrow_owned_field_borrows_preserve_copied_values() {
    for args in [&[][..], &["--no-opt"][..]] {
        let fixture = "run-pass/collections/collection_borrow_parent_field_borrowed_copy.spark";
        let result = run_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
        assert_eq!(result.stdout, "9\n", "{fixture} {args:?}");
    }
}

#[test]
fn collection_borrow_owned_field_clear_releases_stored_local_references() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "run-pass/collections/collection_borrow_owned_field_clear.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 0,
            "owned-field clear {args:?}: {}",
            result.stderr
        );
        assert_eq!(result.stdout, "0\n", "owned-field clear {args:?}");
    }
}

#[test]
fn collection_borrow_owned_field_updates_preserve_helpers_siblings_and_cfg_paths() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, output) in [
            (
                "run-pass/collections/collection_borrow_owned_field_helper_clear.spark",
                "0\n0\n",
            ),
            (
                "run-pass/collections/collection_borrow_owned_field_preserves_sibling.spark",
                "9\n",
            ),
            (
                "run-pass/collections/collection_borrow_owned_field_cfg_clear.spark",
                "0\n0\n",
            ),
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, output, "{fixture} {args:?}");
        }
        for (fixture, message) in [
            ("compile-fail/borrow_errors/collection_borrow_owned_field_escape.spark", "stored value contains a reference to local storage"),
            ("compile-fail/borrow_errors/collection_borrow_owned_field_snapshot_before_clear.spark", "returned value contains a reference to local storage"),
            ("compile-fail/borrow_errors/collection_borrow_owned_field_sibling_escape.spark", "stored value contains a reference to local storage"),
            ("compile-fail/borrow_errors/collection_borrow_owned_field_partial_clear.spark", "stored value contains a reference to local storage"),
        ] {
            let result = build_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(result.stderr.contains(message), "{fixture} {args:?}: {}", result.stderr);
        }
    }
}

#[test]
fn collection_borrow_owned_subfield_writes_preserve_demand_and_generic_layouts() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, output) in [
            (
                "run-pass/collections/collection_borrow_owned_tuple_helper_clear.spark",
                "0\n9\n",
            ),
            (
                "run-pass/collections/collection_borrow_owned_field_cleared_demand.spark",
                "0\n",
            ),
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, output, "{fixture} {args:?}");
        }
        let result = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_owned_field_active_demand.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 1,
            "active field demand {args:?}: {}",
            result.stderr
        );
        assert!(
            result
                .stderr
                .contains("cannot write to a borrowed collection"),
            "active field demand {args:?}: {}",
            result.stderr
        );
    }
}

#[test]
fn collection_borrow_widened_reference_writes_reach_possible_root_readers() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "run-pass/collections/collection_borrow_widened_incoming_reference.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 0,
            "safe incoming widened reference {args:?}: {}",
            result.stderr
        );
        assert_eq!(
            result.stdout, "7\n",
            "safe incoming widened reference {args:?}"
        );
        for (fixture, output) in [
            (
                "run-pass/collections/collection_borrow_widened_disjoint_reader.spark",
                "9\n",
            ),
            (
                "run-pass/collections/collection_borrow_root_clear_widened_reader.spark",
                "0\n1\n",
            ),
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, output, "{fixture} {args:?}");
        }
        let result = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_widened_write_root_reader.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 1,
            "widened/root alias {args:?}: {}",
            result.stderr
        );
        assert!(
            result
                .stderr
                .contains("returned value contains a reference to local storage"),
            "widened/root alias {args:?}: {}",
            result.stderr
        );
    }
}

#[test]
fn collection_borrow_root_writes_reach_possible_widened_readers() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "run-pass/collections/collection_borrow_root_write_widened_incoming.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 0,
            "safe incoming root/family references {args:?}: {}",
            result.stderr
        );
        assert_eq!(
            result.stdout, "7\n9\n",
            "safe incoming root/family references {args:?}"
        );
        let result = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_root_write_widened_reader.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 1,
            "root/widened alias {args:?}: {}",
            result.stderr
        );
        assert!(
            result
                .stderr
                .contains("returned value contains a reference to local storage"),
            "root/widened alias {args:?}: {}",
            result.stderr
        );
    }
}

#[test]
fn collection_borrow_widened_effects_preserve_forwarding_snapshots_and_residual_cells() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, output) in [
            ("run-pass/collections/collection_borrow_widened_forwarded_disjoint.spark", "9\n"),
            ("run-pass/collections/collection_borrow_widened_forwarded_clear.spark", "0\n1\n"),
            ("run-pass/collections/collection_borrow_widened_snapshot_before_write.spark", "9\n"),
            ("run-pass/collections/collection_borrow_widened_residual_excludes_cleared_root.spark", "0\n"),
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, output, "{fixture} {args:?}");
        }
        for fixture in [
            "compile-fail/borrow_errors/collection_borrow_widened_forwarded_escape.spark",
            "compile-fail/borrow_errors/collection_borrow_root_widened_forwarded_escape.spark",
            "compile-fail/borrow_errors/collection_borrow_widened_residual_keeps_other_map.spark",
        ] {
            let result = build_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result
                    .stderr
                    .contains("returned value contains a reference to local storage"),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn collection_borrow_families_preserve_disjoint_bridges_and_source_demand() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, output) in [
            (
                "run-pass/collections/collection_borrow_family_bridge_disjoint.spark",
                "1\n9\n",
            ),
            (
                "run-pass/collections/collection_borrow_family_bridge_incoming.spark",
                "1\n7\n",
            ),
            (
                "run-pass/collections/collection_borrow_family_write_cleared_demand.spark",
                "0\n9\n",
            ),
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, output, "{fixture} {args:?}");
        }
        for (fixture, message) in [
            (
                "compile-fail/borrow_errors/collection_borrow_family_bridge_escape.spark",
                "returned value contains a reference to local storage",
            ),
            (
                "compile-fail/borrow_errors/collection_borrow_family_write_active_demand.spark",
                "cannot write to a borrowed collection",
            ),
        ] {
            let result = build_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result.stderr.contains(message),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn collection_borrow_families_preserve_generic_map_and_recursive_effects() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "run-pass/collections/collection_borrow_family_generic_disjoint.spark",
            "run-pass/collections/collection_borrow_family_map_value_disjoint.spark",
            "run-pass/collections/collection_borrow_family_recursive_disjoint.spark",
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, "9\n", "{fixture} {args:?}");
        }
        for fixture in [
            "compile-fail/borrow_errors/collection_borrow_family_generic_escape.spark",
            "compile-fail/borrow_errors/collection_borrow_family_map_value_escape.spark",
            "compile-fail/borrow_errors/collection_borrow_family_recursive_escape.spark",
        ] {
            let result = build_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result
                    .stderr
                    .contains("returned value contains a reference to local storage"),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

#[test]
fn collection_borrow_family_loops_keep_distinct_targets_after_one_root_is_cleared() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, output) in [
            (
                "run-pass/collections/collection_borrow_family_loop_clear.spark",
                "0\n",
            ),
            (
                "run-pass/collections/collection_borrow_family_loop_other_incoming.spark",
                "7\n",
            ),
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, output, "{fixture} {args:?}");
        }
        let result = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_family_loop_other_escape.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 1,
            "uncleared loop target {args:?}: {}",
            result.stderr
        );
        assert!(
            result
                .stderr
                .contains("returned value contains a reference to local storage"),
            "uncleared loop target {args:?}: {}",
            result.stderr
        );
    }
}

#[test]
fn collection_borrow_recursive_tails_preserve_family_targets() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, output) in [
            (
                "run-pass/collections/collection_borrow_family_tail_disjoint.spark",
                "9\n",
            ),
            (
                "run-pass/collections/collection_borrow_family_tail_shared_incoming.spark",
                "7\n",
            ),
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, output, "{fixture} {args:?}");
        }
        let result = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_family_tail_shared_escape.spark",
            args,
        );
        assert_eq!(
            result.exit_code, 1,
            "recursive tail escape {args:?}: {}",
            result.stderr
        );
        assert!(
            result
                .stderr
                .contains("returned value contains a reference to local storage"),
            "recursive tail escape {args:?}: {}",
            result.stderr
        );
    }
}

#[test]
fn collection_borrow_nonreturning_family_calls_preserve_prefix_reads() {
    for args in [&[][..], &["--no-opt"][..]] {
        let invalid = build_spark_with_args("compile-fail/borrow_errors/collection_borrow_family_nonreturn_prefix_invalid_owner.spark", args);
        assert_eq!(
            invalid.exit_code, 1,
            "invalidated prefix owner {args:?}: {}",
            invalid.stderr
        );
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "invalidated prefix owner {args:?}: {}",
            invalid.stderr
        );
        let live = build_spark_with_args(
            "run-pass/collections/collection_borrow_family_nonreturn_prefix_live_owner.spark",
            args,
        );
        assert_eq!(
            live.exit_code, 0,
            "live nonreturning owner {args:?}: {}",
            live.stderr
        );
        let recursive = run_spark_with_args(
            "run-pass/collections/collection_borrow_family_recursive_prefix_live_owner.spark",
            args,
        );
        assert_eq!(
            recursive.exit_code, 0,
            "live recursive owner {args:?}: {}",
            recursive.stderr
        );
        assert_eq!(recursive.stdout, "5\n5\n", "live recursive owner {args:?}");
    }
}

fn assert_native_borrow_error_fixture(native: &str, fixture: &str, message: &str) {
    let (_dir, source) = prepare_native_fixture(native, fixture);
    let spark = Path::new(env!("CARGO_BIN_EXE_spark"));
    for args in [&[][..], &["--no-opt"][..]] {
        let output = Command::new(spark)
            .args(["build", "--single-file"])
            .args(args)
            .arg(&source)
            .arg("-o")
            .arg(source.with_extension("program"))
            .env(
                "SPARK_RUNTIME_DIR",
                spark.parent().expect("driver directory"),
            )
            .output()
            .expect("build native borrow fixture");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(1),
            "native borrow error {args:?}: {stderr}"
        );
        assert!(
            stderr.contains(message),
            "native borrow error {args:?}: {stderr}"
        );
    }
}

#[test]
fn collection_borrow_unproven_destinations_reject_local_references() {
    let native = "void *opaque_map(void *slot) { return slot; }\n";
    for fixture in [
        include_str!("compile-fail/borrow_errors/collection_borrow_opaque_store.spark"),
        include_str!("compile-fail/borrow_errors/collection_borrow_opaque_imported_memory.spark"),
        include_str!("compile-fail/borrow_errors/collection_borrow_opaque_forwarded_source.spark"),
        include_str!("compile-fail/borrow_errors/collection_borrow_opaque_recursive_source.spark"),
        include_str!(
            "compile-fail/borrow_errors/collection_borrow_opaque_before_state_source.spark"
        ),
        include_str!(
            "compile-fail/borrow_errors/collection_borrow_opaque_snapshot_after_clear.spark"
        ),
        include_str!("compile-fail/borrow_errors/collection_borrow_opaque_mixed_destination.spark"),
        include_str!("compile-fail/borrow_errors/collection_borrow_opaque_borrowed_callback.spark"),
        include_str!("compile-fail/borrow_errors/collection_borrow_opaque_nonreturn_prefix.spark"),
        include_str!(
            "compile-fail/borrow_errors/collection_borrow_opaque_imported_nonreturn.spark"
        ),
    ] {
        assert_native_borrow_error_fixture(native, fixture, "unproven destination");
    }
    for fixture in [
        include_str!("run-pass/collections/collection_borrow_opaque_owned_values.spark"),
        include_str!("run-pass/collections/collection_borrow_opaque_borrowed_for_call.spark"),
        include_str!("run-pass/collections/collection_borrow_opaque_imported_owned_callback.spark"),
        include_str!("run-pass/collections/collection_borrow_opaque_mixed_owned_values.spark"),
        include_str!("run-pass/collections/collection_borrow_opaque_owned_callback.spark"),
    ] {
        assert_native_ownership_fixture(native, fixture, "7");
    }
    for args in [&[][..], &["--no-opt"][..]] {
        let known = run_spark_with_args(
            "run-pass/collections/collection_borrow_known_temporary_store.spark",
            args,
        );
        assert_eq!(
            known.exit_code, 0,
            "known temporary store {args:?}: {}",
            known.stderr
        );
        assert_eq!(known.stdout, "7\n0\n", "known temporary store {args:?}");
    }
}

#[test]
fn collection_borrow_loops_preserve_occurrence_lifetimes() {
    for args in [&[][..], &["--no-opt"][..]] {
        let negative = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_loop_retained_reissue.spark",
            args,
        );
        assert_eq!(negative.exit_code, 1, "{args:?}: {}", negative.stderr);
        assert!(
            negative
                .stderr
                .contains("cannot write to a borrowed collection"),
            "{args:?}: {}",
            negative.stderr
        );
        let positive = run_spark_with_args(
            "run-pass/collections/collection_borrow_loop_cleared_reissue.spark",
            args,
        );
        assert_eq!(positive.exit_code, 0, "{args:?}: {}", positive.stderr);
        assert_eq!(positive.stdout, "2\n2\n", "{args:?}");
    }
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
fn pipeline_first_input_balances_borrowed_value_owners() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\nint64_t probe_list_rc(const void *const *value) { return *(const size_t *)*value; }\nint64_t probe_string_rc(const void *const *value) { return *(const size_t *)*value; }\n",
        include_str!("run-pass/to/pipeline_first_ownership.spark"),
        "3\n3\n0\n1\n3\n0\n7\n0\n0\n9\n-1\n1\n1\n0",
    );
}

#[test]
fn pipeline_first_input_rejects_aliasing_implicit_mutable_borrows() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "compile-fail/borrow_errors/pipeline_alias_mutable_input.spark",
            args,
        );
        assert_eq!(result.exit_code, 1, "{args:?}: {}", result.stderr);
        assert!(
            result
                .stderr
                .contains("cannot borrow value as mutable more than once"),
            "{args:?}: {}",
            result.stderr
        );
    }
}

#[test]
fn pipeline_borrowed_scalar_writes_reach_the_original_local() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args("run-pass/to/pipeline_borrowed_scalar.spark", args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(
            result.stdout, "10\n10\n13\n10\n255\ntrue\ntrue\n10\n10\n5\n5\n8\n5\n9\n",
            "{args:?}"
        );
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
    for args in [&[][..], &["--no-opt"][..]] {
        let result = build_spark_with_args(fixture, args);
        assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
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

fn prepare_native_fixture(native: &str, fixture: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("create native fixture directory");
    let probe = dir.path().join("probe.c");
    std::fs::write(&probe, native).expect("write native fixture source");
    for (tool, args) in [
        ("cc", &["-c", "probe.c", "-o", "probe.o"][..]),
        ("ar", &["rcs", "libunion_probe.a", "probe.o"][..]),
    ] {
        let result = Command::new(tool)
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("build native fixture library");
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
    .expect("write native Spark fixture");

    (dir, source)
}

fn assert_native_ownership_fixture(native: &str, fixture: &str, expected: &str) {
    let (_dir, source) = prepare_native_fixture(native, fixture);
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

// TAG [SPARK-114]
#[test]
fn collection_borrow_keeps_each_callback_target_with_its_own_captures() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/collections/collection_borrow_target_owned_captures.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout, "5\n1\n", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_target_owned_capture_read.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_helper_callback_observes_captured_storage() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/collections/collection_borrow_helper_owned_callback.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout, "5\n");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_helper_callback_read.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_helper_callback_preserves_writes_and_clears() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, expected) in [
            (
                "run-pass/collections/collection_borrow_helper_callback_incoming.spark",
                "7\n",
            ),
            (
                "run-pass/collections/collection_borrow_helper_callback_clear.spark",
                "0\n",
            ),
        ] {
            let safe = run_spark_with_args(fixture, args);
            assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout, expected, "{fixture} {args:?}");
        }
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_helper_callback_write.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid.stderr.contains("reference to local storage"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_recursive_callback_preserves_capture_observations() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/collections/collection_borrow_recursive_owned_callback.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout, "5\n");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_recursive_callback_read.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_callback_effects_preserve_alias_ordering() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, expected) in [
            (
                "run-pass/collections/collection_borrow_callback_disjoint.spark",
                "8\n",
            ),
            (
                "run-pass/collections/collection_borrow_callback_snapshot.spark",
                "8\n",
            ),
            (
                "run-pass/collections/collection_borrow_callback_alias_incoming.spark",
                "7\n",
            ),
        ] {
            let safe = run_spark_with_args(fixture, args);
            assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout, expected, "{fixture} {args:?}");
        }
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_callback_alias.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid.stderr.contains("reference to local storage"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_callback_forwarding_preserves_owned_and_inert_values() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/collections/collection_borrow_callback_forwarding.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout, "5\n7\n1\n");
        let owned = build_spark_with_args(
            "run-pass/collections/collection_borrow_callback_nonreturn_owned.spark",
            args,
        );
        assert_eq!(owned.exit_code, 0, "{args:?}: {}", owned.stderr);
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_callback_nonreturn_read.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_unknown_callback_requires_proven_effects() {
    let native = "void *opaque_callback(void *slot) { return slot; }\nvoid inspect(void *target, void *value) {}\n";
    assert_native_ownership_fixture(
        native,
        include_str!("run-pass/collections/collection_borrow_callback_borrowed_for_call.spark"),
        "7",
    );
    assert_native_ownership_fixture(
        native,
        include_str!("run-pass/collections/collection_borrow_foreign_arguments_for_call.spark"),
        "7",
    );
    assert_native_borrow_error_fixture(
        native,
        include_str!(
            "compile-fail/borrow_errors/collection_borrow_unknown_callback_arguments.spark"
        ),
        "cannot verify effects of a callback on borrowed storage",
    );
    assert_native_borrow_error_fixture(
        native,
        include_str!("compile-fail/borrow_errors/collection_borrow_unknown_callback_write.spark"),
        "cannot verify effects of a callback on borrowed storage",
    );
    assert_native_borrow_error_fixture(
        native,
        include_str!(
            "compile-fail/borrow_errors/collection_borrow_unknown_callback_nonreturn.spark"
        ),
        "cannot verify effects of a callback on borrowed storage",
    );
    for fixture in [
        include_str!("compile-fail/borrow_errors/collection_borrow_unknown_callback_forwarded_arguments.spark"),
        include_str!("compile-fail/borrow_errors/collection_borrow_unknown_callback_nonreturn_arguments.spark"),
    ] {
        assert_native_borrow_error_fixture(native, fixture, "cannot verify effects of a callback on borrowed storage");
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_open_inputs_keep_known_callback_precision() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, expected) in [
            (
                "run-pass/collections/collection_borrow_open_local_argument.spark",
                "7\n",
            ),
            (
                "run-pass/collections/collection_borrow_open_safe_return.spark",
                "9\n",
            ),
            (
                "run-pass/collections/collection_borrow_open_factory.spark",
                "7\n",
            ),
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout, expected, "{fixture} {args:?}");
        }
        for fixture in [
            "compile-fail/borrow_errors/collection_borrow_open_identity_return.spark",
            "compile-fail/borrow_errors/collection_borrow_open_unconditional_return.spark",
        ] {
            let invalid = build_spark_with_args(fixture, args);
            assert_eq!(
                invalid.exit_code, 1,
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
            assert!(
                invalid.stderr.contains("cannot return reference to local"),
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_reference_callback_preserves_writes() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/collections/collection_borrow_ref_callback_incoming.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout, "7\n");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_ref_callback_write.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid.stderr.contains("reference to local"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_open_function_results_preserve_target_roles() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/collections/collection_borrow_open_function_result.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout, "9\n");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_open_function_escape.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid.stderr.contains("reference to local"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_nested_callbacks_keep_capture_lifetimes() {
    let cases = [
        (
            include_str!("run-pass/collections/collection_borrow_nested_owned_callback.spark"),
            true,
        ),
        (
            include_str!("run-pass/collections/collection_borrow_nested_scoped_callback.spark"),
            true,
        ),
        (
            include_str!(
                "compile-fail/borrow_errors/collection_borrow_nested_callback_owner.spark"
            ),
            false,
        ),
    ];
    let dir = tempfile::tempdir().expect("create nested callback directory");
    for (case, (template, valid)) in cases.into_iter().enumerate() {
        for depth in 0..=6 {
            let mut callback = "original".to_string();
            let mut wrappers = String::new();
            for level in 1..=depth {
                wrappers.push_str(&format!(
                    "        wrapped{level}: (): Int = nest({callback})\n"
                ));
                callback = format!("wrapped{level}");
            }
            let start = template
                .find("        once:")
                .expect("wrapper declarations");
            let end = template
                .find("        entries:")
                .expect("carrier declaration");
            let mut source = template.to_string();
            source.replace_range(start..end, &wrappers);
            source = source.replace("{1: three}", &format!("{{1: {callback}}}"));
            let path = dir.path().join(format!("case{case}_depth{depth}.spark"));
            std::fs::write(&path, source).expect("write nested callback source");
            let fixture = path.to_str().expect("UTF-8 source path");
            for args in [&[][..], &["--no-opt"][..]] {
                if valid {
                    let safe = run_spark_with_args(fixture, args);
                    assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
                    assert_eq!(safe.stdout, "5\n", "{fixture} {args:?}");
                } else {
                    let invalid = build_spark_with_args(fixture, args);
                    assert_eq!(
                        invalid.exit_code, 1,
                        "{fixture} {args:?}: {}",
                        invalid.stderr
                    );
                    assert!(
                        invalid
                            .stderr
                            .contains("borrowed value does not live long enough"),
                        "{fixture} {args:?}: {}",
                        invalid.stderr
                    );
                }
            }
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_nested_callbacks_preserve_memory_effects() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, expected) in [
            (
                "run-pass/collections/collection_borrow_nested_callback_incoming.spark",
                "7\n",
            ),
            (
                "run-pass/collections/collection_borrow_nested_callback_reference_incoming.spark",
                "7\n",
            ),
            (
                "run-pass/collections/collection_borrow_nested_callback_clear.spark",
                "0\n",
            ),
            (
                "run-pass/collections/collection_borrow_nested_callback_overwrite.spark",
                "7\n",
            ),
            (
                "run-pass/collections/collection_borrow_nested_callback_same_type.spark",
                "8\n7\n",
            ),
        ] {
            let safe = run_spark_with_args(fixture, args);
            assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout, expected, "{fixture} {args:?}");
        }
        for fixture in [
            "compile-fail/borrow_errors/collection_borrow_nested_callback_write.spark",
            "compile-fail/borrow_errors/collection_borrow_nested_callback_reference_write.spark",
            "compile-fail/borrow_errors/collection_borrow_nested_callback_snapshot.spark",
        ] {
            let invalid = build_spark_with_args(fixture, args);
            assert_eq!(
                invalid.exit_code, 1,
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
            assert!(
                invalid.stderr.contains("reference to local storage"),
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_callback_instances_do_not_gain_alias_identity_from_their_schema() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, expected) in [
            (
                "run-pass/collections/collection_borrow_callback_instances_disjoint.spark",
                "0\n8\n",
            ),
            (
                "run-pass/collections/collection_borrow_callback_instances_same_target.spark",
                "0\n0\n",
            ),
            (
                "run-pass/collections/collection_borrow_callback_instances_list.spark",
                "0\n8\n",
            ),
            (
                "run-pass/collections/collection_borrow_callback_instances_list_same_target.spark",
                "0\n0\n",
            ),
        ] {
            let safe = run_spark_with_args(fixture, args);
            assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout, expected, "{fixture} {args:?}");
        }
        for fixture in [
            "compile-fail/borrow_errors/collection_borrow_callback_instances_partial_clear.spark",
            "compile-fail/borrow_errors/collection_borrow_callback_instances_list_partial_clear.spark",
        ] {
            let invalid = build_spark_with_args(fixture, args);
            assert_eq!(invalid.exit_code, 1, "{fixture} {args:?}: {}", invalid.stderr);
            assert!(invalid.stderr.contains("reference to local storage"), "{fixture} {args:?}: {}", invalid.stderr);
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_nested_function_references_preserve_targets_and_capture_lifetimes() {
    let cases = [
        (
            include_str!("run-pass/collections/collection_borrow_nested_ref_callback_scoped.spark"),
            true,
        ),
        (
            include_str!("run-pass/collections/collection_borrow_nested_ref_callback_owned.spark"),
            true,
        ),
        (
            include_str!(
                "compile-fail/borrow_errors/collection_borrow_nested_ref_callback_owner.spark"
            ),
            false,
        ),
        (
            include_str!(
                "compile-fail/borrow_errors/collection_borrow_nested_ref_callback_source.spark"
            ),
            false,
        ),
    ];
    let dir = tempfile::tempdir().expect("create referenced callback directory");
    for (case, (template, valid)) in cases.into_iter().enumerate() {
        for depth in 0..=6 {
            let mut callback = "once".to_string();
            let mut wrappers = String::new();
            for level in 1..=depth {
                wrappers.push_str(&format!(
                    "        wrapped{level}: (): Int = nest({callback})\n"
                ));
                callback = format!("wrapped{level}");
            }
            let start = template
                .find("        twice:")
                .expect("wrapper declarations");
            let end = template
                .find("        entries:")
                .expect("carrier declaration");
            let mut source = template.to_string();
            source.replace_range(start..end, &wrappers);
            source = source.replace("{1: three}", &format!("{{1: {callback}}}"));
            let path = dir.path().join(format!("case{case}_depth{depth}.spark"));
            std::fs::write(&path, source).expect("write referenced callback source");
            let fixture = path.to_str().expect("UTF-8 source path");
            for args in [&[][..], &["--no-opt"][..]] {
                if valid {
                    let safe = run_spark_with_args(fixture, args);
                    assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
                    assert_eq!(safe.stdout, "5\n", "{fixture} {args:?}");
                } else {
                    let invalid = build_spark_with_args(fixture, args);
                    assert_eq!(
                        invalid.exit_code, 1,
                        "{fixture} {args:?}: {}",
                        invalid.stderr
                    );
                    assert!(
                        invalid
                            .stderr
                            .contains("borrowed value does not live long enough")
                            || invalid.stderr.contains(
                                "cannot verify effects of a callback on borrowed storage"
                            ),
                        "{fixture} {args:?}: {}",
                        invalid.stderr
                    );
                }
            }
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_captured_payload_reads_preserve_the_reference_root() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/collections/collection_borrow_captured_payload_incoming.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout, "7\n", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_captured_payload_escape.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid.stderr.contains("reference to local storage"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_captured_function_payloads_keep_the_pointed_to_capture_sources() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/collections/collection_borrow_captured_function_payload_incoming.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout, "7\n", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/collection_borrow_captured_function_payload_escape.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid.stderr.contains("reference to local storage"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_channel_sends_preserve_payload_lifetimes() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "run-pass/channels/channel_borrow_incoming.spark",
            "run-pass/channels/channel_borrow_same_scope.spark",
            "run-pass/channels/channel_borrow_owned_payload.spark",
        ] {
            let safe = run_spark_with_args(fixture, args);
            assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout, "7\n", "{fixture} {args:?}");
        }
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/channel_borrow_local_payload.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid.stderr.contains("reference to local storage"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_shared_and_returned_channels_preserve_payloads() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "run-pass/channels/channel_borrow_returned_incoming.spark",
            "run-pass/channels/channel_borrow_copied_handles.spark",
            "run-pass/channels/channel_borrow_same_queue_ports.spark",
        ] {
            let safe = run_spark_with_args(fixture, args);
            assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout, "7\n", "{fixture} {args:?}");
        }
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/channel_borrow_returned_local.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid.stderr.contains("reference to local storage"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_channel_helper_aliases_preserve_read_order() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "run-pass/channels/channel_borrow_disjoint_helper.spark",
            "run-pass/channels/channel_borrow_read_before_send.spark",
        ] {
            let safe = run_spark_with_args(fixture, args);
            assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout, "9\n", "{fixture} {args:?}");
        }
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/channel_borrow_aliased_helper_return.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("cannot return reference to local variable"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_channels_preserve_publication_and_source_lifetimes() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, stdout) in [
            (
                "run-pass/channels/channel_borrow_external_store_incoming.spark",
                "7\n",
            ),
            (
                "run-pass/channels/channel_borrow_scoped_string.spark",
                "seven\n",
            ),
        ] {
            let safe = run_spark_with_args(fixture, args);
            assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout, stdout, "{fixture} {args:?}");
        }
        for (fixture, diagnostic) in [
            (
                "compile-fail/borrow_errors/channel_borrow_nonreturning_send.spark",
                "reference to local storage",
            ),
            (
                "compile-fail/borrow_errors/channel_borrow_published_empty.spark",
                "reference to local storage",
            ),
            (
                "compile-fail/borrow_errors/channel_borrow_external_store.spark",
                "reference to local storage",
            ),
            (
                "compile-fail/borrow_errors/channel_borrow_dropped_string.spark",
                "borrowed value does not live long enough",
            ),
            (
                "compile-fail/borrow_errors/channel_borrow_replaced_list.spark",
                "cannot assign to a borrowed value",
            ),
        ] {
            let invalid = build_spark_with_args(fixture, args);
            assert_eq!(
                invalid.exit_code, 1,
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
            assert!(
                invalid.stderr.contains(diagnostic),
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_opaque_channels_preserve_payload_support() {
    let native = include_str!("run-pass/channels/copy_channel.c");
    assert_native_ownership_fixture(
        native,
        include_str!("run-pass/channels/channel_borrow_opaque_incoming.spark"),
        "9",
    );
    assert_native_borrow_error_fixture(
        native,
        include_str!("compile-fail/borrow_errors/channel_borrow_opaque_return.spark"),
        "cannot return reference to local variable",
    );
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_nested_callbacks_preserve_channel_payload_reads() {
    let cases = [
        (
            include_str!("run-pass/channels/channel_borrow_nested_callback_incoming.spark"),
            true,
        ),
        (
            include_str!("compile-fail/borrow_errors/channel_borrow_nested_callback_local.spark"),
            false,
        ),
        (
            include_str!("run-pass/channels/channel_borrow_nested_channel_pointer.spark"),
            true,
        ),
        (
            include_str!("compile-fail/borrow_errors/channel_borrow_nested_channel_pointer.spark"),
            false,
        ),
        (
            include_str!("run-pass/channels/channel_borrow_queued_map_pointee.spark"),
            true,
        ),
        (
            include_str!("compile-fail/borrow_errors/channel_borrow_queued_map_pointee.spark"),
            false,
        ),
    ];
    let dir = tempfile::tempdir().expect("create queue callback directory");
    for (case, (template, valid)) in cases.into_iter().enumerate() {
        for depth in 0..=6 {
            let mut callback = "read".to_string();
            for _ in 0..depth {
                callback = format!("nest({callback})");
            }
            let path = dir.path().join(format!("case{case}_depth{depth}.spark"));
            std::fs::write(&path, template.replace("nest(nest(read))", &callback))
                .expect("write queue callback source");
            let fixture = path.to_str().expect("UTF-8 source path");
            for args in [&[][..], &["--no-opt"][..]] {
                if valid {
                    let safe = run_spark_with_args(fixture, args);
                    assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
                    assert_eq!(safe.stdout, "9\n", "{fixture} {args:?}");
                } else {
                    let invalid = build_spark_with_args(fixture, args);
                    assert_eq!(
                        invalid.exit_code, 1,
                        "{fixture} {args:?}: {}",
                        invalid.stderr
                    );
                    assert!(
                        invalid
                            .stderr
                            .contains("cannot return reference to local variable"),
                        "{fixture} {args:?}: {}",
                        invalid.stderr
                    );
                }
            }
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_callbacks_distinguish_same_type_channel_captures() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/channels/channel_borrow_same_type_captures.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout, "7\n9\n", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/channel_borrow_same_type_captures.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("cannot return reference to local variable"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_queued_reference_reads_keep_nested_channel_handles() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/channels/channel_borrow_queued_channel_pointer.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout, "9\n", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/channel_borrow_queued_channel_pointer.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("cannot return reference to local variable"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_channels_preserve_nested_prefixes_and_recursive_sends() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "run-pass/channels/channel_borrow_nested_publication_incoming.spark",
            "run-pass/channels/channel_borrow_recursive_field_send.spark",
        ] {
            let safe = run_spark_with_args(fixture, args);
            assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout, "9\n", "{fixture} {args:?}");
        }
        for fixture in [
            "compile-fail/borrow_errors/channel_borrow_nested_nonreturning_send.spark",
            "compile-fail/borrow_errors/channel_borrow_recursive_field_send.spark",
        ] {
            let invalid = build_spark_with_args(fixture, args);
            assert_eq!(
                invalid.exit_code, 1,
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
            assert!(
                invalid.stderr.contains("reference to local storage"),
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_returned_factories_keep_distinct_queue_roles() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, expected) in [
            (
                "run-pass/channels/channel_borrow_distinct_factory_outputs.spark",
                "9\n",
            ),
            (
                "run-pass/channels/channel_borrow_owned_factory_outputs.spark",
                "9\n7\n",
            ),
        ] {
            let safe = run_spark_with_args(fixture, args);
            assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout, expected, "{fixture} {args:?}");
        }
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/channel_borrow_factory_output_escape.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("cannot return reference to local variable"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_opaque_queue_aliases_preserve_later_writes_and_helper_inputs() {
    let native = include_str!("run-pass/channels/copy_channel.c");
    for fixture in [
        include_str!("run-pass/channels/channel_borrow_opaque_helper_incoming.spark"),
        include_str!("run-pass/channels/channel_borrow_opaque_alias_write.spark"),
    ] {
        assert_native_ownership_fixture(native, fixture, "9");
    }
    for fixture in [
        include_str!("compile-fail/borrow_errors/channel_borrow_opaque_helper_return.spark"),
        include_str!("compile-fail/borrow_errors/channel_borrow_opaque_alias_write.spark"),
    ] {
        assert_native_borrow_error_fixture(
            native,
            fixture,
            "cannot return reference to local variable",
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_open_queue_factories_keep_validation_separate_from_opaque_effects() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/channels/channel_borrow_open_factory_local.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout, "7\n", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/channel_borrow_open_factory_shared.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid.stderr.contains("reference to local storage"),
            "{args:?}: {}",
            invalid.stderr
        );
        let metadata = run_spark_with_args(
            "run-pass/channels/channel_borrow_dead_payload_metadata.spark",
            args,
        );
        assert_eq!(metadata.exit_code, 0, "{args:?}: {}", metadata.stderr);
        assert_eq!(metadata.stdout, "1\n", "{args:?}");
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_factory_memory_outputs_preserve_roles_and_returned_aliases() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "run-pass/channels/channel_borrow_memory_factory_outputs.spark",
            "run-pass/channels/channel_borrow_owned_memory_factory.spark",
            "run-pass/channels/channel_borrow_returned_and_stored_alias.spark",
        ] {
            let safe = run_spark_with_args(fixture, args);
            assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout, "9\n", "{fixture} {args:?}");
        }
        for fixture in [
            "compile-fail/borrow_errors/channel_borrow_memory_factory_escape.spark",
            "compile-fail/borrow_errors/channel_borrow_returned_and_stored_alias.spark",
        ] {
            let invalid = build_spark_with_args(fixture, args);
            assert_eq!(
                invalid.exit_code, 1,
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
            assert!(
                invalid
                    .stderr
                    .contains("cannot return reference to local variable"),
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_channel_receives_close_and_reallocation_preserve_may_contents() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, expected) in [
            (
                "run-pass/channels/channel_borrow_buffered_after_close.spark",
                "9\n10\n",
            ),
            (
                "run-pass/channels/channel_borrow_repeated_allocation_site.spark",
                "9\n",
            ),
        ] {
            let safe = run_spark_with_args(fixture, args);
            assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout, expected, "{fixture} {args:?}");
        }
        for fixture in [
            "compile-fail/borrow_errors/channel_borrow_buffered_after_close.spark",
            "compile-fail/borrow_errors/channel_borrow_repeated_allocation_site.spark",
        ] {
            let invalid = build_spark_with_args(fixture, args);
            assert_eq!(
                invalid.exit_code, 1,
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
            assert!(
                invalid.stderr.contains("reference to local storage"),
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_channel_effects_require_resolved_builtin_methods() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/channels/channel_borrow_user_send_name.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout, "9\n", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/channel_borrow_user_send_name.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid.stderr.contains("reference to local storage"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_generic_recursive_channels_preserve_payload_provenance() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/channels/channel_borrow_generic_recursive.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout, "9\n", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/channel_borrow_generic_recursive.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("cannot return reference to local variable"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_append_preserves_caller_and_local_reference_provenance() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/collections/collection_append_incoming.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout, "9\n", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/collection_append_local.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid.stderr.contains("reference to local storage"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_append_preserves_snapshots_and_source_owner_lifetimes() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, expected) in [
            (
                "run-pass/collections/collection_append_snapshot.spark",
                "9\n",
            ),
            (
                "run-pass/collections/collection_append_scoped_string.spark",
                "seven\n",
            ),
        ] {
            let safe = run_spark_with_args(fixture, args);
            assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout, expected, "{fixture} {args:?}");
        }
        for (fixture, diagnostic) in [
            (
                "compile-fail/borrow_errors/collection_append_retained_local.spark",
                "cannot return reference to local variable",
            ),
            (
                "compile-fail/borrow_errors/collection_append_owner_drop.spark",
                "borrowed value does not live long enough",
            ),
        ] {
            let invalid = build_spark_with_args(fixture, args);
            assert_eq!(
                invalid.exit_code, 1,
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
            assert!(
                invalid.stderr.contains(diagnostic),
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_append_preserves_callback_and_channel_metadata() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "run-pass/collections/collection_append_owned_callback.spark",
            "run-pass/collections/collection_append_channel_incoming.spark",
        ] {
            let safe = run_spark_with_args(fixture, args);
            assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout, "9\n", "{fixture} {args:?}");
        }
        for fixture in [
            "compile-fail/borrow_errors/collection_append_borrowed_callback.spark",
            "compile-fail/borrow_errors/collection_append_channel_local.spark",
        ] {
            let invalid = build_spark_with_args(fixture, args);
            assert_eq!(
                invalid.exit_code, 1,
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
            assert!(
                invalid.stderr.contains("reference to local storage"),
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_joins_preserve_capture_owner_lifetimes() {
    for args in [&[][..], &["--no-opt"][..]] {
        for (fixture, expected) in [
            ("run-pass/async/future_borrow_scoped_await.spark", "5\n"),
            (
                "run-pass/async/future_borrow_scoped_implicit_join.spark",
                "5\n",
            ),
            (
                "run-pass/async/future_borrow_completed_retained.spark",
                "5\n",
            ),
            ("run-pass/async/spawn_closure.spark", "123\n"),
        ] {
            let safe = run_spark_with_args(fixture, args);
            assert_eq!(safe.exit_code, 0, "{fixture} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout, expected, "{fixture} {args:?}");
        }
        for fixture in [
            "compile-fail/borrow_errors/future_borrow_owner_drop_before_await.spark",
            "compile-fail/borrow_errors/future_borrow_owner_drop_before_implicit_join.spark",
        ] {
            let invalid = build_spark_with_args(fixture, args);
            assert_eq!(
                invalid.exit_code, 1,
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
            assert!(
                invalid
                    .stderr
                    .contains("borrowed value does not live long enough"),
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_pending_reads_do_not_require_an_eventual_join() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/async/future_borrow_dead_payload_metadata.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout, "1\n", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/future_borrow_owner_drop_before_diverging_caller.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_foreign_readers_observe_nested_references() {
    let native = include_str!("run-pass/async/future_read_refs.c");
    assert_native_ownership_fixture(
        native,
        include_str!("run-pass/async/future_borrow_foreign_scoped.spark"),
        "5",
    );
    assert_native_borrow_error_fixture(
        native,
        include_str!("compile-fail/borrow_errors/future_borrow_foreign_owner_drop.spark"),
        "borrowed value does not live long enough",
    );
    assert_native_borrow_error_fixture(
        native,
        include_str!("compile-fail/borrow_errors/future_borrow_foreign_implicit_join.spark"),
        "borrowed value does not live long enough",
    );
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_foreign_slot_replacement_detaches_copy_proof() {
    let native = include_str!("run-pass/async/future_replace.c");
    assert_native_ownership_fixture(
        native,
        include_str!("run-pass/async/future_replace_slot_helper.spark"),
        "10\n20",
    );
    assert_native_borrow_error_fixture(
        native,
        include_str!("compile-fail/borrow_errors/future_replace_slot_helper_pending.spark"),
        "borrowed value does not live long enough",
    );

    assert_native_ownership_fixture(
        native,
        include_str!("run-pass/async/future_replace_slot_nested.spark"),
        "10\n20",
    );
    assert_native_borrow_error_fixture(
        native,
        include_str!("compile-fail/borrow_errors/future_replace_slot_nested_pending.spark"),
        "borrowed value does not live long enough",
    );

    assert_native_ownership_fixture(
        native,
        include_str!("run-pass/async/future_replace_slot.spark"),
        "10\n20",
    );
    assert_native_borrow_error_fixture(
        native,
        include_str!("compile-fail/borrow_errors/future_replace_slot_pending.spark"),
        "borrowed value does not live long enough",
    );
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_helper_await_completes_caller() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "run-pass/async/future_helper_await_completes_caller.spark",
            args,
        );
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout.trim(), "5", "{args:?}: {}", result.stderr);
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_helper_returned_alias_completes_source() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "run-pass/async/future_helper_returned_alias_completes_source.spark",
            args,
        );
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout.trim(), "5", "{args:?}: {}", result.stderr);
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_helper_guarantees_cover_recursive_and_generic_calls() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "run-pass/async/future_helper_recursive_completion.spark",
            "run-pass/async/future_helper_generic_alias.spark",
            "run-pass/async/future_helper_diverging_alternative.spark",
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout.trim(), "5", "{fixture} {args:?}");
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_helper_guarantees_require_every_normal_return() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "compile-fail/borrow_errors/future_helper_conditional_does_not_complete_input.spark",
            "compile-fail/borrow_errors/future_helper_replacement_does_not_complete_input.spark",
            "compile-fail/borrow_errors/future_helper_recursive_does_not_complete_input.spark",
            "compile-fail/borrow_errors/future_helper_alternative_return_is_not_input_alias.spark",
        ] {
            let result = build_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 1, "{fixture} {args:?}: {}", result.stderr);
            assert!(
                result
                    .stderr
                    .contains("borrowed value does not live long enough"),
                "{fixture} {args:?}: {}",
                result.stderr
            );
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_helper_completion_preserves_borrowed_results() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/async/future_helper_borrowed_result_scoped.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout.trim(), "5", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/future_helper_borrowed_result_escape.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid.stderr.contains("reference to local storage"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_helper_proof_refinement_reaches_a_fixed_point() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args("run-pass/async/future_helper_proof_closure.spark", args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_tasks_publish_borrowed_queue_payloads() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/async/future_task_queue_publication_scoped.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout.trim(), "5", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_queue_publication_owner_drop.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_task_queue_effects_preserve_collection_sources_and_private_queues() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "run-pass/async/future_task_queue_list_payload_scoped.spark",
            "run-pass/async/future_task_private_queue_scoped.spark",
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout.trim(), "5", "{fixture} {args:?}");
        }
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_queue_list_payload_owner_drop.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
        let prefix = run_spark_with_args(
            "run-pass/async/future_task_queue_prefix_before_divergence.spark",
            args,
        );
        assert_eq!(prefix.exit_code, 0, "{args:?}: {}", prefix.stderr);
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_task_queue_publication_survives_helpers_and_implicit_joins() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "run-pass/async/future_task_queue_helper_scoped.spark",
            "run-pass/async/future_task_queue_implicit_join_scoped.spark",
        ] {
            let result = run_spark_with_args(fixture, args);
            assert_eq!(result.exit_code, 0, "{fixture} {args:?}: {}", result.stderr);
            assert_eq!(result.stdout.trim(), "5", "{fixture} {args:?}");
        }
        for fixture in [
            "compile-fail/borrow_errors/future_task_queue_helper_owner_drop.spark",
            "compile-fail/borrow_errors/future_task_queue_implicit_join_owner_drop.spark",
        ] {
            let invalid = build_spark_with_args(fixture, args);
            assert_eq!(
                invalid.exit_code, 1,
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
            assert!(
                invalid
                    .stderr
                    .contains("borrowed value does not live long enough"),
                "{fixture} {args:?}: {}",
                invalid.stderr
            );
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_completed_tasks_do_not_publish_again() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "run-pass/async/future_task_completed_relay_does_not_publish_again.spark",
            args,
        );
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout.trim(), "5\n0", "{args:?}");
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_private_returned_queue_has_native_layout() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args(
            "run-pass/async/future_private_returned_queue_scoped.spark",
            args,
        );
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout.trim(), "5", "{args:?}");
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_tasks_publish_late_queue_payloads() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/async/future_task_late_queue_publication_scoped.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout.trim(), "5", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_late_queue_publication_owner_drop.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_private_input_roles_survive_helper_returns() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/async/future_task_private_input_role_scoped.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout.trim(), "5", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_private_input_role_owner_drop.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_queue_closure_covers_chains_cycles_and_divergence() {
    for args in [&[][..], &["--no-opt"][..]] {
        for name in [
            "future_task_two_late_relays_scoped.spark",
            "future_task_self_queue_reaches_a_fixed_point.spark",
        ] {
            let safe = run_spark_with_args(&format!("run-pass/async/{name}"), args);
            assert_eq!(safe.exit_code, 0, "{name} {args:?}: {}", safe.stderr);
            assert_eq!(safe.stdout.trim(), "5", "{name} {args:?}");
        }
        let diverging = build_spark_with_args(
            "run-pass/async/future_task_late_diverging_prefix_scoped.spark",
            args,
        );
        assert_eq!(diverging.exit_code, 0, "{args:?}: {}", diverging.stderr);
        for name in [
            "future_task_two_late_relays_owner_drop.spark",
            "future_task_late_diverging_prefix_owner_drop.spark",
        ] {
            let invalid =
                build_spark_with_args(&format!("compile-fail/borrow_errors/{name}"), args);
            assert_eq!(invalid.exit_code, 1, "{name} {args:?}: {}", invalid.stderr);
            assert!(
                invalid
                    .stderr
                    .contains("borrowed value does not live long enough"),
                "{name} {args:?}: {}",
                invalid.stderr
            );
        }
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_helper_intermediate_writes_retain_publications() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/async/future_task_helper_intermediate_write_scoped.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout.trim(), "5", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_helper_intermediate_write_owner_drop.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_old_endpoints_are_resampled_after_retargeting() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe =
            run_spark_with_args("run-pass/async/future_task_old_endpoint_scoped.spark", args);
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout.trim(), "0\n5", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_old_endpoint_owner_drop.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_postjoin_temporary_writes_do_not_publish() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/async/future_task_postjoin_temporary_does_not_publish.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout.trim(), "0", "{args:?}");
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_prefix_memory_and_queue_effects_compose() {
    for args in [&[][..], &["--no-opt"][..]] {
        let completed = run_spark_with_args(
            "run-pass/async/future_task_postjoin_queue_does_not_publish.spark",
            args,
        );
        assert_eq!(completed.exit_code, 0, "{args:?}: {}", completed.stderr);
        assert_eq!(completed.stdout.trim(), "0", "{args:?}");
        let safe = run_spark_with_args(
            "run-pass/async/future_task_prefix_queue_supplies_endpoint_scoped.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout.trim(), "5", "{args:?}");
        let invalid = build_spark_with_args("compile-fail/borrow_errors/future_task_prefix_queue_supplies_endpoint_owner_drop.spark", args);
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
        let diverging = build_spark_with_args(
            "run-pass/async/future_task_prefix_queue_diverging_scoped.spark",
            args,
        );
        assert_eq!(diverging.exit_code, 0, "{args:?}: {}", diverging.stderr);
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_event_guard_only_retires_the_joined_observer() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/async/future_task_event_guard_keeps_other_observer_scoped.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout.trim(), "5", "{args:?}");
        let invalid = build_spark_with_args("compile-fail/borrow_errors/future_task_event_guard_keeps_other_observer_owner_drop.spark", args);
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_postjoin_relay_does_not_publish_to_the_joined_observer() {
    for args in [&[][..], &["--no-opt"][..]] {
        let safe = run_spark_with_args(
            "run-pass/async/future_task_postjoin_relay_does_not_publish.spark",
            args,
        );
        assert_eq!(safe.exit_code, 0, "{args:?}: {}", safe.stderr);
        assert_eq!(safe.stdout.trim(), "0", "{args:?}");
        let before = run_spark_with_args(
            "run-pass/async/future_task_prejoin_relay_scoped.spark",
            args,
        );
        assert_eq!(before.exit_code, 0, "{args:?}: {}", before.stderr);
        assert_eq!(before.stdout.trim(), "5", "{args:?}");
        let escaping = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_prejoin_relay_owner_drop.spark",
            args,
        );
        assert_eq!(escaping.exit_code, 1, "{args:?}: {}", escaping.stderr);
        assert!(
            escaping
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            escaping.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_dropped_tasks_keep_published_memory() {
    for (scoped, escaping) in [
        (
            "run-pass/async/future_task_drop_keeps_memory_scoped.spark",
            "compile-fail/borrow_errors/future_task_drop_keeps_memory_owner_drop.spark",
        ),
        (
            "run-pass/async/future_task_copy_drop_keeps_memory_scoped.spark",
            "compile-fail/borrow_errors/future_task_copy_drop_keeps_memory_owner_drop.spark",
        ),
        (
            "run-pass/async/future_task_replaced_root_keeps_memory_scoped.spark",
            "compile-fail/borrow_errors/future_task_replaced_root_keeps_memory_owner_drop.spark",
        ),
    ] {
        for args in [&[][..], &["--no-opt"][..]] {
            let valid = run_spark_with_args(scoped, args);
            assert_eq!(valid.exit_code, 0, "{scoped} {args:?}: {}", valid.stderr);
            assert_eq!(valid.stdout.trim(), "5", "{scoped} {args:?}");
            let invalid = build_spark_with_args(escaping, args);
            assert_eq!(
                invalid.exit_code, 1,
                "{escaping} {args:?}: {}",
                invalid.stderr
            );
            assert!(
                invalid
                    .stderr
                    .contains("borrowed value does not live long enough"),
                "{escaping} {args:?}: {}",
                invalid.stderr
            );
        }
    }
}

#[test]
fn collection_borrow_future_helper_memory_survives_scalar_returns() {
    for args in [&[][..], &["--no-opt"][..]] {
        let scoped = run_spark_with_args(
            "run-pass/async/future_task_helper_publishes_memory_scoped.spark",
            args,
        );
        assert_eq!(scoped.exit_code, 0, "{args:?}: {}", scoped.stderr);
        assert_eq!(scoped.stdout.trim(), "5", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_helper_publishes_memory_owner_drop.spark",
            args,
        );
        let local = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_helper_local_memory_escape.spark",
            args,
        );
        assert_eq!(local.exit_code, 1, "{args:?}: {}", local.stderr);
        assert!(
            local
                .stderr
                .contains("stored value contains a reference to local storage"),
            "{args:?}: {}",
            local.stderr
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

#[test]
fn collection_borrow_future_tasks_publish_borrowed_memory() {
    for args in [&[][..], &["--no-opt"][..]] {
        let scoped = run_spark_with_args(
            "run-pass/async/future_task_publishes_memory_scoped.spark",
            args,
        );
        assert_eq!(scoped.exit_code, 0, "{args:?}: {}", scoped.stderr);
        assert_eq!(scoped.stdout.trim(), "5", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_publishes_memory_owner_drop.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

#[test]
fn collection_borrow_future_returned_input_keeps_history_learned_during_the_call() {
    for args in [&[][..], &["--no-opt"][..]] {
        let scoped = run_spark_with_args(
            "run-pass/async/future_task_returned_input_history_scoped.spark",
            args,
        );
        assert_eq!(scoped.exit_code, 0, "{args:?}: {}", scoped.stderr);
        assert_eq!(scoped.stdout.trim(), "0\n5", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_returned_input_history_owner_drop.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

#[test]
fn collection_borrow_future_returned_tasks_preserve_helper_observation_history() {
    for args in [&[][..], &["--no-opt"][..]] {
        let scoped = run_spark_with_args(
            "run-pass/async/future_task_returned_history_old_endpoint_scoped.spark",
            args,
        );
        assert_eq!(scoped.exit_code, 0, "{args:?}: {}", scoped.stderr);
        assert_eq!(scoped.stdout.trim(), "0\n5", "{args:?}");
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_returned_history_old_endpoint_owner_drop.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

#[test]
fn collection_borrow_future_receive_snapshots_survive_nonreturning_outer_calls() {
    for args in [&[][..], &["--no-opt"][..]] {
        let valid = build_spark_with_args(
            "run-pass/async/future_task_tuple_leaf_nonreturning_outer.spark",
            args,
        );
        assert_eq!(valid.exit_code, 0, "{args:?}: {}", valid.stderr);
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_forwarded_nonreturning_outer.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

#[test]
fn collection_borrow_future_returning_publications_keep_tuple_leaf_provenance() {
    for fixture in [
        "run-pass/async/future_task_tuple_leaf_return_only.spark",
        "run-pass/async/future_task_tuple_leaf_same_diverging_leaf.spark",
        "run-pass/async/future_task_tuple_leaf_distinct_diverging_site.spark",
        "run-pass/async/future_task_tuple_leaf_distinct_destination.spark",
        "run-pass/async/future_task_tuple_leaf_shared_diverging_site.spark",
    ] {
        for args in [&[][..], &["--no-opt"][..]] {
            let scoped = run_spark_with_args(fixture, args);
            assert_eq!(scoped.exit_code, 0, "{fixture} {args:?}: {}", scoped.stderr);
            assert_eq!(scoped.stdout.trim(), "5", "{fixture} {args:?}");
        }
    }
}

#[test]
fn collection_borrow_future_nested_owner_returns_survive_forwarders() {
    for args in [&[][..], &["--no-opt"][..]] {
        let scoped = run_spark_with_args(
            "run-pass/async/future_task_nested_forwarder_owner_live.spark",
            args,
        );
        assert_eq!(scoped.exit_code, 0, "{args:?}: {}", scoped.stderr);
        assert_eq!(scoped.stdout.trim(), "5", "{args:?}");
        let escapes = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_nested_forwarder_frame_read.spark",
            args,
        );
        assert_eq!(escapes.exit_code, 1, "{args:?}: {}", escapes.stderr);
        assert!(
            escapes
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            escapes.stderr
        );
    }
}

#[test]
fn collection_borrow_future_forwarded_receives_keep_the_original_owner_lifetime() {
    for args in [&[][..], &["--no-opt"][..]] {
        for fixture in [
            "run-pass/async/future_task_forwarded_owner_live.spark",
            "run-pass/async/future_task_forwarded_receive_before_publication.spark",
            "run-pass/async/future_task_reused_receive_helper_keeps_earlier_value.spark",
        ] {
            let scoped = run_spark_with_args(fixture, args);
            assert_eq!(scoped.exit_code, 0, "{fixture} {args:?}: {}", scoped.stderr);
            assert_eq!(scoped.stdout.trim(), "5", "{fixture} {args:?}");
        }
        let escapes = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_forwarded_inner_frame_read.spark",
            args,
        );
        assert_eq!(escapes.exit_code, 1, "{args:?}: {}", escapes.stderr);
        assert!(
            escapes
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            escapes.stderr
        );
    }
}

#[test]
fn collection_borrow_future_retires_helper_frame_after_the_call() {
    for args in [&[][..], &["--no-opt"][..]] {
        let scoped = run_spark_with_args(
            "run-pass/async/future_task_helper_frame_read_scoped.spark",
            args,
        );
        assert_eq!(scoped.exit_code, 0, "{args:?}: {}", scoped.stderr);
        assert_eq!(scoped.stdout.trim(), "5", "{args:?}");
        let escapes = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_helper_frame_read_after_return.spark",
            args,
        );
        assert_eq!(escapes.exit_code, 1, "{args:?}: {}", escapes.stderr);
        assert!(
            escapes
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            escapes.stderr
        );
    }
}

// TAG [SPARK-114]
#[test]
fn collection_borrow_future_nonreturning_parents_keep_child_memory_writers() {
    for args in [&[][..], &["--no-opt"][..]] {
        let control = run_spark_with_args(
            "run-pass/async/future_task_retained_child_memory_scoped.spark",
            args,
        );
        assert_eq!(control.exit_code, 0, "{args:?}: {}", control.stderr);
        assert_eq!(control.stdout.trim(), "5", "{args:?}");
        let scoped = build_spark_with_args(
            "build-pass/future_task_nonreturning_child_memory_scoped.spark",
            args,
        );
        assert_eq!(scoped.exit_code, 0, "{args:?}: {}", scoped.stderr);
        let invalid = build_spark_with_args(
            "compile-fail/borrow_errors/future_task_nonreturning_child_memory_owner_drop.spark",
            args,
        );
        assert_eq!(invalid.exit_code, 1, "{args:?}: {}", invalid.stderr);
        assert!(
            invalid
                .stderr
                .contains("borrowed value does not live long enough"),
            "{args:?}: {}",
            invalid.stderr
        );
    }
}

// TAG [SPARK-151]
#[test]
fn struct_copies_retain_and_release_owned_fields() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         int64_t probe_cell_rc(const void *const *cell) { return *(const size_t *)*cell; }\n",
        include_str!("run-pass/basics/struct_owned_fields.spark"),
        "1\n1\n7\n0\n1\n2\n1\n7\n0\n1\n1\n1\n1",
    );
}

#[test]
fn generic_struct_fields_keep_concrete_layout_and_ownership() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         struct Box { uint8_t flag; const void *map; };\n\
         int64_t probe_cell_rc(const void *const *cell) { return *(const size_t *)*cell; }\n\
         int64_t probe_box_rc(const struct Box *box) { return *(const size_t *)box->map; }\n",
        include_str!("run-pass/generics/struct_owned_fields.spark"),
        "1\n1\n2\n3\n2\n1",
    );
}

#[test]
fn struct_owned_fields_follow_collection_and_closure_lifetimes() {
    assert_native_ownership_fixture(
        "#include <stddef.h>\n#include <stdint.h>\n\
         int64_t probe_cell_rc(const void *const *cell) { return *(const size_t *)*cell; }\n",
        include_str!("run-pass/basics/struct_owned_fields_integration.spark"),
        "1\n1\n1\n1\n2\n1\n2\n1\n1\n2\n1\n2\n1\n1",
    );
}

#[test]
fn a_leaf_task_with_a_plain_function_reference_joins_before_frame_cleanup() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result =
            run_spark_with_args("run-pass/async/future_leaf_struct_frame_scoped.spark", args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout.trim(), "1");
    }
}

// TAG [SPARK-149]
#[test]
fn future_spawn_releases_its_captured_future_owner() {
    let native = "#include <stddef.h>\n#include <stdint.h>\n\
        int64_t future_rc(const void *const *slot) { return *(const size_t *)*slot; }\n";
    for fixture in [
        include_str!("run-pass/async/future_ordinary_owned_capture.spark"),
        include_str!("run-pass/async/future_spawn_owned_capture.spark"),
    ] {
        assert_native_ownership_fixture(native, fixture, "1\n7\n1");
    }
}

#[test]
fn future_spawn_preserves_float_callback_results() {
    for args in [&[][..], &["--no-opt"][..]] {
        let result = run_spark_with_args("run-pass/async/future_spawn_float_result.spark", args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.stderr);
        assert_eq!(result.stdout.trim(), "1.5");
    }
}
