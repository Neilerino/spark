//! Workspace-level state and multi-file analysis pipeline.
//!
//! When the LSP server has a workspace root, all `.spark` files are
//! discovered, parsed, and analyzed together through the compiler
//! pipeline (lex → parse → modules → resolve → type-check → HIR →
//! MIR → borrow-check). This enables cross-file features such as
//! go-to-definition across files and workspace-wide find-references.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ropey::Rope;
use tower_lsp::lsp_types::{self, Url};

use spark_borrow::BorrowInput;
use spark_diagnostics::Diagnostics;
use spark_hir::HirInput;
use spark_lexer::Lexer;
use spark_mir::MirInput;
use spark_modules::ModuleResolverInput;
use spark_parser::parse;
use spark_resolve::{DefinitionTable, ResolvedAst, ResolverInput, ScopeArena};
use spark_span::{FileId, SourceMap};
use spark_types::{TypeCheckerInput, TypeTable};

use crate::document;

/// Scans file URIs for files named `main.spark` and returns their parent
/// directories as project roots.
///
/// When the workspace root encompasses multiple projects (e.g., a monorepo),
/// each sub-project with a `main.spark` is treated as its own root for
/// module path computation. This ensures `import mymod { hello }` resolves
/// correctly regardless of the workspace root directory.
fn find_project_roots(files: &[(Url, String)]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for (uri, _) in files {
        if let Ok(path) = uri.to_file_path() {
            if path.file_name().is_some_and(|n| n == "main.spark") {
                if let Some(parent) = path.parent() {
                    roots.push(parent.to_path_buf());
                }
            }
        }
    }
    // Sort by path length descending so the most specific root is checked first.
    roots.sort_by_key(|b| std::cmp::Reverse(b.as_os_str().len()));
    roots
}

/// Finds the most specific project root that is an ancestor of `file_path`.
///
/// Falls back to `ws_root` if no project root matches.
fn effective_root_for_file<'a>(
    file_path: &Path,
    project_roots: &'a [PathBuf],
    ws_root: &'a Path,
) -> &'a Path {
    for root in project_roots {
        if file_path.starts_with(root) {
            return root;
        }
    }
    ws_root
}

/// Detects if a file is inside a `spark_stdlib` directory and returns its
/// path relative to the stdlib root (e.g., `spark/math.spark`).
///
/// Returns `None` if the file is not a stdlib file.
fn stdlib_relative_path(file_path: &Path) -> Option<PathBuf> {
    let components: Vec<_> = file_path.components().collect();
    for (i, component) in components.iter().enumerate() {
        if let std::path::Component::Normal(s) = component {
            if **s == *"spark_stdlib" {
                // Take everything after "spark_stdlib/"
                let remainder: PathBuf = components[i + 1..].iter().collect();
                if !remainder.as_os_str().is_empty() {
                    return Some(remainder);
                }
            }
        }
    }
    None
}

/// A source file from a resolved `Spark.toml` dependency.
pub struct DependencySource {
    /// `file://` URI for the source file.
    pub uri: Url,
    /// Source text.
    pub source: String,
    /// Dep-prefixed relative path for module resolution (e.g., `lib_b/lib_b.spark`).
    pub module_path: PathBuf,
}

/// Resolves `Spark.toml` dependencies from a single directory.
///
/// Returns resolved sources and any errors encountered during resolution.
/// If no `Spark.toml` exists, returns empty results (not an error).
pub fn resolve_dependencies(root: &Path) -> (Vec<DependencySource>, Vec<String>) {
    let mut errors = Vec::new();

    let manifest = match spark_package::load_manifest(root) {
        Some(Ok(m)) => m,
        Some(Err(e)) => {
            errors.push(format!("failed to parse Spark.toml: {e}"));
            return (Vec::new(), errors);
        }
        None => return (Vec::new(), errors),
    };

    let resolved = match spark_package::resolve_all_deps(&manifest, root) {
        Ok(r) => r,
        Err(e) => {
            errors.push(format!("dependency resolution failed: {e}"));
            return (Vec::new(), errors);
        }
    };

    let mut dep_sources = Vec::new();
    for src in &resolved.sources {
        let Some(uri) = file_path_to_uri(&src.absolute_path) else {
            errors.push(format!(
                "could not convert path to URI: {}",
                src.absolute_path.display()
            ));
            continue;
        };
        dep_sources.push(DependencySource {
            uri,
            source: src.source.clone(),
            module_path: src.module_path.clone(),
        });
    }

    (dep_sources, errors)
}

/// Resolves `Spark.toml` dependencies for all projects in the workspace.
///
/// Scans the workspace for `Spark.toml` files (at the root and in
/// subdirectories) and resolves dependencies for each one. This supports
/// monorepo layouts where nested projects have their own manifests.
pub fn resolve_all_workspace_deps(
    root: &Path,
) -> (Vec<DependencySource>, Vec<String>) {
    let mut all_deps = Vec::new();
    let mut all_errors = Vec::new();
    let mut seen_uris = std::collections::HashSet::new();

    // Collect all directories containing Spark.toml.
    let mut manifest_dirs = Vec::new();
    discover_manifests(root, &mut manifest_dirs);

    for dir in &manifest_dirs {
        let (deps, errors) = resolve_dependencies(dir);
        all_errors.extend(errors);
        for dep in deps {
            // Deduplicate across projects (e.g., two projects depend on the same lib).
            if seen_uris.insert(dep.uri.clone()) {
                all_deps.push(dep);
            }
        }
    }

    (all_deps, all_errors)
}

