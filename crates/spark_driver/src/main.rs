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
    /// Initialize a new Spark project in the current directory.
    Init {
        /// Project name (defaults to directory name).
        #[arg(short, long)]
        name: Option<String>,
    },
    /// Fetch and update project dependencies.
    Deps,
    /// Publish the current package to the registry.
    Publish {
        /// GitHub token for uploading releases (or uses `gh` CLI auth).
        #[arg(long)]
        token: Option<String>,
    },
    /// Search the package registry.
    Search {
        /// Search query (matches package name and description).
        query: String,
    },
    /// Add a dependency to the current project.
    Add {
        /// Package name.
        package: String,
        /// Version constraint (defaults to latest available version).
        #[arg(short, long)]
        version: Option<String>,
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
        Cmd::Init { name } => match cmd_init(name.as_deref()) {
            Ok(()) => {}
            Err(()) => process::exit(1),
        },
        Cmd::Deps => match cmd_deps() {
            Ok(()) => {}
            Err(()) => process::exit(1),
        },
        Cmd::Publish { token } => match cmd_publish(token.as_deref()) {
            Ok(()) => {}
            Err(()) => process::exit(1),
        },
        Cmd::Search { query } => match cmd_search(&query) {
            Ok(()) => {}
            Err(()) => process::exit(1),
        },
        Cmd::Add { package, version } => match cmd_add(&package, version.as_deref()) {
            Ok(()) => {}
            Err(()) => process::exit(1),
        },
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

    let mut sources: Vec<(PathBuf, String)> = files
        .iter()
        .map(|path| {
            let source = read_source(path)?;
            let relative = path.strip_prefix(&abs_root).unwrap_or(path).to_path_buf();
            Ok((relative, source))
        })
        .collect::<Result<Vec<_>, ()>>()?;

    // Load Spark.toml manifest if present. Resolve all dependencies
    // (path + git, including transitive) and collect link info.
    let mut manifest_links: Vec<String> = Vec::new();
    let mut manifest_link_paths: Vec<String> = Vec::new();

    if let Some(manifest_result) = spark_package::load_manifest(&abs_root) {
        match manifest_result {
            Ok(manifest) => {
                // Resolve all dependencies (path + git + transitive).
                match spark_package::resolve_all_deps(&manifest, &abs_root) {
                    Ok(resolved) => {
                        sources.extend(resolved.sources);

                        // Collect transitive link info from dependencies.
                        manifest_links.extend(resolved.link_libraries);
                        manifest_link_paths.extend(resolved.link_paths);

                        // Write Spark.lock if there are git deps.
                        if let Err(e) = spark_package::write_lock_file(&abs_root, &resolved.lock) {
                            eprintln!("warning: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        return Err(());
                    }
                }

                // Collect build section link info from root manifest.
                for lib in manifest.build.links {
                    if !manifest_links.contains(&lib) {
                        manifest_links.push(lib);
                    }
                }
                for path in manifest.build.link_paths {
                    if !manifest_link_paths.contains(&path) {
                        manifest_link_paths.push(path);
                    }
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                return Err(());
            }
        }
    }

    let result = pipeline::compile(pipeline::CompileInput {
        sources,
        root_path: abs_root,
        opt_level,
    })?;

    // Merge link libraries from AST `link` directives and Spark.toml `[build]`.
    let mut all_link_libs = result.link_libraries;
    for lib in manifest_links {
        if !all_link_libs.contains(&lib) {
            all_link_libs.push(lib);
        }
    }

    // Merge link search paths from `link ... from "path"` directives and Spark.toml.
    let mut all_link_paths = result.link_search_paths;
    for path in manifest_link_paths {
        if !all_link_paths.contains(&path) {
            all_link_paths.push(path);
        }
    }

    link_object(
        &result.object_bytes,
        output_path,
        &all_link_libs,
        &all_link_paths,
    )
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

/// Initializes a new Spark project in the current directory.
///
/// Creates a `Spark.toml` with the given name (or the directory name if not
/// specified). Errors if a `Spark.toml` already exists.
fn cmd_init(name: Option<&str>) -> Result<(), ()> {
    let cwd = std::env::current_dir().map_err(|e| {
        eprintln!("error: could not determine current directory: {e}");
    })?;

    let manifest_path = cwd.join("Spark.toml");
    if manifest_path.exists() {
        eprintln!("error: Spark.toml already exists in this directory");
        return Err(());
    }

    let project_name = name.map(String::from).unwrap_or_else(|| {
        cwd.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("spark-project")
            .to_string()
    });

    let contents = format!(
        "[project]\nname = \"{project_name}\"\nversion = \"0.1.0\"\nentry = \"main.spark\"\n\n[dependencies]\n\n[build]\nlinks = []\nlink-paths = []\n"
    );

    fs::write(&manifest_path, contents).map_err(|e| {
        eprintln!("error: could not write Spark.toml: {e}");
    })?;

    println!("Created Spark.toml for project `{project_name}`");
    Ok(())
}

/// Fetches and updates project dependencies.
///
/// Loads `Spark.toml`, resolves all dependencies (path + git + transitive),
/// writes `Spark.lock`, and prints a summary.
fn cmd_deps() -> Result<(), ()> {
    let cwd = std::env::current_dir().map_err(|e| {
        eprintln!("error: could not determine current directory: {e}");
    })?;

    let manifest = match spark_package::load_manifest(&cwd) {
        Some(Ok(m)) => m,
        Some(Err(e)) => {
            eprintln!("error: {e}");
            return Err(());
        }
        None => {
            eprintln!("error: no Spark.toml found in current directory");
            return Err(());
        }
    };

    if manifest.dependencies.is_empty() {
        println!("No dependencies to resolve.");
        return Ok(());
    }

    let resolved = spark_package::resolve_all_deps(&manifest, &cwd).map_err(|e| {
        eprintln!("error: {e}");
    })?;

    // Write lock file.
    if let Err(e) = spark_package::write_lock_file(&cwd, &resolved.lock) {
        eprintln!("warning: {e}");
    }

    // Print summary.
    let dep_count = manifest.dependencies.len();
    let source_count = resolved.sources.len();
    println!(
        "Resolved {dep_count} dependenc{} ({source_count} source file{}).",
        if dep_count == 1 { "y" } else { "ies" },
        if source_count == 1 { "" } else { "s" },
    );

    if !resolved.lock.packages.is_empty() {
        println!("Wrote Spark.lock with {} git package{}.",
            resolved.lock.packages.len(),
            if resolved.lock.packages.len() == 1 { "" } else { "s" },
        );
    }

    Ok(())
}

/// Publishes the current package to the registry.
///
/// 1. Loads `Spark.toml`, validates required fields
/// 2. Packages all `.spark` files + `Spark.toml` into a tarball
/// 3. Uploads to GitHub Releases via `gh`
/// 4. Clones the registry index, adds the new version, commits and pushes
fn cmd_publish(token: Option<&str>) -> Result<(), ()> {
    let cwd = std::env::current_dir().map_err(|e| {
        eprintln!("error: could not determine current directory: {e}");
    })?;

    let manifest = match spark_package::load_manifest(&cwd) {
        Some(Ok(m)) => m,
        Some(Err(e)) => {
            eprintln!("error: {e}");
            return Err(());
        }
        None => {
            eprintln!("error: no Spark.toml found in current directory");
            return Err(());
        }
    };

    let Some(repo_url) = &manifest.project.repository else {
        eprintln!("error: `repository` field is required in [project] for publishing");
        eprintln!("hint: add `repository = \"https://github.com/owner/repo\"` to Spark.toml");
        return Err(());
    };

    let name = &manifest.project.name;
    let version = &manifest.project.version;

    println!("Packaging {name} v{version}...");

    let (tarball, checksum) =
        spark_package::publish::package_project(&cwd).map_err(|e| {
            eprintln!("error: {e}");
        })?;

    println!("Uploading to GitHub Releases...");

    let download_url =
        spark_package::publish::upload_release(&tarball, repo_url, name, version, token)
            .map_err(|e| {
                eprintln!("error: {e}");
            })?;

    // Clean up local tarball.
    let _ = fs::remove_file(&tarball);

    println!("Updating registry index...");

    let index_path = spark_package::registry::sync_index(&cwd).map_err(|e| {
        eprintln!("error: {e}");
    })?;

    let entry = spark_package::registry::VersionEntry {
        version: version.parse().map_err(|e| {
            eprintln!("error: invalid version `{version}`: {e}");
        })?,
        download: download_url.clone(),
        checksum,
        deps: manifest
            .dependencies
            .iter()
            .filter_map(|(k, v)| {
                if let spark_package::Dependency::Registry(constraint) = v {
                    Some((k.clone(), constraint.clone()))
                } else {
                    None
                }
            })
            .collect(),
    };

    spark_package::publish::update_index(
        &index_path,
        name,
        "", // description from manifest could be added later
        &entry,
    )
    .map_err(|e| {
        eprintln!("error: {e}");
    })?;

    // Push index changes.
    let output = std::process::Command::new("git")
        .args(["push", "--quiet"])
        .current_dir(&index_path)
        .output()
        .map_err(|e| {
            eprintln!("error: could not push index changes: {e}");
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("warning: could not push index changes: {stderr}");
        eprintln!("hint: push manually from {}", index_path.display());
    }

    println!("Published {name} v{version}");
    println!("Download: {download_url}");

    Ok(())
}

/// Searches the package registry for packages matching a query.
fn cmd_search(query: &str) -> Result<(), ()> {
    let cwd = std::env::current_dir().map_err(|e| {
        eprintln!("error: could not determine current directory: {e}");
    })?;

    let index_path = spark_package::registry::sync_index(&cwd).map_err(|e| {
        eprintln!("error: {e}");
    })?;

    let results = spark_package::registry::search_index(&index_path, query).map_err(|e| {
        eprintln!("error: {e}");
    })?;

    if results.is_empty() {
        println!("No packages found matching `{query}`.");
        return Ok(());
    }

    println!(
        "Found {} package{} matching `{query}`:\n",
        results.len(),
        if results.len() == 1 { "" } else { "s" }
    );

    for pkg in &results {
        let latest = pkg.versions.last().map_or_else(
            || "—".to_string(),
            |v| v.version.to_string(),
        );
        println!("  {} v{}  — {}", pkg.name, latest, pkg.description);
    }

    Ok(())
}

/// Adds a dependency to the current project's `Spark.toml`.
fn cmd_add(package: &str, version: Option<&str>) -> Result<(), ()> {
    let cwd = std::env::current_dir().map_err(|e| {
        eprintln!("error: could not determine current directory: {e}");
    })?;

    let manifest_path = cwd.join("Spark.toml");
    if !manifest_path.exists() {
        eprintln!("error: no Spark.toml found in current directory");
        eprintln!("hint: run `spark init` to create a project");
        return Err(());
    }

    // Sync index and look up the package.
    let index_path = spark_package::registry::sync_index(&cwd).map_err(|e| {
        eprintln!("error: {e}");
    })?;

    let metadata =
        spark_package::registry::lookup_package(&index_path, package).map_err(|e| {
            eprintln!("error: {e}");
        })?;

    let version_str = if let Some(v) = version {
        v.to_string()
    } else {
        // Use the latest version as a bare version constraint.
        let latest = metadata
            .versions
            .last()
            .ok_or_else(|| {
                eprintln!("error: package `{package}` has no published versions");
            })?;
        latest.version.to_string()
    };

    // Validate the version constraint resolves.
    spark_package::registry::resolve_version(&metadata, &version_str).map_err(|e| {
        eprintln!("error: {e}");
    })?;

    // Add to Spark.toml.
    spark_package::manifest::add_dependency(&manifest_path, package, &version_str).map_err(|e| {
        eprintln!("error: {e}");
    })?;

    println!("Added `{package} = \"{version_str}\"` to Spark.toml");

    Ok(())
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
fn link_object(
    object_bytes: &[u8],
    output_path: &Path,
    extra_libs: &[String],
    extra_link_paths: &[String],
) -> Result<(), ()> {
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

    // Add extra link search paths from Spark.toml [build] section
    for path in extra_link_paths {
        cmd.arg(format!("-L{path}"));
    }

    // Add extra link libraries from `link` directives and Spark.toml
    for lib in extra_libs {
        cmd.arg(format!("-l{lib}"));
    }

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
