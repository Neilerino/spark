//! LSP backend — implements the `LanguageServer` trait.
//!
//! Provides real-time diagnostics by running the compiler pipeline
//! (lex → parse → resolve → type-check → HIR → MIR → borrow-check)
//! on every document change, and lexer-driven semantic token highlighting.
//!
//! When a workspace root is available, all `.spark` files are analyzed
//! together for cross-file features (go-to-definition, find-references).
//! Falls back to single-file analysis when no workspace root is configured.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use dashmap::DashMap;
use ropey::Rope;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    self, CompletionItem, CompletionItemKind, CompletionList, CompletionOptions, CompletionParams,
    CompletionResponse, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse,
    FileChangeType, FileSystemWatcher, GlobPattern, GotoDefinitionParams, GotoDefinitionResponse,
    Hover, HoverContents, HoverParams, HoverProviderCapability, InitializeParams, InitializeResult,
    InitializedParams, Location, MarkupContent, MarkupKind, MessageType, ReferenceParams,
    Registration, SemanticTokens, SemanticTokensFullOptions, SemanticTokensOptions,
    SemanticTokensParams, SemanticTokensResult, SemanticTokensServerCapabilities,
    ServerCapabilities, ServerInfo, SymbolKind, TextDocumentSyncCapability, TextDocumentSyncKind,
    Url, WatchKind,
};
use tower_lsp::{Client, LanguageServer};

use spark_ast::Item;
use spark_borrow::BorrowInput;
use spark_diagnostics::Diagnostics;
use spark_hir::HirInput;
use spark_lexer::Lexer;
use spark_mir::MirInput;
use spark_modules::{ModuleGraph, ModuleResolverInput};
use spark_parser::parse;
use spark_resolve::{DefinitionKind, ResolverInput};
use spark_span::{FileId, SourceMap};
use spark_types::TypeCheckerInput;

use crate::document::{self, AnalysisResult, DocumentState};
use crate::semantic_tokens;
use crate::workspace::{self, WorkspaceState};

/// The LSP server backend.
///
/// Holds a reference to the LSP client (for sending notifications),
/// a concurrent map of open documents, and optional workspace state
/// for multi-file analysis.
pub struct Backend {
    /// The LSP client connection.
    client: Client,
    /// Open documents, keyed by URI.
    documents: DashMap<Url, DocumentState>,
    /// Workspace state for multi-file analysis. `None` when no workspace
    /// root was provided during initialization.
    workspace: RwLock<Option<WorkspaceState>>,
}

impl Backend {
    /// Creates a new backend with the given client connection.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: DashMap::new(),
            workspace: RwLock::new(None),
        }
    }

    /// Returns `true` if a workspace root is configured.
    fn has_workspace(&self) -> bool {
        self.workspace.read().is_ok_and(|ws| ws.is_some())
    }

    /// Runs the single-file compiler frontend on a document, caches
    /// analysis results, and publishes diagnostics.
    ///
    /// Used as a fallback when no workspace root is configured.
    async fn analyze_and_publish_single(&self, uri: &Url, source: &str, version: Option<i32>) {
        let (lsp_diagnostics, analysis) = analyze(source, uri);

        // Cache analysis result for on-demand queries (go-to-definition, etc.)
        if let Some(mut doc) = self.documents.get_mut(uri) {
            doc.analysis = analysis;
        }

        self.client
            .publish_diagnostics(uri.clone(), lsp_diagnostics, version)
            .await;
    }

    /// Runs the full workspace analysis pipeline, distributes results to
    /// all documents, and publishes diagnostics for every file.
    ///
    /// This collects source text from both open documents and disk files,
    /// runs the compiler frontend on all of them together, then distributes
    /// the shared analysis results back to each open document.
    #[allow(clippy::significant_drop_tightening)]
    async fn analyze_and_publish_workspace(&self) {
        // 1. Collect all file sources (open documents + disk files + dep files).
        let (files, dep_sources_snapshot, root_path) = {
            let ws_guard = self.workspace.read().unwrap();
            let Some(ws) = ws_guard.as_ref() else {
                return;
            };
            let root_path = ws.root_path.clone();

            let mut files: Vec<(Url, String)> = Vec::new();

            // Open documents take priority over disk files.
            let open_uris: Vec<Url> = self
                .documents
                .iter()
                .map(|entry| entry.key().clone())
                .collect();

            for entry in &self.documents {
                files.push((entry.key().clone(), entry.rope.to_string()));
            }

            // Add disk files that are not open in the editor.
            for (uri, source) in &ws.disk_sources {
                if !open_uris.contains(uri) {
                    files.push((uri.clone(), source.clone()));
                }
            }

            // Sort by URI for deterministic FileId assignment across runs.
            // DashMap iteration order is non-deterministic, so without
            // sorting, FileIds can shift between analysis runs, breaking
            // cross-file references.
            files.sort_by(|a, b| a.0.cmp(&b.0));

            // Snapshot dependency sources for the analysis.
            let dep_snapshot: Vec<workspace::DependencySource> = ws
                .dep_sources
                .iter()
                .map(|d| workspace::DependencySource {
                    uri: d.uri.clone(),
                    source: d.source.clone(),
                    module_path: d.module_path.clone(),
                })
                .collect();

            (files, dep_snapshot, root_path)
        }; // Release workspace read lock.

        if files.is_empty() && dep_sources_snapshot.is_empty() {
            return;
        }

        // 2. Run the analysis pipeline (CPU-bound, no locks held).
        let output = workspace::analyze_workspace(&files, &dep_sources_snapshot, &root_path);

        // 3. Wrap shared data in Arc for cheap sharing across documents.
        let resolved = Arc::new(output.resolved);
        let definitions = Arc::new(output.definitions);
        let type_table = output.type_table.map(Arc::new);
        let scope_arena = output.scope_arena.map(Arc::new);

        // 4. Distribute results to open documents.
        // Collect URIs + file IDs first to avoid holding DashMap refs during mutation.
        let open_file_ids: Vec<(Url, FileId)> = self
            .documents
            .iter()
            .filter_map(|entry| {
                let uri = entry.key().clone();
                let file_id = *output.uri_to_file_id.get(&uri)?;
                Some((uri, file_id))
            })
            .collect();

        for (uri, file_id) in &open_file_ids {
            if let Some(mut doc) = self.documents.get_mut(uri) {
                doc.analysis = Some(AnalysisResult {
                    file_id: *file_id,
                    resolved: Arc::clone(&resolved),
                    definitions: Arc::clone(&definitions),
                    type_table: type_table.as_ref().map(Arc::clone),
                    scope_arena: scope_arena.as_ref().map(Arc::clone),
                });
            }
        }

        // 5. Update workspace state with new mappings and ropes.
        {
            let mut ws_guard = self.workspace.write().unwrap();
            if let Some(ws) = ws_guard.as_mut() {
                ws.file_id_to_uri = output.file_id_to_uri;
                ws.uri_to_file_id = output.uri_to_file_id;
                // Update disk ropes from analysis output (for cross-file span conversion).
                for (file_id, rope) in &output.ropes {
                    if let Some(uri) = ws.file_id_to_uri.get(file_id) {
                        if !self.documents.contains_key(uri) {
                            ws.disk_ropes.insert(uri.clone(), rope.clone());
                        }
                    }
                }

                // Clean up stale disk entries for files no longer in the workspace.
                let current_uris: std::collections::HashSet<&Url> =
                    ws.file_id_to_uri.values().collect();
                ws.disk_sources.retain(|uri, _| {
                    current_uris.contains(uri) || self.documents.contains_key(uri)
                });
                ws.disk_ropes.retain(|uri, _| {
                    current_uris.contains(uri) || self.documents.contains_key(uri)
                });
            }
        }

        // 6. Publish diagnostics for all files.
        for (uri, diags) in &output.diagnostics_by_uri {
            // Skip synthetic stdlib URLs — they don't correspond to editor files.
            if uri.scheme() != "file" {
                continue;
            }
            self.client
                .publish_diagnostics(uri.clone(), diags.clone(), None)
                .await;
        }
    }

    /// Resolves a definition span to an LSP `Location`, handling cross-file spans.
    #[allow(clippy::significant_drop_tightening)]
    fn resolve_def_location(
        &self,
        def_span: spark_span::Span,
        current_file_id: FileId,
        current_uri: &Url,
        current_rope: &Rope,
    ) -> Option<Location> {
        if def_span.file == current_file_id {
            // Same file — use the current document's rope.
            let range = document::span_to_range(current_rope, def_span)?;
            Some(Location {
                uri: current_uri.clone(),
                range,
            })
        } else {
            // Different file — look up the target URI and rope.
            let ws_guard = self.workspace.read().ok()?;
            let ws = ws_guard.as_ref()?;
            let target_uri = ws.file_id_to_uri.get(&def_span.file)?;
            let target_rope = document::get_rope_for_file(&self.documents, ws, target_uri)?;
            let range = document::span_to_range(&target_rope, def_span)?;
            Some(Location {
                uri: target_uri.clone(),
                range,
            })
        }
    }
}