/// Recursively finds directories containing `Spark.toml`.
fn discover_manifests(dir: &Path, out: &mut Vec<PathBuf>) {
    if dir.join("Spark.toml").exists() {
        out.push(dir.to_path_buf());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Skip hidden dirs, build output, and package caches.
            if !name_str.starts_with('.')
                && name_str != "target"
                && name_str != "node_modules"
            {
                discover_manifests(&path, out);
            }
        }
    }
}

/// Workspace-level state shared across all documents.
///
/// Tracks the project root, file ID mappings, and source text for
/// files that are not currently open in the editor.
pub struct WorkspaceState {
    /// Project root directory (absolute).
    pub root_path: PathBuf,
    /// `FileId` → URI mapping from the last analysis run.
    pub file_id_to_uri: HashMap<FileId, Url>,
    /// URI → `FileId` mapping from the last analysis run.
    pub uri_to_file_id: HashMap<Url, FileId>,
    /// Source text for files NOT open in the editor.
    pub disk_sources: HashMap<Url, String>,
    /// Ropes for files NOT open in the editor (for cross-file span→range conversion).
    pub disk_ropes: HashMap<Url, Rope>,
    /// Sources from resolved `Spark.toml` dependencies.
    pub dep_sources: Vec<DependencySource>,
}

impl WorkspaceState {
    /// Creates a new workspace state with the given root path.
    #[must_use]
    pub fn new(root_path: PathBuf) -> Self {
        Self {
            root_path,
            file_id_to_uri: HashMap::new(),
            uri_to_file_id: HashMap::new(),
            disk_sources: HashMap::new(),
            disk_ropes: HashMap::new(),
            dep_sources: Vec::new(),
        }
    }
}

/// Output from analyzing the entire workspace.
pub struct WorkspaceAnalysisOutput {
    /// `FileId` → URI mapping.
    pub file_id_to_uri: HashMap<FileId, Url>,
    /// URI → `FileId` mapping.
    pub uri_to_file_id: HashMap<Url, FileId>,
    /// Resolved AST with span → `DefId` mappings for all files.
    pub resolved: ResolvedAst,
    /// Table of all definitions across all files.
    pub definitions: DefinitionTable,
    /// Scope arena for autocomplete queries.
    pub scope_arena: Option<ScopeArena>,
    /// Type table from the type checker (only available when type checking succeeds).
    pub type_table: Option<TypeTable>,
    /// Per-file LSP diagnostics, ready to publish.
    pub diagnostics_by_uri: HashMap<Url, Vec<lsp_types::Diagnostic>>,
    /// Ropes for all analyzed files (for span→range conversion).
    pub ropes: HashMap<FileId, Rope>,
}

/// Discovers all `.spark` files under the given root directory.
///
/// Recursively walks the directory tree, skipping hidden directories
/// (those starting with `.`) and common build output directories.
pub fn discover_spark_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    discover_recursive(root, &mut files);
    files.sort();
    files
}

/// Recursive helper for file discovery.
fn discover_recursive(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Skip hidden directories and common build output
            if !name_str.starts_with('.') && name_str != "target" && name_str != "node_modules" {
                discover_recursive(&path, files);
            }
        } else if path.extension().is_some_and(|ext| ext == "spark") {
            files.push(path);
        }
    }
}

/// Converts an absolute file path to a `file://` URI.
///
/// Returns `None` if the path cannot be converted.
#[must_use]
pub fn file_path_to_uri(path: &Path) -> Option<Url> {
    Url::from_file_path(path).ok()
}

