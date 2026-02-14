//! Workspace-level state and multi-file analysis pipeline.
//!
//! When the LSP server has a workspace root, all `.spark` files are
//! discovered, parsed, and analyzed together through the full compiler
//! frontend pipeline (lex → parse → modules → resolve → type-check).
//! This enables cross-file features such as go-to-definition across
//! files and workspace-wide find-references.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ropey::Rope;
use tower_lsp::lsp_types::{self, Url};

use spark_diagnostics::Diagnostics;
use spark_lexer::Lexer;
use spark_modules::ModuleResolverInput;
use spark_parser::parse;
use spark_resolve::{DefinitionTable, ResolvedAst, ResolverInput};
use spark_span::{FileId, SourceMap};
use spark_types::{TypeCheckerInput, TypeTable};

use crate::document;

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

/// Runs the full compiler frontend pipeline on all workspace files.
///
/// Mirrors `spark_driver::pipeline` phases 1–5:
/// 1. Lex + parse all files
/// 2. Module resolution (builds module graph from file paths)
/// 3. Name resolution (resolves identifiers across all files)
/// 4. Type checking
///
/// Each phase runs regardless of earlier errors — diagnostics accumulate
/// per-file so that all errors are reported.
pub fn analyze_workspace(
    files: &[(Url, String)],
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

    for (uri, source) in files {
        let file_path = uri
            .to_file_path()
            .unwrap_or_else(|()| PathBuf::from("untitled.spark"));
        let file_name = file_path.to_string_lossy();

        let file_id = source_map.add_file(&*file_name, source);
        file_id_to_uri.insert(file_id, uri.clone());
        uri_to_file_id.insert(uri.clone(), file_id);
        ropes.insert(file_id, Rope::from_str(source));
        file_paths.insert(file_id, file_path);

        // Phase 1: Lex
        let tokens = Lexer::new(file_id, source).tokenize();

        // Phase 2: Parse
        let (ast, parse_diags) = parse(tokens, file_id);
        all_diagnostics.extend(parse_diags);
        asts.insert(file_id, ast);
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

    let resolved = resolve_output.resolved.clone();
    let definitions = resolve_output.definitions.clone();

    // Phase 5: Type checking
    let type_table = {
        let type_output = spark_types::check(TypeCheckerInput {
            resolved: resolve_output.resolved,
            definitions: resolve_output.definitions,
            graph,
        });
        all_diagnostics.extend(type_output.diagnostics);
        Some(type_output.table)
    };

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

        analyze_workspace(&files, root)
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

    #[test]
    fn single_file_analysis_works() {
        let output = analyze_workspace_sources(&[
            ("main.spark", "fn main(): Void { x: Int = 42\nx }"),
        ]);
        assert_eq!(output.file_id_to_uri.len(), 1);
        assert!(output.type_table.is_some());
    }

    #[test]
    fn multi_file_analysis_produces_results() {
        let output = analyze_workspace_sources(&[
            ("main.spark", "fn main(): Void { }"),
            ("utils.spark", "fn helper(): Int { return 1 }"),
        ]);
        assert_eq!(output.file_id_to_uri.len(), 2);
        assert_eq!(output.resolved.asts.len(), 2);
    }

    #[test]
    fn diagnostics_distributed_per_file() {
        let output = analyze_workspace_sources(&[
            ("good.spark", "fn main(): Void { }"),
            ("bad.spark", "fn broken(): Int { }"),  // missing return value
        ]);

        // Each file should have its own diagnostics entry
        assert_eq!(
            output.diagnostics_by_uri.len(),
            2,
            "Expected diagnostics entry for each file"
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
    fn discover_skips_target_directory() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        fs::write(root.join("main.spark"), "fn main(): Void { }").unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("target/debug/output.spark"), "fn x(): Void { }").unwrap();

        let files = discover_spark_files(root);
        assert_eq!(files.len(), 1);
    }
}
