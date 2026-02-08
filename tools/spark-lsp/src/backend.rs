//! LSP backend — implements the `LanguageServer` trait.
//!
//! Provides real-time diagnostics by running the compiler's frontend
//! pipeline (lex → parse → resolve → type-check) on every document
//! change, and lexer-driven semantic token highlighting.

use std::collections::HashMap;
use std::path::PathBuf;

use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    self, DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams,
    HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams, Location,
    MarkupContent, MarkupKind, MessageType, ReferenceParams, SemanticTokens,
    SemanticTokensFullOptions, SemanticTokensOptions, SemanticTokensParams,
    SemanticTokensResult, SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo,
    TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use tower_lsp::{Client, LanguageServer};

use spark_diagnostics::Diagnostics;
use spark_lexer::Lexer;
use spark_modules::{ModuleResolverInput, ModuleGraph};
use spark_parser::parse;
use spark_resolve::{DefinitionKind, ResolverInput};
use spark_span::{FileId, SourceMap};
use spark_types::TypeCheckerInput;

use crate::document::{self, AnalysisResult, DocumentState};
use crate::semantic_tokens;

/// The LSP server backend.
///
/// Holds a reference to the LSP client (for sending notifications) and
/// a concurrent map of open documents.
pub struct Backend {
    /// The LSP client connection.
    client: Client,
    /// Open documents, keyed by URI.
    documents: DashMap<Url, DocumentState>,
}

impl Backend {
    /// Creates a new backend with the given client connection.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: DashMap::new(),
        }
    }

    /// Runs the compiler frontend on a document, caches analysis results,
    /// and publishes diagnostics.
    async fn analyze_and_publish(&self, uri: &Url, source: &str, version: Option<i32>) {
        let (lsp_diagnostics, analysis) = analyze(source, uri);

        // Cache analysis result for on-demand queries (go-to-definition, etc.)
        if let Some(mut doc) = self.documents.get_mut(uri) {
            doc.analysis = analysis;
        }

        self.client
            .publish_diagnostics(uri.clone(), lsp_diagnostics, version)
            .await;
    }
}