/// Runs the compiler pipeline on all workspace files.
///
/// Mirrors `spark_driver::pipeline` phases 1–8:
/// 1. Lex + parse all files
/// 2. Module resolution (builds module graph from file paths)
/// 3. Name resolution (resolves identifiers across all files)
/// 4. Type checking
/// 5. HIR lowering (only if no errors so far)
/// 6. MIR construction (only if no errors so far)
/// 7. Borrow checking (only if no errors so far)
///
/// Phases 1–4 always run and accumulate diagnostics. Phases 5–7 only
/// run if prior phases produced no errors.
///
/// Dependency files from `dep_files` are included in the analysis
/// alongside workspace files, with deduplication by URI. Their
/// `module_path` is used for module resolution instead of computing
/// a relative path from the workspace root.
pub fn analyze_workspace(
    files: &[(Url, String)],
    dep_files: &[DependencySource],
    root_path: &Path,
) -> WorkspaceAnalysisOutput {
    let mut source_map = SourceMap::new();
    let mut file_id_to_uri = HashMap::new();
    let mut uri_to_file_id = HashMap::new();
    let mut ropes = HashMap::new();
    let mut all_diagnostics = Diagnostics::new();

    // Assign FileIds, build ropes, and lex+parse all files.
    let mut asts = HashMap::new();
    let mut file_paths = HashMap::new();

    // Detect project roots (directories containing main.spark) so that
    // module paths are computed relative to the correct root.
    let project_roots = find_project_roots(files);

    // Track whether on-disk stdlib files were found.
    let mut found_stdlib_on_disk = false;

    for (uri, source) in files {
        let file_path = uri
            .to_file_path()
            .unwrap_or_else(|()| PathBuf::from("untitled.spark"));
        let file_name = file_path.to_string_lossy();

        let file_id = source_map.add_file(&*file_name, source);
        file_id_to_uri.insert(file_id, uri.clone());
        uri_to_file_id.insert(uri.clone(), file_id);
        ropes.insert(file_id, Rope::from_str(source));

        // Compute relative path for module resolution (mirrors spark_driver).
        // - Stdlib files inside a spark_stdlib directory get paths like
        //   "spark/math.spark" so their module paths match the canonical names.
        // - Other files use the nearest project root (directory with main.spark)
        //   so that e.g. "mymod/mymod.spark" → module "mymod".
        #[allow(clippy::option_if_let_else)] // closure can't mutate found_stdlib_on_disk
        let relative_path = if let Some(stdlib_rel) = stdlib_relative_path(&file_path) {
            found_stdlib_on_disk = true;
            stdlib_rel
        } else {
            let effective_root =
                effective_root_for_file(&file_path, &project_roots, root_path);
            file_path
                .strip_prefix(effective_root)
                .unwrap_or(&file_path)
                .to_path_buf()
        };
        file_paths.insert(file_id, relative_path);

        // Phase 1: Lex
        let tokens = Lexer::new(file_id, source).tokenize();

        // Phase 2: Parse
        let (ast, parse_diags) = parse(tokens, file_id);
        all_diagnostics.extend(parse_diags);
        asts.insert(file_id, ast);
    }

    // Add dependency files. If a dep file was already discovered via the
    // workspace directory walk, override its module path with the dep-prefixed
    // path so imports resolve correctly (e.g., `libs/lib_b/lib_b.spark` →
    // `lib_b/lib_b.spark`).
    for dep in dep_files {
        if let Some(&existing_id) = uri_to_file_id.get(&dep.uri) {
            // File already lexed/parsed from the directory walk — just fix
            // its module path so the module resolver sees the dep-prefixed name.
            file_paths.insert(existing_id, dep.module_path.clone());
            continue;
        }

        let file_path = dep.uri
            .to_file_path()
            .unwrap_or_else(|()| PathBuf::from("dep.spark"));
        let file_name = file_path.to_string_lossy();

        let file_id = source_map.add_file(&*file_name, &dep.source);
        file_id_to_uri.insert(file_id, dep.uri.clone());
        uri_to_file_id.insert(dep.uri.clone(), file_id);
        ropes.insert(file_id, Rope::from_str(&dep.source));
        file_paths.insert(file_id, dep.module_path.clone());

        // Phase 1: Lex
        let tokens = Lexer::new(file_id, &dep.source).tokenize();

        // Phase 2: Parse
        let (ast, parse_diags) = parse(tokens, file_id);
        all_diagnostics.extend(parse_diags);
        asts.insert(file_id, ast);
    }

    // Inject stdlib from embedded sources if not found on disk.
    // This ensures `import spark.math { sqrt }` works in any workspace.
    if !found_stdlib_on_disk {
        for stdlib_src in spark_stdlib::stdlib_sources() {
            let file_id = source_map.add_file(stdlib_src.path, stdlib_src.source);
            let uri = Url::parse(&format!("spark-stdlib:///{}", stdlib_src.path))
                .expect("valid synthetic URI");
            file_id_to_uri.insert(file_id, uri.clone());
            uri_to_file_id.insert(uri, file_id);
            ropes.insert(file_id, Rope::from_str(stdlib_src.source));
            file_paths.insert(file_id, PathBuf::from(stdlib_src.path));

            let tokens = Lexer::new(file_id, stdlib_src.source).tokenize();
            let (ast, parse_diags) = parse(tokens, file_id);
            all_diagnostics.extend(parse_diags);
            asts.insert(file_id, ast);
        }
    }

    // Phase 3: Module resolution
    let module_output = spark_modules::resolve(ModuleResolverInput {
        asts: asts.clone(),
        root_path: root_path.to_path_buf(),
        file_paths,
    });
    all_diagnostics.extend(module_output.diagnostics);
    let graph = module_output.graph;

    // Phase 4: Name resolution
    let resolve_output = spark_resolve::resolve(ResolverInput {
        graph: graph.clone(),
        asts,
    });
    all_diagnostics.extend(resolve_output.diagnostics);

    // Phase 5: Type checking
    let type_output = spark_types::check(TypeCheckerInput {
        resolved: &resolve_output.resolved,
        definitions: &resolve_output.definitions,
        graph,
    });
    all_diagnostics.extend(type_output.diagnostics);
    let builtins = type_output.builtins;
    let type_table = Some(type_output.table);

    let resolved = resolve_output.resolved;
    let definitions = resolve_output.definitions;
    let scope_arena = Some(resolve_output.scope_arena);

    // Phases 6–8: HIR → MIR → Borrow checking
    // Run unconditionally when the type table is available. Unlike the
    // compiler pipeline (which exits early on errors), the LSP accumulates
    // diagnostics from ALL files in the workspace. Gating on
    // `has_errors()` would skip borrow checking for the entire workspace
    // if any single file had a type/parse error. The downstream phases
    // handle error types gracefully.
    if let Some(ref table) = type_table {
        let hir_output = spark_hir::lower(HirInput {
            resolved: resolved.clone(),
            definitions: definitions.clone(),
            table: table.clone(),
            builtins: builtins.clone(),
        });
        all_diagnostics.extend(hir_output.diagnostics);

        let mir_output = spark_mir::lower(MirInput {
            hir: hir_output.hir,
            table: hir_output.table,
            definitions: hir_output.definitions,
        });
        all_diagnostics.extend(mir_output.diagnostics);

        let borrow_output = spark_borrow::check(BorrowInput {
            mir: mir_output.mir,
            table: mir_output.table,
            definitions: mir_output.definitions,
        });
        all_diagnostics.extend(borrow_output.diagnostics);
    }

    // Distribute diagnostics by file
    let diagnostics_by_uri = distribute_diagnostics(
        &all_diagnostics,
        &ropes,
        &file_id_to_uri,
    );

    WorkspaceAnalysisOutput {
        file_id_to_uri,
        uri_to_file_id,
        resolved,
        definitions,
        scope_arena,
        type_table,
        diagnostics_by_uri,
        ropes,
    }
}

