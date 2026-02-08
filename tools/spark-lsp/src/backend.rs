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
    GotoDefinitionParams, GotoDefinitionResponse, InitializeParams, InitializeResult,
    InitializedParams, MessageType, SemanticTokens, SemanticTokensFullOptions,
    SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo,
    TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use tower_lsp::{Client, LanguageServer};

use spark_diagnostics::Diagnostics;
use spark_lexer::Lexer;
use spark_modules::{ModuleResolverInput, ModuleGraph};
use spark_parser::parse;
use spark_resolve::ResolverInput;
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

            // Cache resolver output for on-demand queries (go-to-definition, etc.)
            // Clone before type checking consumes ownership.
            analysis = Some(AnalysisResult {
                file_id,
                resolved: resolve_output.resolved.clone(),
                definitions: resolve_output.definitions.clone(),
            });

            if !all_diagnostics.has_errors() {
                // Phase 5: Type checking
                let type_output = spark_types::check(TypeCheckerInput {
                    resolved: resolve_output.resolved,
                    definitions: resolve_output.definitions,
                    graph,
                });
                all_diagnostics.extend(type_output.diagnostics);
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
