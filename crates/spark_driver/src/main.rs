//! Spark compiler driver.
//!
//! This is the main entry point for the `spark` command. It compiles `.spark`
//! source files to native executables using the full compilation pipeline.
//!
//! ```text
//! spark build <input.spark> [-o output]   # compile to executable
//! spark run <input.spark> [-- args...]    # compile + run
//! ```

#![forbid(unsafe_code)]
#![allow(clippy::cast_possible_truncation)]

mod pipeline;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

use clap::{Parser, Subcommand};

/// The Spark compiler.
#[derive(Parser)]
#[command(name = "spark", version, about = "Spark compiler")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Compile a Spark source file to a native executable.
    Build {
        /// Path to the input `.spark` source file.
        input: PathBuf,
        /// Output path for the executable (defaults to input stem).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Compile and run a Spark program.
    Run {
        /// Path to the input `.spark` source file.
        input: PathBuf,
        /// Arguments to forward to the program.
        #[arg(last = true)]
        args: Vec<String>,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Cmd::Build { input, output } => {
            let output_path = output.unwrap_or_else(|| default_output_path(&input));
            match cmd_build(&input, &output_path) {
                Ok(()) => {}
                Err(()) => process::exit(1),
            }
        }
        Cmd::Run { input, args } => {
            let code = cmd_run(&input, &args);
            process::exit(code);
        }
    }
}

/// Compiles `input` to an executable at `output_path`.
fn cmd_build(input: &Path, output_path: &Path) -> Result<(), ()> {
    let source = read_source(input)?;
    let file_name = input
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<unknown>")
        .to_string();

    let result = pipeline::compile(pipeline::CompileInput { source, file_name })?;

    link_object(&result.object_bytes, output_path)
}

/// Compiles and runs `input`, returning the program's exit code.
fn cmd_run(input: &Path, args: &[String]) -> i32 {
    // Compile to a temp directory
    let tmp_dir = std::env::temp_dir().join("spark_run");
    if fs::create_dir_all(&tmp_dir).is_err() {
        eprintln!("error: could not create temp directory");
        return 1;
    }

    let exe_name = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("a.out");
    let exe_path = tmp_dir.join(exe_name);

    if cmd_build(input, &exe_path).is_err() {
        return 1;
    }

    // Run the compiled program
    let status = Command::new(&exe_path).args(args).status();

    // Clean up
    let _ = fs::remove_file(&exe_path);

    match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("error: could not run program: {e}");
            1
        }
    }
}

/// Reads a source file, emitting a diagnostic on failure.
fn read_source(path: &Path) -> Result<String, ()> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) => {
            eprintln!("error: could not read `{}`: {e}", path.display());
            Err(())
        }
    }
}

/// Locates `libspark_runtime.a` for linking.
///
/// Search order:
/// 1. `SPARK_RUNTIME_DIR` environment variable
/// 2. Next to the `spark` binary (works for `cargo run` where both live in target/debug/)
/// 3. `target/debug/` relative to the current directory (works for installed binaries
///    run from the project root)
fn find_runtime_lib() -> Result<PathBuf, ()> {
    let lib_name = "libspark_runtime.a";

    // Check env override first.
    if let Ok(dir) = std::env::var("SPARK_RUNTIME_DIR") {
        let path = PathBuf::from(dir).join(lib_name);
        if path.exists() {
            return Ok(path);
        }
    }

    // Check next to the spark binary (target/debug/ or target/release/).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let path = dir.join(lib_name);
            if path.exists() {
                return Ok(path);
            }
        }
    }

    // Check target/debug/ and target/release/ relative to cwd (for installed binaries).
    for profile in &["debug", "release"] {
        let path = Path::new("target").join(profile).join(lib_name);
        if path.exists() {
            return Ok(path);
        }
    }

    eprintln!("error: could not find libspark_runtime.a");
    eprintln!("hint: run `cargo build -p spark_runtime` first, or set SPARK_RUNTIME_DIR");
    Err(())
}

/// Links an object file into an executable by invoking `cc`.
fn link_object(object_bytes: &[u8], output_path: &Path) -> Result<(), ()> {
    let runtime_lib = find_runtime_lib()?;

    // Write object to a temp file
    let tmp_obj = std::env::temp_dir().join("spark_output.o");
    if fs::write(&tmp_obj, object_bytes).is_err() {
        eprintln!("error: could not write object file");
        return Err(());
    }

    let status = Command::new("cc")
        .arg(&tmp_obj)
        .arg(&runtime_lib)
        .arg("-o")
        .arg(output_path)
        .status();

    // Clean up temp .o
    let _ = fs::remove_file(&tmp_obj);

    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            eprintln!(
                "error: linker failed{}",
                s.code()
                    .map_or_else(String::new, |c| format!(" (exit code {c})"))
            );
            Err(())
        }
        Err(e) => {
            eprintln!("error: could not invoke linker `cc`: {e}");
            eprintln!("hint: make sure a C compiler is installed (Xcode CLI tools on macOS)");
            Err(())
        }
    }
}

/// Derives the default output path from the input: `hello.spark` → `hello`.
fn default_output_path(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("a.out");
    input.parent().unwrap_or_else(|| Path::new(".")).join(stem)
}