/// Groups diagnostics by file URI for publishing.
///
/// Each diagnostic is assigned to the file containing its primary label.
/// Diagnostics without a primary label or with unknown file IDs are skipped.
fn distribute_diagnostics(
    diagnostics: &Diagnostics,
    ropes: &HashMap<FileId, Rope>,
    file_id_to_uri: &HashMap<FileId, Url>,
) -> HashMap<Url, Vec<lsp_types::Diagnostic>> {
    let mut by_uri: HashMap<Url, Vec<lsp_types::Diagnostic>> = HashMap::new();

    // Ensure every known file has an entry (even if empty) so we can
    // clear stale diagnostics for files that no longer have errors.
    for uri in file_id_to_uri.values() {
        by_uri.entry(uri.clone()).or_default();
    }

    for diag in diagnostics.iter() {
        let primary_label = diag.labels.iter().find(|l| l.is_primary);
        let Some(label) = primary_label else {
            continue;
        };

        let file_id = label.span.file;
        let Some(uri) = file_id_to_uri.get(&file_id) else {
            continue;
        };
        let Some(rope) = ropes.get(&file_id) else {
            continue;
        };

        let lsp_diag = diagnostic_to_lsp_cross_file(diag, rope, uri, ropes, file_id_to_uri);
        by_uri.entry(uri.clone()).or_default().push(lsp_diag);
    }

    by_uri
}