/// Runs the compiler frontend pipeline and returns LSP diagnostics
/// plus an optional analysis result for on-demand queries.
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

            // Cache resolver output for on-demand queries (go-to-definition, hover, etc.)
            // Clone before type checking consumes ownership.
            analysis = Some(AnalysisResult {
                file_id,
                resolved: resolve_output.resolved.clone(),
                definitions: resolve_output.definitions.clone(),
                type_table: None,
            });

            if !all_diagnostics.has_errors() {
                // Phase 5: Type checking
                let type_output = spark_types::check(TypeCheckerInput {
                    resolved: resolve_output.resolved,
                    definitions: resolve_output.definitions,
                    graph,
                });
                all_diagnostics.extend(type_output.diagnostics);

                // Cache the type table for hover information
                if let Some(ref mut a) = analysis {
                    a.type_table = Some(type_output.table);
                }
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

/// Returns the type name for a definition, if type info is available.
fn def_type_str(analysis: &AnalysisResult, def_id: spark_resolve::DefId) -> Option<String> {
    let table = analysis.type_table.as_ref()?;
    let type_id = table.get_def_type(def_id)?;
    Some(spark_types::type_name(type_id, table, &analysis.definitions))
}

/// Formats hover content for a definition.
///
/// Uses the type table (if available) to show typed signatures. Falls back
/// to showing just the definition kind and name when type info is missing.
fn format_hover(analysis: &AnalysisResult, def_id: spark_resolve::DefId) -> Option<String> {
    let def_info = analysis.definitions.get(def_id)?;
    let name = def_info.name.as_str();

    let content = match &def_info.kind {
        DefinitionKind::Function => def_type_str(analysis, def_id)
            .map_or_else(|| format!("fn {name}(...)"), |ty| format!("fn {name}{ty}")),
        DefinitionKind::Struct => format_struct_hover(analysis, def_id, name),
        DefinitionKind::Enum => format_enum_hover(analysis, def_id, name),
        DefinitionKind::Local | DefinitionKind::Parameter => def_type_str(analysis, def_id)
            .map_or_else(|| name.to_string(), |ty| format!("{name}: {ty}")),
        DefinitionKind::Const => def_type_str(analysis, def_id)
            .map_or_else(|| format!("const {name}"), |ty| format!("const {name}: {ty}")),
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
        DefinitionKind::Interface => format!("interface {name}"),
        DefinitionKind::GenericParam => name.to_string(),
        DefinitionKind::LifetimeParam => format!("'{name}"),
        DefinitionKind::BuiltinFunction => format!("fn {name}(...)"),
    };

    Some(content)
}

/// Formats hover for a struct, showing fields when type info is available.
fn format_struct_hover(analysis: &AnalysisResult, def_id: spark_resolve::DefId, name: &str) -> String {
    let fields = analysis
        .type_table
        .as_ref()
        .and_then(|table| {
            table.get_struct_fields(def_id).map(|fields| {
                fields
                    .iter()
                    .map(|(sym, tid)| {
                        let ty = spark_types::type_name(*tid, table, &analysis.definitions);
                        format!("    {sym}: {ty}")
                    })
                    .collect::<Vec<_>>()
            })
        });
    match fields {
        Some(f) if f.is_empty() => format!("struct {name} {{ }}"),
        Some(f) => format!("struct {name} {{\n{}\n}}", f.join(",\n")),
        None => format!("struct {name}"),
    }
}

/// Formats hover for an enum, showing variants when type info is available.
fn format_enum_hover(analysis: &AnalysisResult, def_id: spark_resolve::DefId, name: &str) -> String {
    let variants = analysis
        .type_table
        .as_ref()
        .and_then(|table| {
            table.get_enum_variants(def_id).map(|variants| {
                variants
                    .iter()
                    .map(|(sym, _)| format!("    {sym}"))
                    .collect::<Vec<_>>()
            })
        });
    match variants {
        Some(v) if v.is_empty() => format!("enum {name} {{ }}"),
        Some(v) => format!("enum {name} {{\n{}\n}}", v.join(",\n")),
        None => format!("enum {name}"),
    }
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

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                definition_provider: Some(lsp_types::OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                references_provider: Some(lsp_types::OneOf::Left(true)),
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
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = &params.text_document.uri;
        let source = self
            .documents
            .get(uri)
            .map(|doc| doc.rope.to_string());

        let tokens = source
            .as_deref()
            .map(semantic_tokens::compute_semantic_tokens)
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
            let (_span, def_id) = resolutions.find_at_offset(offset)?;
            let def_info = analysis.definitions.get(def_id)?;
            let range = document::span_to_range(&doc.rope, def_info.span)?;
            Some(lsp_types::Location {
                uri: uri.clone(),
                range,
            })
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
            let (span, def_id) = resolutions.find_at_offset(offset)?;
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

            let mut ref_spans = resolutions.find_references(def_id);

            // Include the definition site if requested
            if params.context.include_declaration {
                if let Some(def_info) = analysis.definitions.get(def_id) {
                    ref_spans.push(def_info.span);
                }
            }

            let locs: Vec<Location> = ref_spans
                .iter()
                .filter_map(|&span| {
                    let range = document::span_to_range(&doc.rope, span)?;
                    Some(Location {
                        uri: uri.clone(),
                        range,
                    })
                })
                .collect();

            if locs.is_empty() { None } else { Some(locs) }
        });

        Ok(locations)
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = &params.text_document.text;
        let version = params.text_document.version;

        self.documents
            .insert(uri.clone(), DocumentState::new(text));
        self.analyze_and_publish(&uri, text, Some(version)).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;

        // We use FULL sync, so there's exactly one change with the full text.
        if let Some(change) = params.content_changes.into_iter().next() {
            if let Some(mut doc) = self.documents.get_mut(&uri) {
                doc.update(&change.text);
            }
            self.analyze_and_publish(&uri, &change.text, Some(version))
                .await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.remove(&uri);
        // Clear diagnostics for closed document
        self.client
            .publish_diagnostics(uri, Vec::new(), None)
            .await;
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

        if spans.is_empty() { None } else { Some(spans) }
    }

    #[test]
    fn hover_function_shows_signature() {
        let source = "fn add(a: Int, b: Int): Int { return a + b }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // Hover over `add` at offset 3 (inside "add")
        let hover = hover_at(&analysis, 3).expect("hover should produce content");
        assert!(hover.starts_with("fn add"), "Expected fn signature, got: {hover}");
        assert!(hover.contains("Int"), "Expected Int in signature, got: {hover}");
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
        assert!(hover.contains("struct Point"), "Expected struct, got: {hover}");
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
        assert!(hover.contains("x"), "Expected name, got: {hover}");
    }

    #[test]
    fn hover_parameter_shows_type() {
        let source = "fn greet(name: String): Void { name }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // `name` usage at offset 31 (inside the body)
        let hover = hover_at(&analysis, 31).expect("hover should produce content");
        assert!(hover.contains("String"), "Expected param type, got: {hover}");
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
        assert!(refs.len() >= 2, "Expected >=2 references, got: {}", refs.len());
    }

    #[test]
    fn references_variable_in_expression() {
        let source = "fn main(): Void { x: Int = 42\nx + x }";
        let analysis = analyze_source(source).expect("analysis should succeed");
        // `x` usage at offset 30 (first `x` after newline)
        let refs = references_at(&analysis, 30, false).expect("should find references");
        // `x` is used twice in `x + x`
        assert!(refs.len() >= 2, "Expected >=2 references, got: {}", refs.len());
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
}
