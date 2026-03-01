//! Spark compiler driver.
//!
//! This is the main entry point for the `spark` command. It compiles `.spark`
//! source files to native executables using the full compilation pipeline.
//!
//! ```text
//! spark build <input.spark> [-o output]          # compile project to executable
//! spark run <input.spark> [-- args...]           # compile project + run
//! spark run --single-file <input.spark>          # compile only the input file
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
        /// Disable optimization passes.
        #[arg(long)]
        no_opt: bool,
        /// Compile only the input file, ignoring other `.spark` files in the project.
        #[arg(long)]
        single_file: bool,
    },
    /// Compile and run a Spark program.
    Run {
        /// Path to the input `.spark` source file.
        input: PathBuf,
        /// Disable optimization passes.
        #[arg(long)]
        no_opt: bool,
        /// Compile only the input file, ignoring other `.spark` files in the project.
        #[arg(long)]
        single_file: bool,
        /// Arguments to forward to the program.
        #[arg(last = true)]
        args: Vec<String>,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Cmd::Build {
            input,
            output,
            no_opt,
            single_file,
        } => {
            let output_path = output.unwrap_or_else(|| default_output_path(&input));
            let opt_level = if no_opt {
                spark_opt::OptLevel::None
            } else {
                spark_opt::OptLevel::default()
            };
            match cmd_build(&input, &output_path, opt_level, single_file) {
                Ok(()) => {}
                Err(()) => process::exit(1),
            }
        }
        Cmd::Run {
            input,
            no_opt,
            single_file,
            args,
        } => {
            let opt_level = if no_opt {
                spark_opt::OptLevel::None
            } else {
                spark_opt::OptLevel::default()
            };
            let code = cmd_run(&input, &args, opt_level, single_file);
            process::exit(code);
        }
    }
}

/// Compiles `input` to an executable at `output_path`.
///
/// The project root is derived from the input file's parent directory. The
/// input file itself is always included, plus all `.spark` files found
/// recursively in subdirectories of the root (module directories).
/// Sibling `.spark` files at the root level are **not** included - only the
/// specified input file represents the root-level source.
fn cmd_build(
    input: &Path,
    output_path: &Path,
    opt_level: spark_opt::OptLevel,
    single_file: bool,
) -> Result<(), ()> {
    let abs_input = input.canonicalize().map_err(|e| {
        eprintln!("error: could not resolve path `{}`: {e}", input.display());
    })?;
    let abs_root = abs_input
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let files = if single_file {
        vec![abs_input.clone()]
    } else {
        discover_project_files(&abs_input, &abs_root)?
    };

    let sources = files
        .iter()
        .map(|path| {
            let source = read_source(path)?;
            let relative = path.strip_prefix(&abs_root).unwrap_or(path).to_path_buf();
            Ok((relative, source))
        })
        .collect::<Result<Vec<_>, ()>>()?;

    let result = pipeline::compile(pipeline::CompileInput {
        sources,
        root_path: abs_root,
        opt_level,
    })?;

    link_object(&result.object_bytes, output_path)
}

/// Compiles and runs `input`, returning the program's exit code.
fn cmd_run(
    input: &Path,
    args: &[String],
    opt_level: spark_opt::OptLevel,
    single_file: bool,
) -> i32 {
    // Compile to a per-process temp directory
    let tmp_dir = std::env::temp_dir().join(format!("spark_run_{}", std::process::id()));
    if fs::create_dir_all(&tmp_dir).is_err() {
        eprintln!("error: could not create temp directory");
        return 1;
    }

    let exe_name = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("a.out");
    let exe_path = tmp_dir.join(exe_name);

    if cmd_build(input, &exe_path, opt_level, single_file).is_err() {
        return 1;
    }

    // Run the compiled program
    let status = Command::new(&exe_path).args(args).status();

    // Clean up the per-process temp directory
    let _ = fs::remove_dir_all(&tmp_dir);

    match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("error: could not run program: {e}");
            1
        }
    }
}

/// Discovers project source files for compilation.
///
/// Includes the input file itself plus all `.spark` files found recursively
/// in subdirectories of the root. Sibling `.spark` files at the root level
/// are excluded so that unrelated files in the same directory don't
/// interfere with compilation.
fn discover_project_files(input: &Path, root: &Path) -> Result<Vec<PathBuf>, ()> {
    let mut files = vec![input.to_path_buf()];

    // Walk subdirectories of root for module files.
    let entries = fs::read_dir(root).map_err(|e| {
        eprintln!("error: could not read directory `{}`: {e}", root.display());
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| {
            eprintln!("error: could not read directory entry: {e}");
        })?;
        let path = entry.path();

        if path.is_dir() {
            collect_spark_files(&path, &mut files)?;
        }
    }

    files.sort();
    Ok(files)
}

/// Recursively collects all `.spark` files under `dir`.
fn collect_spark_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), ()> {
    let entries = fs::read_dir(dir).map_err(|e| {
        eprintln!("error: could not read directory `{}`: {e}", dir.display());
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| {
            eprintln!("error: could not read directory entry: {e}");
        })?;
        let path = entry.path();

        if path.is_dir() {
            collect_spark_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("spark") {
            out.push(path);
        }
    }

    Ok(())
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

    // Write object to a unique temp file (avoid races when multiple spark
    // processes link concurrently, e.g. during parallel test execution).
    let tmp_obj = std::env::temp_dir().join(format!(
        "spark_output_{}.o",
        std::process::id()
    ));
    if fs::write(&tmp_obj, object_bytes).is_err() {
        eprintln!("error: could not write object file");
        return Err(());
    }

    let mut cmd = Command::new("cc");
    cmd.arg(&tmp_obj)
        .arg(&runtime_lib)
        .arg("-o")
        .arg(output_path)
        .arg("-lm") // this is for math
        .arg("-lpthread") // This is for async/threading
        .arg("-ldl");

    // Pin the macOS deployment target to 11.0 (Big Sur) to match the minos/sdk
    // values embedded in the object file by spark_codegen. Without this the
    // linker warns about version mismatches between the .o and runtime library.
    if cfg!(target_os = "macos") {
        cmd.arg("-mmacosx-version-min=11.0");
    }

    let status = cmd.status();

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

/// Derives the default output path from the input: `hello.spark` -> `hello`.
fn default_output_path(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("a.out");
    input.parent().unwrap_or_else(|| Path::new(".")).join(stem)
}