/// Converts a compiler diagnostic to an LSP diagnostic with cross-file
/// related information.
///
/// Secondary labels in other files get correct URIs and ranges.
fn diagnostic_to_lsp_cross_file(
    diag: &spark_diagnostics::Diagnostic,
    primary_rope: &Rope,
    _primary_uri: &Url,
    ropes: &HashMap<FileId, Rope>,
    file_id_to_uri: &HashMap<FileId, Url>,
) -> lsp_types::Diagnostic {
    let primary_label = diag.labels.iter().find(|l| l.is_primary);
    let range = primary_label
        .and_then(|l| document::span_to_range(primary_rope, l.span))
        .unwrap_or(lsp_types::Range {
            start: lsp_types::Position { line: 0, character: 0 },
            end: lsp_types::Position { line: 0, character: 0 },
        });

    let severity = if diag.is_error() {
        Some(lsp_types::DiagnosticSeverity::ERROR)
    } else if diag.is_warning() {
        Some(lsp_types::DiagnosticSeverity::WARNING)
    } else {
        Some(lsp_types::DiagnosticSeverity::INFORMATION)
    };

    // Convert secondary labels to related information, resolving
    // cross-file spans to their correct URIs.
    let related_information: Vec<lsp_types::DiagnosticRelatedInformation> = diag
        .labels
        .iter()
        .filter(|l| !l.is_primary)
        .filter_map(|l| {
            let label_file_id = l.span.file;
            let label_uri = file_id_to_uri.get(&label_file_id)?;
            let label_rope = ropes.get(&label_file_id)?;
            let label_range = document::span_to_range(label_rope, l.span)?;
            Some(lsp_types::DiagnosticRelatedInformation {
                location: lsp_types::Location {
                    uri: label_uri.clone(),
                    range: label_range,
                },
                message: l.message.clone(),
            })
        })
        .collect();

    lsp_types::Diagnostic {
        range,
        severity,
        code: diag.code.as_ref().map(|c| {
            lsp_types::NumberOrString::String(c.as_str().to_string())
        }),
        source: Some("spark".to_string()),
        message: diag.message.clone(),
        related_information: if related_information.is_empty() {
            None
        } else {
            Some(related_information)
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: creates a temp workspace with the given files and runs analysis.
    fn analyze_workspace_sources(sources: &[(&str, &str)]) -> WorkspaceAnalysisOutput {
        let tmp = TempDir::new().expect("failed to create temp dir");
        let root = tmp.path();

        let files: Vec<(Url, String)> = sources
            .iter()
            .map(|(rel_path, source)| {
                let full_path = root.join(rel_path);
                if let Some(parent) = full_path.parent() {
                    fs::create_dir_all(parent).ok();
                }
                fs::write(&full_path, source).expect("failed to write file");
                let uri = Url::from_file_path(&full_path).expect("failed to create URI");
                (uri, (*source).to_string())
            })
            .collect();

        analyze_workspace(&files, &[], root)
    }

    #[test]
    fn workspace_discovers_spark_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Create some .spark files
        fs::write(root.join("main.spark"), "fn main(): Void { }").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/utils.spark"), "fn helper(): Void { }").unwrap();

        // Create a non-spark file that should be ignored
        fs::write(root.join("readme.txt"), "hello").unwrap();

        // Create a hidden directory that should be skipped
        fs::create_dir_all(root.join(".hidden")).unwrap();
        fs::write(root.join(".hidden/secret.spark"), "fn secret(): Void { }").unwrap();

        let files = discover_spark_files(root);
        assert_eq!(files.len(), 2, "Expected 2 .spark files, got: {files:?}");
        assert!(files.iter().any(|p| p.ends_with("main.spark")));
        assert!(files.iter().any(|p| p.ends_with("utils.spark")));
    }

    /// Counts URIs with the `file` scheme (excludes injected stdlib).
    fn count_file_uris(map: &HashMap<Url, impl std::any::Any>) -> usize {
        map.keys().filter(|u| u.scheme() == "file").count()
    }

    #[test]
    fn single_file_analysis_works() {
        let output = analyze_workspace_sources(&[
            ("main.spark", "fn main(): Void { x: Int = 42\nx }"),
        ]);
        // 1 user file (stdlib files are also present but use synthetic URIs).
        let file_count = output.file_id_to_uri.values()
            .filter(|u| u.scheme() == "file")
            .count();
        assert_eq!(file_count, 1);
        assert!(output.type_table.is_some());
    }

    #[test]
    fn multi_file_analysis_produces_results() {
        let output = analyze_workspace_sources(&[
            ("main.spark", "fn main(): Void { }"),
            ("utils.spark", "fn helper(): Int { return 1 }"),
        ]);
        let file_count = output.file_id_to_uri.values()
            .filter(|u| u.scheme() == "file")
            .count();
        assert_eq!(file_count, 2);
        // Resolved asts include user files + stdlib.
        assert!(output.resolved.asts.len() >= 2);
    }

    #[test]
    fn diagnostics_distributed_per_file() {
        let output = analyze_workspace_sources(&[
            ("good.spark", "fn main(): Void { }"),
            ("bad.spark", "fn broken(): Int { }"),  // missing return value
        ]);

        // Each user file should have its own diagnostics entry.
        let user_diag_count = count_file_uris(&output.diagnostics_by_uri);
        assert_eq!(
            user_diag_count,
            2,
            "Expected diagnostics entry for each user file"
        );
    }

    #[test]
    fn file_path_to_uri_roundtrip() {
        let path = PathBuf::from("/tmp/test.spark");
        let uri = file_path_to_uri(&path).expect("should create URI");
        let back = uri.to_file_path().expect("should convert back");
        assert_eq!(back, path);
    }

    #[test]
    fn cross_file_import_resolves() {
        // Mirrors the multimod test project: main.spark imports from mymod.
        let output = analyze_workspace_sources(&[
            ("main.spark", "import mymod { hello }\nfn main(): Void { hello() }"),
            ("mymod/mymod.spark", "module mymod\npub fn hello(): Void { }"),
        ]);

        // Import should resolve without errors (no "module not found" or
        // "not exported" diagnostics).
        let has_import_error = output.diagnostics_by_uri.values().any(|diags| {
            diags.iter().any(|d| {
                d.message.contains("cannot find module")
                    || d.message.contains("not exported")
            })
        });
        assert!(
            !has_import_error,
            "Cross-file import should resolve without errors. Diagnostics: {:?}",
            output.diagnostics_by_uri.values().flatten().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn discover_skips_target_directory() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        fs::write(root.join("main.spark"), "fn main(): Void { }").unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("target/debug/output.spark"), "fn x(): Void { }").unwrap();

        let files = discover_spark_files(root);
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn stdlib_import_resolves() {
        // Verifies that stdlib modules are injected and resolvable
        // when no on-disk stdlib is present.
        let output = analyze_workspace_sources(&[
            ("main.spark", "import spark.math { sqrt }\nfn main(): Void { sqrt(4.0) }"),
        ]);

        let has_module_error = output.diagnostics_by_uri.values().any(|diags| {
            diags.iter().any(|d| {
                d.message.contains("cannot find module")
                    || d.message.contains("not exported")
            })
        });
        assert!(
            !has_module_error,
            "stdlib import should resolve without errors. Diagnostics: {:?}",
            output.diagnostics_by_uri.values().flatten().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn project_root_module_paths() {
        // Simulates a monorepo layout where the workspace root is a parent
        // directory containing a sub-project with main.spark.
        // Files should get module paths relative to the sub-project root,
        // not the workspace root.
        let tmp = TempDir::new().expect("failed to create temp dir");
        let root = tmp.path();

        // Create a sub-project: root/subproject/main.spark imports mymod
        let project_dir = root.join("subproject");
        fs::create_dir_all(project_dir.join("mymod")).unwrap();
        fs::write(
            project_dir.join("main.spark"),
            "import mymod { greet }\nfn main(): Void { greet() }",
        ).unwrap();
        fs::write(
            project_dir.join("mymod/mymod.spark"),
            "module mymod\npub fn greet(): Void { }",
        ).unwrap();

        // Build file list with URIs and sources
        let files: Vec<(Url, String)> = vec![
            (
                Url::from_file_path(project_dir.join("main.spark")).unwrap(),
                "import mymod { greet }\nfn main(): Void { greet() }".to_string(),
            ),
            (
                Url::from_file_path(project_dir.join("mymod/mymod.spark")).unwrap(),
                "module mymod\npub fn greet(): Void { }".to_string(),
            ),
        ];

        // Analyze with the parent as workspace root (not the project dir).
        let output = analyze_workspace(&files, &[], root);

        let has_import_error = output.diagnostics_by_uri.values().any(|diags| {
            diags.iter().any(|d| {
                d.message.contains("cannot find module")
                    || d.message.contains("not exported")
            })
        });
        assert!(
            !has_import_error,
            "Import should resolve when workspace root is a parent of the project. Diagnostics: {:?}",
            output.diagnostics_by_uri.values().flatten().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn stdlib_relative_path_detection() {
        // Files inside spark_stdlib should get correct relative paths.
        let path = Path::new("/workspace/crates/spark_stdlib/spark/math.spark");
        let rel = stdlib_relative_path(path);
        assert_eq!(rel, Some(PathBuf::from("spark/math.spark")));

        // Non-stdlib files should return None.
        let path = Path::new("/workspace/src/main.spark");
        let rel = stdlib_relative_path(path);
        assert_eq!(rel, None);
    }

    #[test]
    fn borrow_error_surfaces_in_diagnostics() {
        let output = analyze_workspace_sources(&[(
            "main.spark",
            "fn main(): Int {\n    x: Int! = 5\n    a: Ref[Int]! = x\n    b: Ref[Int]! = x\n    a = 10\n    print(b)\n    return 0\n}",
        )]);

        let all_messages: Vec<&str> = output
            .diagnostics_by_uri
            .values()
            .flatten()
            .map(|d| d.message.as_str())
            .collect();
        assert!(
            all_messages.iter().any(|m| m.contains("cannot borrow")),
            "Expected borrow checker error in diagnostics, got: {all_messages:?}"
        );
    }

    #[test]
    fn borrow_error_with_type_error_in_other_file() {
        // Simulates a workspace where one file has a type error and
        // another has a borrow error. Both should get diagnostics.
        let output = analyze_workspace_sources(&[
            (
                "main.spark",
                "fn main(): Int {\n    x: Int! = 5\n    a: Ref[Int]! = x\n    b: Ref[Int]! = x\n    a = 10\n    print(b)\n    return 0\n}",
            ),
            (
                "bad.spark",
                "fn broken(): Int { }",  // type error: missing return value
            ),
        ]);

        let all_messages: Vec<String> = output
            .diagnostics_by_uri
            .values()
            .flatten()
            .map(|d| d.message.clone())
            .collect();
        eprintln!("all diagnostics: {all_messages:?}");

        // The type error file should have its error.
        assert!(
            all_messages.iter().any(|m| !m.contains("cannot borrow")),
            "Expected type error for bad.spark"
        );
        // The borrow error file should ALSO have its error.
        assert!(
            all_messages.iter().any(|m| m.contains("cannot borrow")),
            "Expected borrow error even with type errors in other file, got: {all_messages:?}"
        );
    }

    #[test]
    fn find_project_roots_detects_main_spark() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        fs::create_dir_all(root.join("proj_a")).unwrap();
        fs::write(root.join("proj_a/main.spark"), "").unwrap();
        fs::create_dir_all(root.join("proj_b")).unwrap();
        fs::write(root.join("proj_b/main.spark"), "").unwrap();

        let files: Vec<(Url, String)> = vec![
            (Url::from_file_path(root.join("proj_a/main.spark")).unwrap(), String::new()),
            (Url::from_file_path(root.join("proj_b/main.spark")).unwrap(), String::new()),
            (Url::from_file_path(root.join("proj_a/lib.spark")).unwrap(), String::new()),
        ];

        let roots = find_project_roots(&files);
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().any(|r| r.ends_with("proj_a")));
        assert!(roots.iter().any(|r| r.ends_with("proj_b")));
    }

    #[test]
    fn path_dependency_resolves_imports() {
        // Workspace with a Spark.toml path dep.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let lib_dir = tmp.path().join("libs/mylib");

        // Create the dependency library.
        // File lives at the dep root (not in a subdir) so that resolve_path_dep
        // produces module_path = "mylib/mylib.spark".
        fs::create_dir_all(&lib_dir).unwrap();
        fs::write(
            lib_dir.join("mylib.spark"),
            "module mylib\npub fn helper(): Int { return 42 }\n",
        )
        .unwrap();

        // Create main project.
        fs::write(
            root.join("main.spark"),
            "import mylib { helper }\nfn main(): Void { helper() }",
        )
        .unwrap();

        // Resolve dependencies.
        let (dep_sources, dep_errors) = resolve_dependencies(root);
        // No Spark.toml exists yet, so no deps.
        assert!(dep_sources.is_empty());
        assert!(dep_errors.is_empty());

        // Write Spark.toml.
        fs::write(
            root.join("Spark.toml"),
            format!(
                "[project]\nname = \"test\"\nversion = \"0.1.0\"\n\n[dependencies]\nmylib = {{ path = \"{}\" }}\n",
                lib_dir.display()
            ),
        )
        .unwrap();

        // Now resolve should find the dependency.
        let (dep_sources, dep_errors) = resolve_dependencies(root);
        assert!(dep_errors.is_empty(), "unexpected errors: {dep_errors:?}");
        assert_eq!(dep_sources.len(), 1, "expected 1 dep source file");
        assert!(
            dep_sources[0].module_path.display().to_string().contains("mylib"),
            "expected mylib in module path, got: {}",
            dep_sources[0].module_path.display()
        );

        // Build workspace files.
        let main_path = root.join("main.spark");
        let main_source = fs::read_to_string(&main_path).unwrap();
        let files = vec![(
            Url::from_file_path(&main_path).unwrap(),
            main_source,
        )];

        // Analyze with deps — import should resolve.
        let output = analyze_workspace(&files, &dep_sources, root);
        let has_import_error = output.diagnostics_by_uri.values().any(|diags| {
            diags.iter().any(|d| {
                d.message.contains("cannot find module")
                    || d.message.contains("not exported")
            })
        });
        assert!(
            !has_import_error,
            "Import from path dep should resolve. Diagnostics: {:?}",
            output.diagnostics_by_uri.values().flatten().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn transitive_path_deps_resolve() {
        // Mirrors the transitive_deps e2e test fixture layout:
        //   root/main.spark            imports lib_b
        //   root/libs/lib_b/lib_b.spark  imports lib_c
        //   root/libs/lib_c/lib_c.spark  provides base_value
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let lib_b = tmp.path().join("libs/lib_b");
        let lib_c = tmp.path().join("libs/lib_c");

        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&lib_b).unwrap();
        fs::create_dir_all(&lib_c).unwrap();

        // lib_c: provides `doubled` function.
        fs::write(
            lib_c.join("lib_c.spark"),
            "module lib_c\npub fn doubled(x: Int): Int { return x * 2 }\n",
        )
        .unwrap();
        fs::write(
            lib_c.join("Spark.toml"),
            "[project]\nname = \"lib_c\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        // lib_b: imports from lib_c (depends on lib_c).
        fs::write(
            lib_b.join("lib_b.spark"),
            "module lib_b\nimport lib_c { doubled }\npub fn helper(): Int { return doubled(21) }\n",
        )
        .unwrap();
        fs::write(
            lib_b.join("Spark.toml"),
            format!(
                "[project]\nname = \"lib_b\"\nversion = \"0.1.0\"\n\n[dependencies]\nlib_c = {{ path = \"{}\" }}\n",
                lib_c.display()
            ),
        )
        .unwrap();

        // root: depends on lib_b, uses helper.
        fs::write(
            root.join("main.spark"),
            "import lib_b { helper }\nfn main(): Void { helper() }",
        )
        .unwrap();
        fs::write(
            root.join("Spark.toml"),
            format!(
                "[project]\nname = \"root\"\nversion = \"0.1.0\"\n\n[dependencies]\nlib_b = {{ path = \"{}\" }}\n",
                lib_b.display()
            ),
        )
        .unwrap();

        // Resolve deps — should find both lib_b and lib_c.
        let (dep_sources, dep_errors) = resolve_dependencies(&root);
        assert!(dep_errors.is_empty(), "unexpected errors: {dep_errors:?}");
        assert_eq!(dep_sources.len(), 2, "expected lib_b + lib_c sources");

        // Analyze.
        let main_path = root.join("main.spark");
        let main_source = fs::read_to_string(&main_path).unwrap();
        let files = vec![(
            Url::from_file_path(&main_path).unwrap(),
            main_source,
        )];
        let output = analyze_workspace(&files, &dep_sources, &root);
        let has_import_error = output.diagnostics_by_uri.values().any(|diags| {
            diags.iter().any(|d| {
                d.message.contains("cannot find module")
                    || d.message.contains("not exported")
            })
        });
        assert!(
            !has_import_error,
            "Transitive deps should resolve. Diagnostics: {:?}",
            output.diagnostics_by_uri.values().flatten().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn dep_files_deduplicated() {
        // When a path dep is inside the workspace root, its files are
        // already discovered by the directory walk. The dep_files should
        // be deduplicated so they aren't analyzed twice.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Create a lib inside the workspace root.
        fs::create_dir_all(root.join("mylib")).unwrap();
        fs::write(
            root.join("mylib/mylib.spark"),
            "module mylib\npub fn helper(): Int { return 1 }\n",
        )
        .unwrap();
        fs::write(
            root.join("main.spark"),
            "fn main(): Void { }",
        )
        .unwrap();

        // Discover files (includes mylib from directory walk).
        let spark_files = discover_spark_files(root);
        let files: Vec<(Url, String)> = spark_files
            .iter()
            .filter_map(|p| {
                let uri = file_path_to_uri(p)?;
                let source = fs::read_to_string(p).ok()?;
                Some((uri, source))
            })
            .collect();

        // Create a duplicate dep entry for the same file.
        let mylib_path = root.join("mylib/mylib.spark");
        let dep_sources = vec![DependencySource {
            uri: file_path_to_uri(&mylib_path).unwrap(),
            source: fs::read_to_string(&mylib_path).unwrap(),
            module_path: PathBuf::from("mylib/mylib.spark"),
        }];

        let output = analyze_workspace(&files, &dep_sources, root);

        // Count how many file:// URIs map to the mylib file.
        let mylib_count = output
            .file_id_to_uri
            .values()
            .filter(|u| u.scheme() == "file" && u.path().contains("mylib"))
            .count();
        assert_eq!(
            mylib_count, 1,
            "mylib should appear exactly once (deduplicated), got {mylib_count}"
        );
    }

    #[test]
    fn in_workspace_deps_get_module_path_override() {
        // When dependency source files live inside the workspace directory,
        // the directory walk discovers them with workspace-relative paths
        // (e.g., `libs/lib_b/lib_b.spark`). The dep resolution provides the
        // correct dep-prefixed module paths (e.g., `lib_b/lib_b.spark`).
        // The override must happen so `import lib_b { ... }` resolves.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Create dep inside the workspace at libs/mylib/mylib.spark.
        fs::create_dir_all(root.join("libs/mylib")).unwrap();
        fs::write(
            root.join("libs/mylib/mylib.spark"),
            "module mylib\npub fn helper(): Int { return 1 }\n",
        )
        .unwrap();
        fs::write(
            root.join("main.spark"),
            "import mylib { helper }\nfn main(): Void { helper() }",
        )
        .unwrap();

        // Directory walk discovers both main.spark and libs/mylib/mylib.spark.
        let spark_files = discover_spark_files(root);
        let files: Vec<(Url, String)> = spark_files
            .iter()
            .filter_map(|p| {
                let uri = file_path_to_uri(p)?;
                let source = fs::read_to_string(p).ok()?;
                Some((uri, source))
            })
            .collect();

        // Without deps, the import fails (path is libs/mylib/mylib.spark).
        let output_no_deps = analyze_workspace(&files, &[], root);
        let has_error_without_deps = output_no_deps.diagnostics_by_uri.values().any(|diags| {
            diags.iter().any(|d| d.message.contains("cannot find module"))
        });
        assert!(
            has_error_without_deps,
            "Without dep resolution, import should fail"
        );

        // With dep resolution providing the correct module path, it resolves.
        let mylib_path = root.join("libs/mylib/mylib.spark");
        let dep_sources = vec![DependencySource {
            uri: file_path_to_uri(&mylib_path).unwrap(),
            source: fs::read_to_string(&mylib_path).unwrap(),
            module_path: PathBuf::from("mylib/mylib.spark"),
        }];
        let output_with_deps = analyze_workspace(&files, &dep_sources, root);
        let has_import_error = output_with_deps.diagnostics_by_uri.values().any(|diags| {
            diags.iter().any(|d| {
                d.message.contains("cannot find module")
                    || d.message.contains("not exported")
            })
        });
        assert!(
            !has_import_error,
            "With dep resolution, import should resolve. Diagnostics: {:?}",
            output_with_deps.diagnostics_by_uri.values().flatten().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn transitive_deps_fixture_layout() {
        // Exact replica of the transitive_deps e2e fixture.
        // root/main.spark              imports lib_b
        // root/libs/lib_b/lib_b.spark  imports lib_c (dep: path = "../lib_c")
        // root/libs/lib_c/lib_c.spark  provides base_value
        let tmp = TempDir::new().unwrap();
        // Canonicalize like the LSP backend does (macOS: /var → /private/var).
        let root = tmp.path().canonicalize().unwrap();

        fs::create_dir_all(root.join("libs/lib_b")).unwrap();
        fs::create_dir_all(root.join("libs/lib_c")).unwrap();

        fs::write(
            root.join("main.spark"),
            "import lib_b { doubled }\nfn main(): Int {\n    print(doubled())\n    return 0\n}\n",
        ).unwrap();
        fs::write(
            root.join("Spark.toml"),
            "[project]\nname = \"transitive_test\"\nversion = \"0.1.0\"\n\n[dependencies]\nlib_b = { path = \"libs/lib_b\" }\n",
        ).unwrap();
        fs::write(
            root.join("libs/lib_b/lib_b.spark"),
            "module lib_b\n\nimport lib_c { base_value }\n\npub fn doubled(): Int {\n    return base_value() * 2\n}\n",
        ).unwrap();
        fs::write(
            root.join("libs/lib_b/Spark.toml"),
            "[project]\nname = \"lib_b\"\nversion = \"0.1.0\"\n\n[dependencies]\nlib_c = { path = \"../lib_c\" }\n",
        ).unwrap();
        fs::write(
            root.join("libs/lib_c/lib_c.spark"),
            "module lib_c\n\npub fn base_value(): Int {\n    return 10\n}\n",
        ).unwrap();
        fs::write(
            root.join("libs/lib_c/Spark.toml"),
            "[project]\nname = \"lib_c\"\nversion = \"0.1.0\"\n",
        ).unwrap();

        // 1. Resolve dependencies the same way the LSP backend does.
        let (dep_sources, dep_errors) = resolve_dependencies(&root);
        assert!(dep_errors.is_empty(), "dep errors: {dep_errors:?}");
        assert_eq!(dep_sources.len(), 2, "expected lib_b + lib_c");

        // Print for debugging.
        for ds in &dep_sources {
            eprintln!("dep: uri={} module_path={}", ds.uri, ds.module_path.display());
        }

        // 2. Discover files the same way the LSP backend does.
        let spark_files = discover_spark_files(&root);
        let files: Vec<(Url, String)> = spark_files
            .iter()
            .filter_map(|p| {
                let uri = file_path_to_uri(p)?;
                let source = fs::read_to_string(p).ok()?;
                Some((uri, source))
            })
            .collect();

        // Print for debugging.
        for (uri, _) in &files {
            eprintln!("discovered: {uri}");
        }

        // 3. Analyze with deps — all imports should resolve.
        let output = analyze_workspace(&files, &dep_sources, &root);

        let all_diags: Vec<&str> = output
            .diagnostics_by_uri
            .values()
            .flatten()
            .map(|d| d.message.as_str())
            .collect();
        let has_import_error = all_diags.iter().any(|m| {
            m.contains("cannot find module")
                || m.contains("not exported")
                || m.contains("cannot find value")
        });
        assert!(
            !has_import_error,
            "All imports should resolve in transitive_deps fixture. Errors: {all_diags:?}"
        );
    }

    #[test]
    fn no_spark_toml_graceful() {
        // No Spark.toml — resolve_dependencies should return empty, no errors.
        let tmp = TempDir::new().unwrap();
        let (deps, errors) = resolve_dependencies(tmp.path());
        assert!(deps.is_empty());
        assert!(errors.is_empty());
    }
}