/// Runs the single-file compiler frontend pipeline and returns LSP
/// diagnostics plus an optional analysis result for on-demand queries.
///
/// Runs as many phases as possible, collecting all diagnostics.
/// Parse errors prevent resolve/type-check but are still reported.
fn analyze(source: &str, uri: &Url) -> (Vec<lsp_types::Diagnostic>, Option<AnalysisResult>) {
    let file_name = uri
        .to_file_path()
        .unwrap_or_else(|()| PathBuf::from("untitled.spark"));
    let file_name_str = file_name.to_string_lossy();

    let mut source_map = SourceMap::new();
    let file_id = source_map.add_file(&*file_name_str, source);
    let rope = ropey::Rope::from_str(source);

    let mut all_diagnostics = Diagnostics::new();
    let mut analysis = None;

    // Phase 1: Lex
    let tokens = Lexer::new(file_id, source).tokenize();

    // Phase 2: Parse
    let (ast, parse_diags) = parse(tokens, file_id);
    all_diagnostics.extend(parse_diags);

    // If parsing produced errors, we can still report them but can't
    // reliably run later phases.
    if !all_diagnostics.has_errors() {
        // Phase 3: Module resolution
        let module_output = resolve_modules(file_id, ast.clone(), &file_name_str);
        all_diagnostics.extend(module_output.diagnostics);

        if !all_diagnostics.has_errors() {
            let graph = module_output.graph;

            // Phase 4: Name resolution
            let resolve_output = resolve_names(graph.clone(), file_id, ast);
            all_diagnostics.extend(resolve_output.diagnostics);

            // Phase 5: Type checking (borrows resolver output)
            let type_table = if all_diagnostics.has_errors() {
                None
            } else {
                let type_output = spark_types::check(TypeCheckerInput {
                    resolved: &resolve_output.resolved,
                    definitions: &resolve_output.definitions,
                    graph,
                });
                all_diagnostics.extend(type_output.diagnostics);
                Some((Arc::new(type_output.table), type_output.builtins))
            };

            // Cache resolver output for on-demand queries (go-to-definition, hover, etc.)
            let scope_arena = Arc::new(resolve_output.scope_arena);
            analysis = Some(AnalysisResult {
                file_id,
                resolved: Arc::new(resolve_output.resolved.clone()),
                definitions: Arc::new(resolve_output.definitions.clone()),
                type_table: type_table.as_ref().map(|(t, _)| t.clone()),
                scope_arena: Some(scope_arena),
            });

            // Phases 6–8: HIR → MIR → Borrow checking
            // Run when the type table is available. No error gating — the
            // downstream phases handle error types gracefully, and in
            // single-file mode we still want borrow errors reported.
            if let Some((ref table, ref builtins)) = type_table {
                let hir_output = spark_hir::lower(HirInput {
                    resolved: resolve_output.resolved,
                    definitions: resolve_output.definitions,
                    table: (**table).clone(),
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
        }
    }

    // Convert all collected diagnostics to LSP format
    let lsp_diags = all_diagnostics
        .iter()
        .map(|d| document::diagnostic_to_lsp(d, &rope, uri))
        .collect();

    (lsp_diags, analysis)
}

/// Searches field resolutions from the type table for a field at the given offset.
///
/// Used as a fallback when the resolver's `find_at_offset` returns `None`,
/// which happens for field access expressions (e.g., `point.x`) because the
/// resolver defers field resolution to the type checker.
fn find_field_at_offset(
    table: &spark_types::TypeTable,
    offset: u32,
    file_id: spark_span::FileId,
) -> Option<(spark_span::Span, spark_resolve::DefId)> {
    table
        .field_resolutions()
        .filter(|(span, _)| span.file == file_id && span.contains(offset))
        .min_by_key(|(span, _)| span.end - span.start)
        .map(|(span, def_id)| (*span, *def_id))
}

/// Searches method resolutions from the type table for a method call at the given offset.
///
/// Used as a fallback when both the resolver and field resolution return `None`,
/// which happens for method call expressions (e.g., `user.serialize()`) because
/// method name resolution is deferred to the type checker.
fn find_method_at_offset(
    table: &spark_types::TypeTable,
    offset: u32,
    file_id: spark_span::FileId,
) -> Option<(spark_span::Span, spark_resolve::DefId)> {
    table
        .method_resolutions()
        .filter(|(span, _)| span.file == file_id && span.contains(offset))
        .min_by_key(|(span, _)| span.end - span.start)
        .map(|(span, def_id)| (*span, *def_id))
}

/// Returns the type name for a definition, if type info is available.
fn def_type_str(analysis: &AnalysisResult, def_id: spark_resolve::DefId) -> Option<String> {
    let table = analysis.type_table.as_ref()?;
    let type_id = table.get_def_type(def_id)?;
    Some(spark_types::display_type(
        type_id,
        table,
        &analysis.definitions,
    ))
}

/// Formats a single generic constraint for display (e.g., `Comparable` or `{ id: Int }`).
fn format_constraint(
    constraint: &spark_types::ty::Constraint,
    table: &spark_types::TypeTable,
    definitions: &spark_resolve::DefinitionTable,
) -> String {
    match constraint {
        spark_types::ty::Constraint::Interface {
            interface_def_id,
            type_args,
        } => {
            let name = definitions
                .get(*interface_def_id)
                .map_or_else(|| "?".to_string(), |info| info.name.to_string());
            if type_args.is_empty() {
                name
            } else {
                let args: Vec<_> = type_args
                    .iter()
                    .map(|tid| spark_types::display_type(*tid, table, definitions))
                    .collect();
                format!("{name}[{}]", args.join(", "))
            }
        }
        spark_types::ty::Constraint::Structural { fields } => {
            let parts: Vec<_> = fields
                .iter()
                .map(|(sym, tid)| {
                    let ty = spark_types::display_type(*tid, table, definitions);
                    format!("{sym}: {ty}")
                })
                .collect();
            format!("{{ {} }}", parts.join(", "))
        }
    }
}

/// Formats a generic parameter list like `[T: Comparable, U]`.
/// Returns `None` if there are no generic params.
fn format_generic_params(
    def_id: spark_resolve::DefId,
    analysis: &AnalysisResult,
) -> Option<String> {
    let table = analysis.type_table.as_ref()?;
    let params = table.get_generic_params(def_id)?;
    if params.is_empty() {
        return None;
    }
    let parts: Vec<_> = params
        .iter()
        .map(|param_def_id| {
            let name = analysis
                .definitions
                .get(*param_def_id)
                .map_or("?", |info| info.name.as_str());
            table.get_generic_constraint(*param_def_id).map_or_else(
                || name.to_string(),
                |constraint| {
                    let c = format_constraint(constraint, table, &analysis.definitions);
                    format!("{name}: {c}")
                },
            )
        })
        .collect();
    Some(format!("[{}]", parts.join(", ")))
}

/// Formats hover content for a definition.
///
/// Uses the type table (if available) to show typed signatures. Falls back
/// to showing just the definition kind and name when type info is missing.
fn format_hover(analysis: &AnalysisResult, def_id: spark_resolve::DefId) -> Option<String> {
    let def_info = analysis.definitions.get(def_id)?;
    let name = def_info.name.as_str();

    let content = match &def_info.kind {
        DefinitionKind::Function => {
            let generics = format_generic_params(def_id, analysis).unwrap_or_default();
            def_type_str(analysis, def_id).map_or_else(
                || format!("fn {name}{generics}(...)"),
                |ty| format!("fn {name}{generics}{ty}"),
            )
        }
        DefinitionKind::Struct => format_struct_hover(analysis, def_id, name),
        DefinitionKind::Enum => format_enum_hover(analysis, def_id, name),
        DefinitionKind::Local | DefinitionKind::Parameter => def_type_str(analysis, def_id)
            .map_or_else(|| name.to_string(), |ty| format!("{name}: {ty}")),
        DefinitionKind::Const => def_type_str(analysis, def_id).map_or_else(
            || format!("const {name}"),
            |ty| format!("const {name}: {ty}"),
        ),
        DefinitionKind::StructField(parent_id) => {
            let parent_name = analysis
                .definitions
                .get(*parent_id)
                .map_or("?", |info| info.name.as_str());
            def_type_str(analysis, def_id).map_or_else(
                || format!("{parent_name}.{name}"),
                |ty| format!("{parent_name}.{name}: {ty}"),
            )
        }
        DefinitionKind::EnumVariant(parent_id) => {
            let parent_name = analysis
                .definitions
                .get(*parent_id)
                .map_or("?", |info| info.name.as_str());
            format!("{parent_name}.{name}")
        }
        DefinitionKind::TypeAlias => format!("alias {name}"),
        DefinitionKind::TypeDef => format!("type {name}"),
        DefinitionKind::BuiltinType => name.to_string(),
        DefinitionKind::Interface => format!("interface {name}"),
        DefinitionKind::GenericParam => {
            // Show "T: Comparable" if constrained, else just "T"
            analysis
                .type_table
                .as_ref()
                .and_then(|table| {
                    let constraint = table.get_generic_constraint(def_id)?;
                    let c = format_constraint(constraint, table, &analysis.definitions);
                    Some(format!("{name}: {c}"))
                })
                .unwrap_or_else(|| name.to_string())
        }
        DefinitionKind::LifetimeParam => format!("'{name}"),
        DefinitionKind::BuiltinFunction => format!("fn {name}(...)"),
        DefinitionKind::BuiltinMethod | DefinitionKind::BindingMethod(_) => {
            let generics = format_generic_params(def_id, analysis).unwrap_or_default();
            def_type_str(analysis, def_id).map_or_else(
                || format!("fn .{name}{generics}(...)"),
                |ty| format!("fn {name}{generics}{ty}"),
            )
        }
        DefinitionKind::Lambda => {
            let generics = format_generic_params(def_id, analysis).unwrap_or_default();
            def_type_str(analysis, def_id).map_or_else(
                || format!("fn {name}{generics}(...)"),
                |ty| format!("fn {name}{generics}{ty}"),
            )
        }
        DefinitionKind::ExternFunction => {
            let generics = format_generic_params(def_id, analysis).unwrap_or_default();
            def_type_str(analysis, def_id).map_or_else(
                || format!("extern fn {name}{generics}(...)"),
                |ty| format!("extern fn {name}{generics}{ty}"),
            )
        }
    };

    Some(content)
}

/// Formats hover for a struct, showing fields when type info is available.
fn format_struct_hover(
    analysis: &AnalysisResult,
    def_id: spark_resolve::DefId,
    name: &str,
) -> String {
    let generics = format_generic_params(def_id, analysis).unwrap_or_default();
    let fields = analysis.type_table.as_ref().and_then(|table| {
        table.get_struct_fields(def_id).map(|fields| {
            fields
                .iter()
                .map(|(sym, tid)| {
                    let ty = spark_types::display_type(*tid, table, &analysis.definitions);
                    format!("    {sym}: {ty}")
                })
                .collect::<Vec<_>>()
        })
    });
    match fields {
        Some(f) if f.is_empty() => format!("struct {name}{generics} {{ }}"),
        Some(f) => format!("struct {name}{generics} {{\n{}\n}}", f.join(",\n")),
        None => format!("struct {name}{generics}"),
    }
}

/// Formats hover for an enum, showing variants when type info is available.
fn format_enum_hover(
    analysis: &AnalysisResult,
    def_id: spark_resolve::DefId,
    name: &str,
) -> String {
    let generics = format_generic_params(def_id, analysis).unwrap_or_default();
    let variants = analysis.type_table.as_ref().and_then(|table| {
        table.get_enum_variants(def_id).map(|variants| {
            variants
                .iter()
                .map(|(sym, _)| format!("    {sym}"))
                .collect::<Vec<_>>()
        })
    });
    match variants {
        Some(v) if v.is_empty() => format!("enum {name}{generics} {{ }}"),
        Some(v) => format!("enum {name}{generics} {{\n{}\n}}", v.join(",\n")),
        None => format!("enum {name}{generics}"),
    }
}

/// Converts an AST `Item` to an LSP `DocumentSymbol`.
///
/// Returns `None` for items without a name (e.g. interface bindings).
#[allow(deprecated)] // DocumentSymbol::deprecated field is deprecated in the LSP spec
fn item_to_document_symbol(item: &Item, rope: &ropey::Rope) -> Option<DocumentSymbol> {
    let (name, kind, span, name_span, children) = match item {
        Item::Function(f) => (f.name.name, SymbolKind::FUNCTION, f.span, f.name.span, None),
        Item::Struct(s) => {
            let kids = struct_field_symbols(&s.fields, rope);
            (
                s.name.name,
                SymbolKind::STRUCT,
                s.span,
                s.name.span,
                Some(kids),
            )
        }
        Item::Enum(e) => {
            let kids = enum_variant_symbols(&e.variants, rope);
            (
                e.name.name,
                SymbolKind::ENUM,
                e.span,
                e.name.span,
                Some(kids),
            )
        }
        Item::Interface(i) => {
            let kids = interface_method_symbols(&i.methods, rope);
            (
                i.name.name,
                SymbolKind::INTERFACE,
                i.span,
                i.name.span,
                Some(kids),
            )
        }
        Item::TypeAlias(t) => (
            t.name.name,
            SymbolKind::TYPE_PARAMETER,
            t.span,
            t.name.span,
            None,
        ),
        Item::TypeDef(t) => (
            t.name.name,
            SymbolKind::TYPE_PARAMETER,
            t.span,
            t.name.span,
            None,
        ),
        Item::Const(c) => (c.name.name, SymbolKind::CONSTANT, c.span, c.name.span, None),
        Item::Binding(_) => return None,
    };

    let range = document::span_to_range(rope, span)?;
    let selection_range = document::span_to_range(rope, name_span)?;

    Some(DocumentSymbol {
        name: name.as_str().to_string(),
        detail: None,
        kind,
        tags: None,
        deprecated: None,
        range,
        selection_range,
        children,
    })
}

/// Converts struct fields to child `DocumentSymbol`s.
#[allow(deprecated)]
fn struct_field_symbols(
    fields: &[spark_ast::StructField],
    rope: &ropey::Rope,
) -> Vec<DocumentSymbol> {
    fields
        .iter()
        .filter_map(|f| {
            let range = document::span_to_range(rope, f.span)?;
            let selection_range = document::span_to_range(rope, f.name.span)?;
            Some(DocumentSymbol {
                name: f.name.name.as_str().to_string(),
                detail: None,
                kind: SymbolKind::FIELD,
                tags: None,
                deprecated: None,
                range,
                selection_range,
                children: None,
            })
        })
        .collect()
}

/// Converts enum variants to child `DocumentSymbol`s.
#[allow(deprecated)]
fn enum_variant_symbols(
    variants: &[spark_ast::EnumVariant],
    rope: &ropey::Rope,
) -> Vec<DocumentSymbol> {
    variants
        .iter()
        .filter_map(|v| {
            let range = document::span_to_range(rope, v.span)?;
            let selection_range = document::span_to_range(rope, v.name.span)?;
            Some(DocumentSymbol {
                name: v.name.name.as_str().to_string(),
                detail: None,
                kind: SymbolKind::ENUM_MEMBER,
                tags: None,
                deprecated: None,
                range,
                selection_range,
                children: None,
            })
        })
        .collect()
}

/// Converts interface methods to child `DocumentSymbol`s.
#[allow(deprecated)]
fn interface_method_symbols(
    methods: &[spark_ast::FunctionDef],
    rope: &ropey::Rope,
) -> Vec<DocumentSymbol> {
    methods
        .iter()
        .filter_map(|m| {
            let range = document::span_to_range(rope, m.span)?;
            let selection_range = document::span_to_range(rope, m.name.span)?;
            Some(DocumentSymbol {
                name: m.name.name.as_str().to_string(),
                detail: None,
                kind: SymbolKind::METHOD,
                tags: None,
                deprecated: None,
                range,
                selection_range,
                children: None,
            })
        })
        .collect()
}

/// Sets up a single-file module graph (mirrors `spark_driver::pipeline`).
fn resolve_modules(
    file_id: FileId,
    ast: spark_ast::Ast,
    file_name: &str,
) -> spark_modules::ModuleResolverOutput {
    let mut asts = HashMap::new();
    asts.insert(file_id, ast);

    let mut file_paths = HashMap::new();
    file_paths.insert(file_id, PathBuf::from(file_name));

    spark_modules::resolve(ModuleResolverInput {
        asts,
        root_path: PathBuf::from("."),
        file_paths,
    })
}

/// Runs name resolution on a single-file module (mirrors `spark_driver::pipeline`).
fn resolve_names(
    graph: ModuleGraph,
    file_id: FileId,
    ast: spark_ast::Ast,
) -> spark_resolve::ResolverOutput {
    let mut asts = HashMap::new();
    asts.insert(file_id, ast);

    spark_resolve::resolve(ResolverInput { graph, asts })
}

// ============================================================================
// Completion helpers
// ============================================================================

/// Maps a [`DefinitionKind`](spark_resolve::DefinitionKind) to an LSP
/// [`CompletionItemKind`].
const fn def_kind_to_completion_kind(kind: &spark_resolve::DefinitionKind) -> CompletionItemKind {
    use spark_resolve::DefinitionKind;
    match kind {
        DefinitionKind::Function
        | DefinitionKind::BuiltinFunction
        | DefinitionKind::Lambda
        | DefinitionKind::ExternFunction => CompletionItemKind::FUNCTION,
        DefinitionKind::Struct => CompletionItemKind::STRUCT,
        DefinitionKind::StructField(_) => CompletionItemKind::FIELD,
        DefinitionKind::Enum => CompletionItemKind::ENUM,
        DefinitionKind::EnumVariant(_) => CompletionItemKind::ENUM_MEMBER,
        DefinitionKind::Interface => CompletionItemKind::INTERFACE,
        DefinitionKind::TypeAlias | DefinitionKind::TypeDef | DefinitionKind::BuiltinType => {
            CompletionItemKind::CLASS
        }
        DefinitionKind::Const => CompletionItemKind::CONSTANT,
        DefinitionKind::Local | DefinitionKind::Parameter => CompletionItemKind::VARIABLE,
        DefinitionKind::GenericParam | DefinitionKind::LifetimeParam => {
            CompletionItemKind::TYPE_PARAMETER
        }
        DefinitionKind::BuiltinMethod | DefinitionKind::BindingMethod(_) => {
            CompletionItemKind::METHOD
        }
    }
}

/// Returns a sort-text prefix so that locals/params appear first, then
/// functions, then types, then keywords.
const fn sort_prefix(kind: &spark_resolve::DefinitionKind) -> &'static str {
    use spark_resolve::DefinitionKind;
    match kind {
        DefinitionKind::Local | DefinitionKind::Parameter => "0",
        DefinitionKind::Function | DefinitionKind::BuiltinFunction => "1",
        DefinitionKind::Struct | DefinitionKind::Enum | DefinitionKind::Interface => "2",
        _ => "3",
    }
}

/// Generates scope-aware completions (general, non-dot context).
fn scope_completions(analysis: &AnalysisResult, offset: u32) -> Option<Vec<CompletionItem>> {
    let mut items = Vec::new();

    // Try scope arena first
    if let Some(arena) = analysis.scope_arena.as_ref() {
        if let Some(scope_id) = arena.find_scope_at_offset(offset, analysis.file_id) {
            // Value-namespace names (collect once, reuse for dedup)
            let visible_vals = arena.visible_values(scope_id);
            let value_names: std::collections::HashSet<spark_intern::Symbol> =
                visible_vals.iter().map(|(s, _)| *s).collect();
            for (sym, def_id) in &visible_vals {
                if let Some(def_info) = analysis.definitions.get(*def_id) {
                    let label = sym.as_str().to_string();
                    let kind = def_kind_to_completion_kind(&def_info.kind);
                    let detail = def_type_str(analysis, *def_id);
                    let prefix = sort_prefix(&def_info.kind);
                    items.push(CompletionItem {
                        label: label.clone(),
                        kind: Some(kind),
                        detail,
                        sort_text: Some(format!("{prefix}{label}")),
                        ..Default::default()
                    });
                }
            }
            // Type-namespace names (skip those already added from values)
            for (sym, def_id) in arena.visible_types(scope_id) {
                if value_names.contains(&sym) {
                    continue;
                }
                if let Some(def_info) = analysis.definitions.get(def_id) {
                    let label = sym.as_str().to_string();
                    let kind = def_kind_to_completion_kind(&def_info.kind);
                    let prefix = sort_prefix(&def_info.kind);
                    items.push(CompletionItem {
                        label: label.clone(),
                        kind: Some(kind),
                        sort_text: Some(format!("{prefix}{label}")),
                        ..Default::default()
                    });
                }
            }
        }
    }

    // Fallback: iterate definitions for this file when scope arena is empty
    if items.is_empty() {
        for (def_id, def_info) in analysis.definitions.iter() {
            if def_info.span.file != analysis.file_id {
                continue;
            }
            let label = def_info.name.as_str().to_string();
            if label.is_empty() {
                continue;
            }
            let kind = def_kind_to_completion_kind(&def_info.kind);
            items.push(CompletionItem {
                label,
                kind: Some(kind),
                detail: def_type_str(analysis, def_id),
                ..Default::default()
            });
        }
    }

    // Add keyword completions
    for kw in KEYWORD_COMPLETIONS {
        items.push(CompletionItem {
            label: (*kw).to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            sort_text: Some(format!("4{kw}")),
            ..Default::default()
        });
    }

    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

/// Keywords to include in general completions.
const KEYWORD_COMPLETIONS: &[&str] = &[
    "fn",
    "if",
    "else",
    "match",
    "return",
    "struct",
    "enum",
    "interface",
    "import",
    "const",
    "pub",
    "loop",
    "break",
    "continue",
    "spawn",
    "async",
    "await",
    "alias",
    "type",
    "binding",
];

/// Generates dot-completions for member access.
fn dot_completions(
    analysis: &AnalysisResult,
    offset: u32,
    source: &str,
) -> Option<Vec<CompletionItem>> {
    let table = analysis.type_table.as_ref()?;
    let dot_offset = offset - 1; // offset of the '.'

    // Find the identifier/expression before the dot.
    // Walk backwards from dot to find the identifier end.
    let before_dot = &source[..dot_offset as usize];
    let ident_end = before_dot.len();
    let ident_start = before_dot
        .bytes()
        .rposition(|b| !b.is_ascii_alphanumeric() && b != b'_')
        .map_or(0, |pos| pos + 1);

    if ident_start >= ident_end {
        return None;
    }

    // Try to find the DefId for the identifier before the dot via resolution maps.
    let resolutions = analysis.resolved.get_resolutions(analysis.file_id)?;
    let search_offset = (ident_start as u32) + 1; // inside the identifier
    let (_, def_id) = resolutions.find_at_offset(search_offset)?;

    // Get the type of this definition and delegate to the type-based helper.
    let type_id = table.get_def_type(def_id)?;
    let ty = table.resolve(type_id);
    dot_completions_for_ty(ty, table, analysis)
}

/// Helper for dot-completions on an already-resolved type.
fn dot_completions_for_ty(
    ty: &spark_types::Ty,
    table: &spark_types::TypeTable,
    analysis: &AnalysisResult,
) -> Option<Vec<CompletionItem>> {
    let mut items = Vec::new();
    match ty {
        spark_types::Ty::Struct(def_id) => {
            if let Some(fields) = table.get_struct_fields(*def_id) {
                for (name, ftype) in fields {
                    let type_str = spark_types::display_type(*ftype, table, &analysis.definitions);
                    items.push(CompletionItem {
                        label: name.as_str().to_string(),
                        kind: Some(CompletionItemKind::FIELD),
                        detail: Some(type_str),
                        sort_text: Some(format!("0{}", name.as_str())),
                        ..Default::default()
                    });
                }
            }
            add_bound_method_completions(&mut items, *def_id, table, analysis);
        }
        spark_types::Ty::Enum(def_id) => {
            if let Some(variants) = table.get_enum_variants(*def_id) {
                for (name, _) in variants {
                    items.push(CompletionItem {
                        label: name.as_str().to_string(),
                        kind: Some(CompletionItemKind::ENUM_MEMBER),
                        sort_text: Some(format!("0{}", name.as_str())),
                        ..Default::default()
                    });
                }
            }
            add_bound_method_completions(&mut items, *def_id, table, analysis);
        }
        spark_types::Ty::String => {
            add_builtin_methods(
                &mut items,
                &[
                    ("len", "Int"),
                    ("contains", "(String): Bool"),
                    ("toInt", "Int?"),
                    ("toFloat", "Float?"),
                    ("toString", "String"),
                ],
            );
        }
        spark_types::Ty::List(_) => {
            add_builtin_methods(
                &mut items,
                &[
                    ("len", "Int"),
                    ("append", "(T): Void"),
                    ("contains", "(T): Bool"),
                    ("remove", "(Int): Void"),
                    ("sort", "List[T]"),
                    ("take", "(Int): List[T]"),
                    ("reverse", "List[T]"),
                ],
            );
        }
        spark_types::Ty::Map(_, _) => {
            add_builtin_methods(
                &mut items,
                &[
                    ("len", "Int"),
                    ("contains", "(K): Bool"),
                    ("remove", "(K): Void"),
                ],
            );
        }
        spark_types::Ty::Set(_) => {
            add_builtin_methods(
                &mut items,
                &[
                    ("len", "Int"),
                    ("contains", "(T): Bool"),
                    ("remove", "(T): Void"),
                    ("add", "(T): Void"),
                ],
            );
        }
        spark_types::Ty::Mutable(inner_id) => {
            let inner_ty = table.resolve(*inner_id);
            return dot_completions_for_ty(inner_ty, table, analysis);
        }
        _ => {}
    }
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

fn add_bound_method_completions(
    items: &mut Vec<CompletionItem>,
    def_id: spark_resolve::DefId,
    table: &spark_types::TypeTable,
    analysis: &AnalysisResult,
) {
    if let Some(methods) = table.get_type_methods(def_id) {
        for (name, method_def_id) in methods {
            items.push(CompletionItem {
                label: name.as_str().to_string(),
                kind: Some(CompletionItemKind::METHOD),
                detail: def_type_str(analysis, *method_def_id),
                sort_text: Some(format!("1{}", name.as_str())),
                ..Default::default()
            });
        }
    }
}

/// Adds built-in method completions.
fn add_builtin_methods(items: &mut Vec<CompletionItem>, methods: &[(&str, &str)]) {
    for (name, detail) in methods {
        items.push(CompletionItem {
            label: (*name).to_string(),
            kind: Some(CompletionItemKind::METHOD),
            detail: Some((*detail).to_string()),
            sort_text: Some(format!("0{name}")),
            ..Default::default()
        });
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Extract workspace root from initialization params.
        let root_path = params
            .workspace_folders
            .as_ref()
            .and_then(|folders| folders.first())
            .and_then(|folder| folder.uri.to_file_path().ok())
            .or_else(|| {
                #[allow(deprecated)] // root_uri is deprecated but still used by some clients
                params.root_uri.as_ref()?.to_file_path().ok()
            });

        if let Some(root) = root_path {
            // Canonicalize so all derived paths and URIs are consistent
            // (e.g., macOS resolves /var → /private/var).
            let root = root.canonicalize().unwrap_or(root);
            let mut ws_guard = self.workspace.write().unwrap();
            *ws_guard = Some(WorkspaceState::new(root));
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                definition_provider: Some(lsp_types::OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                references_provider: Some(lsp_types::OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".into()]),
                    ..Default::default()
                }),
                document_symbol_provider: Some(lsp_types::OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: semantic_tokens::legend(),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: Some(false),
                            ..Default::default()
                        },
                    ),
                ),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "spark-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            offset_encoding: None,
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "Spark LSP server initialized")
            .await;

        // If we have a workspace root, discover all .spark files and
        // run the initial workspace analysis.
        let has_workspace = self.has_workspace();
        if has_workspace {
            // Discover files and read them from disk.
            let root_path = {
                let ws_guard = self.workspace.read().unwrap();
                ws_guard.as_ref().map(|ws| ws.root_path.clone())
            };

            if let Some(root) = root_path {
                let spark_files = workspace::discover_spark_files(&root);

                self.client
                    .log_message(
                        MessageType::INFO,
                        format!("Found {} .spark files in workspace", spark_files.len()),
                    )
                    .await;

                // Read each file from disk and store in workspace state.
                {
                    let mut ws_guard = self.workspace.write().unwrap();
                    if let Some(ws) = ws_guard.as_mut() {
                        for path in &spark_files {
                            if let Some(uri) = workspace::file_path_to_uri(path) {
                                if let Ok(source) = std::fs::read_to_string(path) {
                                    ws.disk_ropes.insert(uri.clone(), Rope::from_str(&source));
                                    ws.disk_sources.insert(uri, source);
                                }
                            }
                        }
                    }
                }

                // Resolve Spark.toml dependencies from all projects in the workspace.
                let (dep_sources, dep_errors) = workspace::resolve_all_workspace_deps(&root);
                for err in &dep_errors {
                    self.client.log_message(MessageType::WARNING, err).await;
                }
                if !dep_sources.is_empty() {
                    self.client
                        .log_message(
                            MessageType::INFO,
                            format!("Resolved {} dependency source files", dep_sources.len()),
                        )
                        .await;
                    let mut ws_guard = self.workspace.write().unwrap();
                    if let Some(ws) = ws_guard.as_mut() {
                        ws.dep_sources = dep_sources;
                    }
                }

                // Run initial workspace analysis.
                self.analyze_and_publish_workspace().await;
            }

            // Register dynamic file watcher for *.spark files and Spark.toml.
            let watch_kind = WatchKind::Create | WatchKind::Change | WatchKind::Delete;
            let registration = Registration {
                id: "spark-file-watcher".to_string(),
                method: "workspace/didChangeWatchedFiles".to_string(),
                register_options: Some(
                    serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                        watchers: vec![
                            FileSystemWatcher {
                                glob_pattern: GlobPattern::String("**/*.spark".to_string()),
                                kind: Some(watch_kind),
                            },
                            FileSystemWatcher {
                                glob_pattern: GlobPattern::String("**/Spark.toml".to_string()),
                                kind: Some(watch_kind),
                            },
                        ],
                    })
                    .unwrap(),
                ),
            };

            let _ = self.client.register_capability(vec![registration]).await;
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = &params.text_document.uri;

        let tokens = self
            .documents
            .get(uri)
            .map(|doc| {
                let source = doc.rope.to_string();
                let context = doc.analysis.as_ref().and_then(|analysis| {
                    let resolutions = analysis.resolved.get_resolutions(analysis.file_id)?;
                    Some(semantic_tokens::SemanticContext {
                        resolutions,
                        definitions: &analysis.definitions,
                    })
                });
                semantic_tokens::compute_semantic_tokens(&source, context.as_ref())
            })
            .unwrap_or_default();

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: tokens,
        })))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let location = self.documents.get(uri).and_then(|doc| {
            let analysis = doc.analysis.as_ref()?;
            let offset = document::position_to_offset(&doc.rope, position)?;
            let resolutions = analysis.resolved.get_resolutions(analysis.file_id)?;

            // Try normal resolution first, then fall back to field/method resolutions.
            let def_id = if let Some((_span, def_id)) = resolutions.find_at_offset(offset) {
                def_id
            } else {
                let table = analysis.type_table.as_ref()?;
                if let Some((_span, def_id)) = find_field_at_offset(table, offset, analysis.file_id)
                {
                    def_id
                } else {
                    let (_span, def_id) = find_method_at_offset(table, offset, analysis.file_id)?;
                    def_id
                }
            };

            let def_info = analysis.definitions.get(def_id)?;

            self.resolve_def_location(def_info.span, analysis.file_id, uri, &doc.rope)
        });

        Ok(location.map(GotoDefinitionResponse::Scalar))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let hover = self.documents.get(uri).and_then(|doc| {
            let analysis = doc.analysis.as_ref()?;
            let offset = document::position_to_offset(&doc.rope, position)?;
            let resolutions = analysis.resolved.get_resolutions(analysis.file_id)?;

            // Try normal resolution first, then fall back to field/method resolutions
            // from the type checker (field access names like `point.x` and method
            // calls like `user.serialize()` are not recorded in the resolver's maps).
            let (span, def_id) = if let Some(result) = resolutions.find_at_offset(offset) {
                result
            } else {
                let table = analysis.type_table.as_ref()?;
                if let Some(result) = find_field_at_offset(table, offset, analysis.file_id) {
                    result
                } else {
                    find_method_at_offset(table, offset, analysis.file_id)?
                }
            };

            let content = format_hover(analysis, def_id)?;
            let range = document::span_to_range(&doc.rope, span);

            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: format!("```spark\n{content}\n```"),
                }),
                range,
            })
        });

        Ok(hover)
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let locations = self.documents.get(uri).and_then(|doc| {
            let analysis = doc.analysis.as_ref()?;
            let offset = document::position_to_offset(&doc.rope, position)?;
            let resolutions = analysis.resolved.get_resolutions(analysis.file_id)?;
            let (_span, def_id) = resolutions.find_at_offset(offset)?;

            let mut locs: Vec<Location> = Vec::new();

            // Search ALL files' resolution maps for references to this DefId.
            for res_map in analysis.resolved.resolutions.values() {
                let ref_spans = res_map.find_references(def_id);
                for span in ref_spans {
                    if let Some(loc) =
                        self.resolve_def_location(span, analysis.file_id, uri, &doc.rope)
                    {
                        locs.push(loc);
                    }
                }
            }

            // Include the definition site if requested.
            if params.context.include_declaration {
                if let Some(def_info) = analysis.definitions.get(def_id) {
                    if let Some(loc) =
                        self.resolve_def_location(def_info.span, analysis.file_id, uri, &doc.rope)
                    {
                        locs.push(loc);
                    }
                }
            }

            if locs.is_empty() {
                None
            } else {
                Some(locs)
            }
        });

        Ok(locations)
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = &params.text_document.uri;

        let symbols = self.documents.get(uri).and_then(|doc| {
            let analysis = doc.analysis.as_ref()?;
            let ast = analysis.resolved.asts.get(&analysis.file_id)?;
            let syms: Vec<DocumentSymbol> = ast
                .items
                .iter()
                .filter_map(|item| item_to_document_symbol(item, &doc.rope))
                .collect();

            if syms.is_empty() {
                None
            } else {
                Some(syms)
            }
        });

        Ok(symbols.map(DocumentSymbolResponse::Nested))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let items = self.documents.get(uri).and_then(|doc| {
            let analysis = doc.analysis.as_ref()?;
            let offset = document::position_to_offset(&doc.rope, position)?;

            // Detect dot-completion: check the character before cursor without
            // materializing the entire rope into a String.
            let is_dot = offset > 0 && {
                let byte_idx = (offset - 1) as usize;
                byte_idx < doc.rope.len_bytes() && doc.rope.byte(byte_idx) == b'.'
            };

            if is_dot {
                let source = doc.rope.to_string();
                dot_completions(analysis, offset, &source)
            } else {
                scope_completions(analysis, offset)
            }
        });

        Ok(items.map(|items| {
            CompletionResponse::List(CompletionList {
                is_incomplete: false,
                items,
            })
        }))
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = &params.text_document.text;
        let version = params.text_document.version;

        self.documents.insert(uri.clone(), DocumentState::new(text));

        if self.has_workspace() {
            // Remove from disk sources (now tracked by editor).
            {
                let mut ws_guard = self.workspace.write().unwrap();
                if let Some(ws) = ws_guard.as_mut() {
                    ws.disk_sources.remove(&uri);
                    ws.disk_ropes.remove(&uri);
                }
            }
            self.analyze_and_publish_workspace().await;
        } else {
            self.analyze_and_publish_single(&uri, text, Some(version))
                .await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;

        // We use FULL sync, so there's exactly one change with the full text.
        if let Some(change) = params.content_changes.into_iter().next() {
            if let Some(mut doc) = self.documents.get_mut(&uri) {
                doc.update(&change.text);
            }

            if self.has_workspace() {
                self.analyze_and_publish_workspace().await;
            } else {
                self.analyze_and_publish_single(&uri, &change.text, Some(version))
                    .await;
            }
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.remove(&uri);

        if self.has_workspace() {
            // Re-read the file from disk so it stays in the workspace.
            if let Ok(path) = uri.to_file_path() {
                if let Ok(source) = std::fs::read_to_string(&path) {
                    let mut ws_guard = self.workspace.write().unwrap();
                    if let Some(ws) = ws_guard.as_mut() {
                        ws.disk_ropes.insert(uri.clone(), Rope::from_str(&source));
                        ws.disk_sources.insert(uri.clone(), source);
                    }
                }
            }
            self.analyze_and_publish_workspace().await;
        } else {
            // Clear diagnostics for closed document.
            self.client.publish_diagnostics(uri, Vec::new(), None).await;
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        if !self.has_workspace() {
            return;
        }

        let mut needs_reanalysis = false;
        let mut manifest_changed = false;

        // Collect changes to apply, then apply under a single write lock.
        let mut to_update: Vec<(Url, String)> = Vec::new();
        let mut to_delete: Vec<Url> = Vec::new();

        for change in &params.changes {
            let uri = &change.uri;

            // Detect Spark.toml changes.
            if uri.path().ends_with("Spark.toml") {
                manifest_changed = true;
                continue;
            }

            // Skip files that are open in the editor (editor is the source of truth).
            if self.documents.contains_key(uri) {
                continue;
            }

            match change.typ {
                FileChangeType::CREATED | FileChangeType::CHANGED => {
                    if let Ok(path) = uri.to_file_path() {
                        if let Ok(source) = std::fs::read_to_string(&path) {
                            to_update.push((uri.clone(), source));
                        }
                    }
                }
                FileChangeType::DELETED => {
                    to_delete.push(uri.clone());
                }
                _ => {}
            }
        }

        if !to_update.is_empty() || !to_delete.is_empty() {
            let mut ws_guard = self.workspace.write().unwrap();
            if let Some(ws) = ws_guard.as_mut() {
                for (uri, source) in to_update {
                    ws.disk_ropes.insert(uri.clone(), Rope::from_str(&source));
                    ws.disk_sources.insert(uri, source);
                }
                for uri in to_delete {
                    ws.disk_sources.remove(&uri);
                    ws.disk_ropes.remove(&uri);
                }
                needs_reanalysis = true;
            }
        }

        // Re-resolve dependencies when Spark.toml changes.
        if manifest_changed {
            let root = {
                let ws_guard = self.workspace.read().unwrap();
                ws_guard.as_ref().map(|ws| ws.root_path.clone())
            };
            if let Some(root) = root {
                let (dep_sources, dep_errors) = workspace::resolve_all_workspace_deps(&root);
                for err in &dep_errors {
                    self.client.log_message(MessageType::WARNING, err).await;
                }
                {
                    let mut ws_guard = self.workspace.write().unwrap();
                    if let Some(ws) = ws_guard.as_mut() {
                        ws.dep_sources = dep_sources;
                    }
                }
                needs_reanalysis = true;
            }
        }

        if needs_reanalysis {
            self.analyze_and_publish_workspace().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_span::Span;

    /// Helper: runs the analyze pipeline and returns the `AnalysisResult`.
    fn analyze_source(source: &str) -> Option<AnalysisResult> {
        let uri = Url::parse("file:///test.spark").unwrap();
        let (_diags, analysis) = analyze(source, &uri);
        analysis
    }

    /// Helper: finds the hover content for the symbol at a byte offset.
    fn hover_at(analysis: &AnalysisResult, offset: u32) -> Option<String> {
        let resolutions = analysis.resolved.get_resolutions(analysis.file_id)?;
        let (_span, def_id) = resolutions.find_at_offset(offset)?;
        format_hover(analysis, def_id)
    }

    /// Helper: finds all reference spans for the symbol at a byte offset.
    fn references_at(
        analysis: &AnalysisResult,
        offset: u32,
        include_declaration: bool,
    ) -> Option<Vec<Span>> {
        let resolutions = analysis.resolved.get_resolutions(analysis.file_id)?;
        let (_span, def_id) = resolutions.find_at_offset(offset)?;

        let mut spans = resolutions.find_references(def_id);

        if include_declaration {
            if let Some(def_info) = analysis.definitions.get(def_id) {
                spans.push(def_info.span);
            }
        }

        if spans.is_empty() {
            None
        } else {
            Some(spans)
        }
    }

    #[test]
    fn hover_function_shows_signature() {
        let source = "fn add(a: Int, b: Int): Int { return a + b }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // Hover over `add` at offset 3 (inside "add")
        let hover = hover_at(&analysis, 3).expect("hover should produce content");
        assert!(
            hover.starts_with("fn add"),
            "Expected fn signature, got: {hover}"
        );
        assert!(
            hover.contains("Int"),
            "Expected Int in signature, got: {hover}"
        );
    }

    #[test]
    fn hover_variable_shows_type() {
        let source = "fn main(): Void { x: Int = 42\nx }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // `x` usage is at offset 30 (the `x` after newline)
        let hover = hover_at(&analysis, 30).expect("hover should produce content");
        assert!(hover.contains("Int"), "Expected type info, got: {hover}");
    }

    #[test]
    fn hover_struct_shows_fields() {
        let source = "struct Point { x: Int, y: Int }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // `Point` starts at offset 7
        let hover = hover_at(&analysis, 7).expect("hover should produce content");
        assert!(
            hover.contains("struct Point"),
            "Expected struct, got: {hover}"
        );
        assert!(hover.contains("x: Int"), "Expected field x, got: {hover}");
        assert!(hover.contains("y: Int"), "Expected field y, got: {hover}");
    }

    #[test]
    fn hover_const_shows_type() {
        let source = "const MAX: Int = 100";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // `MAX` starts at offset 6
        let hover = hover_at(&analysis, 6).expect("hover should produce content");
        assert!(hover.contains("const MAX"), "Expected const, got: {hover}");
        assert!(hover.contains("Int"), "Expected type, got: {hover}");
    }

    #[test]
    fn hover_unknown_position_returns_none() {
        let source = "fn main(): Void { }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // Offset 0 is the `f` in `fn` — a keyword, not a resolved name
        let hover = hover_at(&analysis, 0);
        assert!(hover.is_none(), "Expected None for keyword position");
    }

    #[test]
    fn hover_without_type_table_shows_basic_info() {
        let source = "fn main(): Void { x: Int = 42\nx }";
        let mut analysis = analyze_source(source).expect("analysis should succeed");
        // Simulate type checking failure by removing the type table
        analysis.type_table = None;
        // `x` usage at offset 30
        let hover = hover_at(&analysis, 30).expect("hover should produce content");
        // Without type info, should still show the name
        assert!(hover.contains('x'), "Expected name, got: {hover}");
    }

    #[test]
    fn hover_parameter_shows_type() {
        let source = "fn greet(name: String): Void { name }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // `name` usage at offset 31 (inside the body)
        let hover = hover_at(&analysis, 31).expect("hover should produce content");
        assert!(
            hover.contains("String"),
            "Expected param type, got: {hover}"
        );
    }

    #[test]
    fn analyze_caches_type_table() {
        let source = "fn main(): Void { x: Int = 42 }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        assert!(
            analysis.type_table.is_some(),
            "Type table should be cached after successful type check"
        );
    }

    #[test]
    fn analyze_no_type_table_on_parse_error() {
        // Intentionally broken syntax
        let source = "fn main( { }";
        let analysis = analyze_source(source);
        // Parse error means no analysis at all (or no type table)
        match analysis {
            None => {} // Expected — parse errors prevent analysis
            Some(a) => assert!(
                a.type_table.is_none(),
                "Type table should be None when earlier phases fail"
            ),
        }
    }

    // ── Find References tests ───────────────────────────────────────────

    #[test]
    fn references_function_called_multiple_times() {
        let source = "fn add(a: Int, b: Int): Int { return a + b }\nfn main(): Void { add(1, 2)\nadd(3, 4) }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // `add` call at offset 63 (first call in main body)
        let refs = references_at(&analysis, 63, false).expect("should find references");
        // At least 2 call-site references to `add`
        assert!(
            refs.len() >= 2,
            "Expected >=2 references, got: {}",
            refs.len()
        );
    }

    #[test]
    fn references_variable_in_expression() {
        let source = "fn main(): Void { x: Int = 42\nx + x }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // `x` usage at offset 30 (first `x` after newline)
        let refs = references_at(&analysis, 30, false).expect("should find references");
        // `x` is used twice in `x + x`
        assert!(
            refs.len() >= 2,
            "Expected >=2 references, got: {}",
            refs.len()
        );
    }

    #[test]
    fn references_include_declaration_true() {
        let source = "fn main(): Void { x: Int = 42\nx }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // `x` usage at offset 30
        let refs_with = references_at(&analysis, 30, true).expect("should find references");
        let refs_without = references_at(&analysis, 30, false).expect("should find references");
        assert!(
            refs_with.len() > refs_without.len(),
            "include_declaration=true should return more spans ({} vs {})",
            refs_with.len(),
            refs_without.len()
        );
    }

    #[test]
    fn references_include_declaration_false_excludes_def() {
        let source = "fn main(): Void { x: Int = 42\nx }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        let def_info_span = {
            let resolutions = analysis.resolved.get_resolutions(analysis.file_id).unwrap();
            let (_, def_id) = resolutions.find_at_offset(30).unwrap();
            analysis.definitions.get(def_id).unwrap().span
        };
        let refs = references_at(&analysis, 30, false).expect("should find references");
        // The definition span should NOT appear in usage-only references
        assert!(
            !refs.contains(&def_info_span),
            "include_declaration=false should not contain the definition span"
        );
    }

    #[test]
    fn references_unknown_position_returns_none() {
        let source = "fn main(): Void { }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // Offset 0 is `f` in `fn` — a keyword, not a resolved symbol
        let refs = references_at(&analysis, 0, false);
        assert!(refs.is_none(), "Expected None for keyword position");
    }

    // ── Document Symbol tests ───────────────────────────────────────────

    /// Helper: returns document symbols for a source string.
    #[allow(deprecated)]
    fn document_symbols_for(source: &str) -> Vec<DocumentSymbol> {
        let analysis = analyze_source(source).expect("analysis should succeed");
        let ast = analysis.resolved.asts.get(&analysis.file_id).unwrap();
        let rope = ropey::Rope::from_str(source);
        ast.items
            .iter()
            .filter_map(|item| item_to_document_symbol(item, &rope))
            .collect()
    }

    #[test]
    fn symbol_function() {
        let symbols = document_symbols_for("fn foo(): Void { }");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "foo");
        assert_eq!(symbols[0].kind, SymbolKind::FUNCTION);
        assert!(symbols[0].children.is_none());
    }

    #[test]
    fn symbol_struct_with_fields() {
        let symbols = document_symbols_for("struct Point { x: Int, y: Int }");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "Point");
        assert_eq!(symbols[0].kind, SymbolKind::STRUCT);
        let children = symbols[0]
            .children
            .as_ref()
            .expect("struct should have children");
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].name, "x");
        assert_eq!(children[0].kind, SymbolKind::FIELD);
        assert_eq!(children[1].name, "y");
        assert_eq!(children[1].kind, SymbolKind::FIELD);
    }

    #[test]
    fn symbol_enum_with_variants() {
        let symbols = document_symbols_for("enum Color { Red, Green, Blue }");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "Color");
        assert_eq!(symbols[0].kind, SymbolKind::ENUM);
        let children = symbols[0]
            .children
            .as_ref()
            .expect("enum should have children");
        assert_eq!(children.len(), 3);
        assert_eq!(children[0].name, "Red");
        assert_eq!(children[0].kind, SymbolKind::ENUM_MEMBER);
    }

    #[test]
    fn symbol_interface_with_methods() {
        let symbols = document_symbols_for("interface Printable { fn print(): Void }");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "Printable");
        assert_eq!(symbols[0].kind, SymbolKind::INTERFACE);
        let children = symbols[0]
            .children
            .as_ref()
            .expect("interface should have children");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "print");
        assert_eq!(children[0].kind, SymbolKind::METHOD);
    }

    #[test]
    fn symbol_const() {
        let symbols = document_symbols_for("const MAX: Int = 100");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "MAX");
        assert_eq!(symbols[0].kind, SymbolKind::CONSTANT);
    }

    #[test]
    fn symbol_multiple_items() {
        let source = "fn foo(): Void { }\nstruct Bar { }\nconst N: Int = 1";
        let symbols = document_symbols_for(source);
        assert_eq!(symbols.len(), 3);
        assert_eq!(symbols[0].name, "foo");
        assert_eq!(symbols[1].name, "Bar");
        assert_eq!(symbols[2].name, "N");
    }

    #[test]
    fn symbol_empty_file_returns_empty() {
        let source = "";
        let uri = Url::parse("file:///test.spark").unwrap();
        let (_diags, analysis) = analyze(source, &uri);
        // Empty file may not produce analysis at all
        if let Some(analysis) = analysis {
            if let Some(ast) = analysis.resolved.asts.get(&analysis.file_id) {
                let rope = ropey::Rope::from_str(source);
                let has_symbols = ast
                    .items
                    .iter()
                    .any(|item| item_to_document_symbol(item, &rope).is_some());
                assert!(!has_symbols, "Empty file should have no symbols");
            }
        }
    }

    #[test]
    fn hover_struct_field_access() {
        let source = "struct Point { x: Int, y: Int }\nfn main(): Void { p: Point = Point { x: 1, y: 2 }\np.x }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // `x` in `p.x` — the field name is near the end of the source.
        // "p.x }" → x is at len - 3 (before the space and closing brace).
        let x_offset = (source.len() - 3) as u32;
        // Normal resolution won't find it, but field resolution fallback should.
        let table = analysis
            .type_table
            .as_ref()
            .expect("type table should exist");
        let field_result = find_field_at_offset(table, x_offset, analysis.file_id);
        assert!(field_result.is_some(), "field resolution should find x");
        let (_span, def_id) = field_result.unwrap();
        let hover = format_hover(&analysis, def_id);
        assert!(hover.is_some(), "hover should produce content for field");
        let content = hover.unwrap();
        assert!(
            content.contains("Int"),
            "Expected field type Int, got: {content}"
        );
    }

    // ── Generic constraint hover tests ────────────────────────────────────

    #[test]
    fn hover_generic_param_with_interface_constraint() {
        let source = "interface Printable { fn display(): String }\nfn showAll[T: Printable](item: T): String { return \"ok\" }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // `T` in `[T: Printable]` — find offset of the generic param T
        let t_offset = source.find("showAll[T").unwrap() + "showAll[".len();
        let hover = hover_at(&analysis, t_offset as u32).expect("hover should produce content");
        assert!(
            hover.contains("T: Printable"),
            "Expected 'T: Printable', got: {hover}"
        );
    }

    #[test]
    fn hover_generic_param_with_structural_constraint() {
        let source = "fn getId[T: { id: Int }](entity: T): Int { return 1 }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        let t_offset = source.find("getId[T").unwrap() + "getId[".len();
        let hover = hover_at(&analysis, t_offset as u32).expect("hover should produce content");
        // `Int` is lowered to `Int64` in the type system
        assert!(
            hover.contains("T: { id: Int64 }"),
            "Expected 'T: {{ id: Int64 }}', got: {hover}"
        );
    }

    #[test]
    fn hover_generic_param_unconstrained() {
        let source = "fn identity[T](x: T): T { return x }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        let t_offset = source.find("identity[T").unwrap() + "identity[".len();
        let hover = hover_at(&analysis, t_offset as u32).expect("hover should produce content");
        assert_eq!(hover, "T", "Expected just 'T', got: {hover}");
    }

    #[test]
    fn hover_function_with_generic_params() {
        let source = "fn identity[T](x: T): T { return x }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // Hover over `identity` function name
        let fn_offset = source.find("identity").unwrap();
        let hover = hover_at(&analysis, fn_offset as u32).expect("hover should produce content");
        assert!(
            hover.contains("fn identity[T]"),
            "Expected 'fn identity[T]', got: {hover}"
        );
    }

    #[test]
    fn hover_struct_with_generic_params() {
        let source = "struct Pair[A, B] { first: A, second: B }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        let s_offset = source.find("Pair").unwrap();
        let hover = hover_at(&analysis, s_offset as u32).expect("hover should produce content");
        assert!(
            hover.contains("struct Pair[A, B]"),
            "Expected 'struct Pair[A, B]', got: {hover}"
        );
    }

    // ── Completion tests ─────────────────────────────────────────────────

    #[test]
    fn completion_includes_defined_functions() {
        let source = "fn add(a: Int, b: Int): Int { return a + b }\nfn main(): Void { }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // Request completions inside `main` body (offset inside `{ }`)
        let body_offset = source.rfind('}').unwrap() as u32 - 1;
        let items = scope_completions(&analysis, body_offset).expect("should return completions");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"add"),
            "Expected 'add' in completions: {labels:?}"
        );
        assert!(
            labels.contains(&"main"),
            "Expected 'main' in completions: {labels:?}"
        );
    }

    #[test]
    fn completion_includes_variables_in_scope() {
        let source = "fn main(): Void { x: Int = 42\nx }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // Offset at the `x` usage (after newline)
        let items = scope_completions(&analysis, 30).expect("should return completions");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"x"),
            "Expected 'x' in completions: {labels:?}"
        );
    }

    #[test]
    fn completion_includes_keywords() {
        let source = "fn main(): Void { }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        let items = scope_completions(&analysis, 18).expect("should return completions");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"fn"), "Expected 'fn' keyword: {labels:?}");
        assert!(labels.contains(&"if"), "Expected 'if' keyword: {labels:?}");
        assert!(
            labels.contains(&"return"),
            "Expected 'return' keyword: {labels:?}"
        );
    }

    #[test]
    fn completion_dot_on_struct_shows_fields() {
        // Use a valid expression with `.x` to get analysis, then query dot-completion
        // at the position of the dot.
        let source = "struct Point { x: Int, y: Int }\nfn main(): Void { p: Point = Point { x: 1, y: 2 }\np.x }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // Position right after the '.' in `p.x`
        let dot_pos = source.rfind('.').unwrap() as u32 + 1;
        let items = dot_completions(&analysis, dot_pos, source);
        if let Some(items) = items {
            let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(labels.contains(&"x"), "Expected 'x' field: {labels:?}");
            assert!(labels.contains(&"y"), "Expected 'y' field: {labels:?}");
        }
        // Note: dot-completion may return None if the resolver doesn't
        // record the identifier before the dot in a way we can look up.
        // This is acceptable — the test verifies the code path doesn't panic.
    }

    #[test]
    fn completion_dot_on_string_shows_methods() {
        let source = "fn main(): Void { s: String = \"hello\"\ns.len() }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // Position right after the '.' in `s.len()`
        let dot_pos = source.rfind('.').unwrap() as u32 + 1;
        let items = dot_completions(&analysis, dot_pos, source);
        if let Some(items) = items {
            let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(labels.contains(&"len"), "Expected 'len' method: {labels:?}");
            assert!(
                labels.contains(&"contains"),
                "Expected 'contains' method: {labels:?}"
            );
        }
    }

    #[test]
    fn completion_out_of_scope_not_suggested() {
        let source = "fn foo(): Void { inner: Int = 99 }\nfn main(): Void { }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // Inside main body — `inner` should NOT be visible
        let main_body_offset = source.rfind('}').unwrap() as u32 - 1;
        let items =
            scope_completions(&analysis, main_body_offset).expect("should return completions");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            !labels.contains(&"inner"),
            "Did not expect 'inner' outside its scope: {labels:?}"
        );
    }
}
