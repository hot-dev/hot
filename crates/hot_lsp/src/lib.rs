// Hot Language Server Protocol implementation
// This is a simplified version that focuses on basic functionality

use ahash::{AHashMap, AHashSet};
use hot::lang::ast::Program;
use hot::lang::bytecode::NamespaceRegistry;
use hot::lang::compiler::Compiler;
use hot::lang::compiler::core_registry::CoreVariableRegistry;
use hot::lang::errors::{CompilerError, CompilerErrors};
use hot::lang::repl::{ReplConfig, ReplSession};
use hot::lang::runtime::resolution::UnifiedResolver;
use hot::val::Val;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tower_lsp::jsonrpc::Result as JsonRpcResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use url::Url;

/// Parameters for the hot/eval custom request
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalParams {
    /// The code to evaluate
    pub code: String,
    /// Optional namespace context (e.g., "::myapp::module")
    pub namespace: Option<String>,
    /// Optional file URI for context
    pub file_uri: Option<String>,
}

/// Result of the hot/eval custom request
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalResult {
    /// Whether the evaluation succeeded
    pub success: bool,
    /// The result value (as a string)
    pub result: String,
    /// Error message if evaluation failed
    pub error: Option<String>,
    /// The current namespace after evaluation
    pub namespace: String,
    /// Captured stdout output (from print/println calls)
    pub stdout: Option<String>,
}

pub struct Backend {
    client: Client,
    conf: std::sync::Arc<tokio::sync::Mutex<hot::val::Val>>,
    src_paths: std::sync::Arc<tokio::sync::Mutex<Vec<PathBuf>>>,
    docs: tokio::sync::Mutex<AHashMap<Url, String>>,
    symbol_index: std::sync::Arc<tokio::sync::Mutex<Option<SymbolIndex>>>,
    unified_resolver: std::sync::Arc<tokio::sync::Mutex<Option<UnifiedResolver>>>,
    program: std::sync::Arc<tokio::sync::Mutex<Option<Program>>>,
    /// REPL session for stateful evaluation
    repl_session: std::sync::Arc<tokio::sync::Mutex<Option<ReplSession>>>,
}

#[derive(Default, Clone)]
struct SymbolIndex {
    namespaces: Vec<String>,
    ns_to_vars: AHashMap<String, Vec<SymbolVar>>,
    /// Core variables (functions, types, constants) that are auto-imported everywhere
    core_vars: Vec<SymbolVar>,
}

#[derive(Clone)]
struct SymbolVar {
    name: String,
    kind: CompletionItemKind,
    detail: Option<String>,
    doc_markdown: Option<String>,
    ast_value: hot::lang::ast::Value,
    /// Source location of this symbol's definition (file, line, col)
    def_source: Option<hot::lang::ast::Source>,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(
        &self,
        _: InitializeParams,
    ) -> tower_lsp::jsonrpc::Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        will_save: Some(false),
                        will_save_wait_until: Some(false),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                    },
                )),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    trigger_characters: Some(vec![
                        ":".to_string(),
                        "/".to_string(),
                        ".".to_string(),
                        "{".to_string(), // For struct field completion
                    ]),
                    work_done_progress_options: Default::default(),
                    all_commit_characters: None,
                    completion_item: None,
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: Some(vec![")".to_string()]),
                    work_done_progress_options: Default::default(),
                }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "hot-lsp".to_string(),
                version: Some(env!("HOT_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let _ = self
            .client
            .log_message(MessageType::INFO, "hot-lsp initialized")
            .await;
    }

    async fn shutdown(&self) -> tower_lsp::jsonrpc::Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let text = params.text_document.text;

        if uri.path().ends_with(".hot") {
            {
                let mut docs = self.docs.lock().await;
                docs.insert(uri.clone(), text.clone());
            }

            // Compile and analyze the file
            let conf = self.conf.lock().await.clone();
            let src_paths = self.src_paths.lock().await.clone();
            let diags = self
                .analyze_and_cache(&conf, &src_paths, uri.clone(), text)
                .await;
            let _ = self.client.publish_diagnostics(uri, diags, None).await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        if uri.path().ends_with(".hot") {
            let text = params
                .content_changes
                .into_iter()
                .last()
                .map(|c| c.text)
                .unwrap_or_default();

            {
                let mut docs = self.docs.lock().await;
                if !text.is_empty() {
                    docs.insert(uri.clone(), text.clone());
                }
            }

            let conf = self.conf.lock().await.clone();
            let src_paths = self.src_paths.lock().await.clone();
            let diags = self
                .analyze_and_cache(&conf, &src_paths, uri.clone(), text)
                .await;
            let _ = self.client.publish_diagnostics(uri, diags, None).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        if uri.path().ends_with(".hot") {
            {
                let mut docs = self.docs.lock().await;
                docs.remove(&uri);
            }
            let _ = self.client.publish_diagnostics(uri, Vec::new(), None).await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        if uri.path().ends_with(".hot") {
            // On save, re-analyze the file to ensure diagnostics are up-to-date
            // Get text from our cache (already updated by did_change)
            let text = {
                let docs = self.docs.lock().await;
                docs.get(&uri).cloned()
            };

            if let Some(text) = text {
                let conf = self.conf.lock().await.clone();
                let src_paths = self.src_paths.lock().await.clone();
                let diags = self
                    .analyze_and_cache(&conf, &src_paths, uri.clone(), text)
                    .await;
                let _ = self.client.publish_diagnostics(uri, diags, None).await;
            }
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        // Handle file system changes (create, modify, delete)
        for change in params.changes {
            let uri = change.uri;
            if !uri.path().ends_with(".hot") {
                continue;
            }

            match change.typ {
                FileChangeType::CREATED | FileChangeType::CHANGED => {
                    // For files changed outside the editor, read from disk and analyze
                    // Skip if file is already open (we'll use the in-memory version)
                    let is_open = {
                        let docs = self.docs.lock().await;
                        docs.contains_key(&uri)
                    };

                    if !is_open
                        && let Ok(path) = uri.to_file_path()
                        && let Ok(text) = fs::read_to_string(&path)
                    {
                        let conf = self.conf.lock().await.clone();
                        let src_paths = self.src_paths.lock().await.clone();
                        let diags = self
                            .analyze_and_cache(&conf, &src_paths, uri.clone(), text)
                            .await;
                        let _ = self.client.publish_diagnostics(uri, diags, None).await;
                    }
                }
                FileChangeType::DELETED => {
                    // Clear diagnostics for deleted files
                    let _ = self
                        .client
                        .publish_diagnostics(uri.clone(), Vec::new(), None)
                        .await;

                    // Remove from docs cache if present
                    let mut docs = self.docs.lock().await;
                    docs.remove(&uri);

                    // Rebuild the symbol index to remove stale symbols
                    drop(docs);
                    self.build_symbol_index().await;
                }
                _ => {}
            }
        }
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> tower_lsp::jsonrpc::Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        // Get current line content from open docs
        let docs = self.docs.lock().await;
        let text = if let Some(t) = docs.get(&uri) {
            t.clone()
        } else {
            return Ok(None);
        };
        drop(docs);

        let line_text = if let Some(line) = text.lines().nth(position.line as usize) {
            line.to_string()
        } else {
            String::new()
        };
        let col = position.character as usize;
        let prefix = if col <= line_text.len() {
            &line_text[..col]
        } else {
            &line_text
        };

        // Check for namespace completion (::)
        if let Some(ns_ctx) = extract_namespace_context(prefix) {
            let symbol_index = self.symbol_index.lock().await;
            if let Some(index) = &*symbol_index {
                let items = complete_namespaces(index, &ns_ctx, position);
                return Ok(Some(CompletionResponse::Array(items)));
            }
        }

        // Extract namespace aliases from file content for completion
        let aliases = self.extract_namespace_aliases(&text);

        // Check for variable completion (::ns/)
        if let Some(var_ctx) = extract_variable_context(prefix) {
            let symbol_index = self.symbol_index.lock().await;
            if let Some(index) = &*symbol_index {
                // Try to resolve the namespace through aliases first
                let resolved_ns = self.resolve_namespace_alias(&var_ctx.namespace, &aliases);
                let resolved_ctx = VariableContext {
                    namespace: resolved_ns.clone(),
                };

                // Get completions from resolved namespace
                let items = complete_variables(index, &resolved_ctx);
                if !items.is_empty() {
                    return Ok(Some(CompletionResponse::Array(items)));
                }
                // Fall back to original namespace if resolution didn't find anything
                if resolved_ns != var_ctx.namespace {
                    let items = complete_variables(index, &var_ctx);
                    if !items.is_empty() {
                        return Ok(Some(CompletionResponse::Array(items)));
                    }
                }
            }
        }

        // Fallback: if prefix contains :: pattern, provide completions
        if prefix.contains("::") {
            let symbol_index = self.symbol_index.lock().await;
            if let Some(index) = &*symbol_index {
                // If it looks like a variable context (contains /), try to extract namespace and provide vars
                if prefix.contains('/') {
                    // Try to find the namespace before the /
                    if let Some(slash_pos) = prefix.rfind('/') {
                        let ns_part = &prefix[..slash_pos];
                        if ns_part.starts_with("::") {
                            // Try to resolve through aliases
                            let resolved_ns = self.resolve_namespace_alias(ns_part, &aliases);

                            // Try resolved namespace first
                            if let Some(vars) = index.ns_to_vars.get(&resolved_ns) {
                                let items: Vec<CompletionItem> = vars
                                    .iter()
                                    .map(|v| CompletionItem {
                                        label: v.name.clone(),
                                        kind: Some(v.kind),
                                        detail: v.detail.clone(),
                                        ..Default::default()
                                    })
                                    .collect();
                                return Ok(Some(CompletionResponse::Array(items)));
                            }

                            // Fall back to original namespace
                            if resolved_ns != ns_part
                                && let Some(vars) = index.ns_to_vars.get(ns_part)
                            {
                                let items: Vec<CompletionItem> = vars
                                    .iter()
                                    .map(|v| CompletionItem {
                                        label: v.name.clone(),
                                        kind: Some(v.kind),
                                        detail: v.detail.clone(),
                                        ..Default::default()
                                    })
                                    .collect();
                                return Ok(Some(CompletionResponse::Array(items)));
                            }
                        }
                    }
                }
            }
        }

        // Check for struct field completion: TypeName({ or TypeName({field: value,
        if let Some(type_field_ctx) = extract_type_field_context(prefix) {
            let symbol_index = self.symbol_index.lock().await;
            if let Some(index) = &*symbol_index
                && let Some(items) =
                    self.complete_type_fields(index, &type_field_ctx.type_name, &text)
            {
                return Ok(Some(CompletionResponse::Array(items)));
            }
        }

        // Fallback: suggest core functions for bare identifier prefixes
        // Extract the current word being typed (for bare identifier completion)
        if let Some(word_prefix) = extract_bare_identifier_prefix(prefix)
            && !word_prefix.is_empty()
        {
            let symbol_index = self.symbol_index.lock().await;
            if let Some(index) = &*symbol_index {
                let items: Vec<CompletionItem> = index
                    .core_vars
                    .iter()
                    .filter(|v| v.name.starts_with(&word_prefix))
                    .map(|v| CompletionItem {
                        label: v.name.clone(),
                        kind: Some(v.kind),
                        detail: v.detail.clone(),
                        documentation: v.doc_markdown.as_ref().map(|md| {
                            Documentation::MarkupContent(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: md.clone(),
                            })
                        }),
                        ..Default::default()
                    })
                    .collect();
                if !items.is_empty() {
                    return Ok(Some(CompletionResponse::Array(items)));
                }
            }
        }

        Ok(None)
    }

    async fn hover(&self, params: HoverParams) -> tower_lsp::jsonrpc::Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        if !uri.path().ends_with(".hot") {
            return Ok(None);
        }

        // Get document text
        let text = {
            let docs = self.docs.lock().await;
            if let Some(t) = docs.get(&uri) {
                t.clone()
            } else {
                return Ok(None);
            }
        };

        let line_text = text.lines().nth(position.line as usize).unwrap_or("");
        let col = position.character as usize;
        let (left, right) = if col <= line_text.len() {
            (&line_text[..col], &line_text[col..])
        } else {
            (line_text, "")
        };

        // Try to extract a symbol under cursor
        if let Some(symbol) = extract_symbol_at_position(left, right)
            && let Some(hover_content) = self
                .get_hover_for_symbol_with_context(
                    &symbol,
                    &uri,
                    &text,
                    position.line,
                    position.character,
                )
                .await
        {
            return Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: hover_content,
                }),
                range: None,
            }));
        }

        // Note: Function signature help after opening parenthesis is handled by signature_help method
        // This hover method is for hovering over symbols directly

        Ok(None)
    }

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> tower_lsp::jsonrpc::Result<Option<SignatureHelp>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        if !uri.path().ends_with(".hot") {
            return Ok(None);
        }

        // Get document text
        let text = {
            let docs = self.docs.lock().await;
            if let Some(t) = docs.get(&uri) {
                t.clone()
            } else {
                return Ok(None);
            }
        };

        let line_text = text.lines().nth(position.line as usize).unwrap_or("");
        let col = position.character as usize;
        let left = if col <= line_text.len() {
            &line_text[..col]
        } else {
            line_text
        };

        // Extract function name before the opening parenthesis
        if let Some(function_symbol) = extract_function_before_paren(left) {
            let active_parameter = active_parameter_from_call_args(left);
            // Try to get function information directly from the symbol index
            if let Some(signature_infos) = self.get_all_function_signatures(&function_symbol).await
            {
                let signature_help = SignatureHelp {
                    signatures: signature_infos,
                    active_signature: Some(0),
                    active_parameter: Some(active_parameter),
                };

                return Ok(Some(signature_help));
            }

            // Fallback to hover content if direct lookup fails
            if let Some(hover_content) = self.get_hover_for_symbol(&function_symbol).await {
                // Extract function signature from hover content
                if let Some(signature_info) = self.extract_signature_from_hover(&hover_content) {
                    let signature_help = SignatureHelp {
                        signatures: vec![signature_info],
                        active_signature: Some(0),
                        active_parameter: Some(active_parameter),
                    };

                    return Ok(Some(signature_help));
                }
            }
        }

        Ok(None)
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> tower_lsp::jsonrpc::Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        if !uri.path().ends_with(".hot") {
            return Ok(None);
        }

        let text = {
            let docs = self.docs.lock().await;
            match docs.get(&uri) {
                Some(t) => t.clone(),
                None => return Ok(None),
            }
        };

        let line_text = text.lines().nth(position.line as usize).unwrap_or("");
        let col = position.character as usize;
        let (left, right) = if col <= line_text.len() {
            (&line_text[..col], &line_text[col..])
        } else {
            (line_text, "")
        };

        let symbol = match extract_symbol_at_position(left, right) {
            Some(s) => s,
            None => return Ok(None),
        };

        let symbol_index = self.symbol_index.lock().await;
        let index = match &*symbol_index {
            Some(idx) => idx,
            None => return Ok(None),
        };

        let current_namespace = self.extract_namespace_from_file(&text);
        let resolved = self
            .resolve_symbol_with_context(
                &symbol,
                index,
                &current_namespace,
                &text,
                position.line,
                position.character,
            )
            .await;

        if let Some((_ns, _name, var_info)) = resolved
            && let Some(src) = &var_info.def_source
        {
            let target_uri = if let Some(ref file_path) = src.file {
                match Url::from_file_path(file_path) {
                    Ok(u) => u,
                    Err(_) => uri.clone(),
                }
            } else {
                uri.clone()
            };

            let def_line = src.line.saturating_sub(1) as u32;
            let def_col = src.column.saturating_sub(1) as u32;
            let target_pos = Position::new(def_line, def_col);
            let target_range = Range::new(
                target_pos,
                Position::new(def_line, def_col + src.length as u32),
            );

            return Ok(Some(GotoDefinitionResponse::Scalar(Location::new(
                target_uri,
                target_range,
            ))));
        }

        Ok(None)
    }

    async fn references(
        &self,
        params: ReferenceParams,
    ) -> tower_lsp::jsonrpc::Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        if !uri.path().ends_with(".hot") {
            return Ok(None);
        }

        let text = {
            let docs = self.docs.lock().await;
            match docs.get(&uri) {
                Some(t) => t.clone(),
                None => return Ok(None),
            }
        };

        let line_text = text.lines().nth(position.line as usize).unwrap_or("");
        let col = position.character as usize;
        let (left, right) = if col <= line_text.len() {
            (&line_text[..col], &line_text[col..])
        } else {
            (line_text, "")
        };

        let symbol = match extract_symbol_at_position(left, right) {
            Some(s) => s,
            None => return Ok(None),
        };

        let symbol_index = self.symbol_index.lock().await;
        let index = match &*symbol_index {
            Some(idx) => idx,
            None => return Ok(None),
        };

        let current_namespace = self.extract_namespace_from_file(&text);
        let resolved = self
            .resolve_symbol_with_context(
                &symbol,
                index,
                &current_namespace,
                &text,
                position.line,
                position.character,
            )
            .await;

        let mut names = vec![symbol.clone()];
        let mut declaration = None;
        if let Some((namespace, name, var_info)) = resolved {
            if !names.iter().any(|n| n == &name) {
                names.push(name.clone());
            }
            let qualified = format!("{}/{}", namespace, name);
            if !names.iter().any(|n| n == &qualified) {
                names.push(qualified);
            }
            declaration = var_info.def_source.as_ref().and_then(source_to_location);
        }

        let docs = self.docs.lock().await.clone();
        let mut locations = Vec::new();
        let mut seen = AHashSet::new();
        for (doc_uri, doc_text) in docs {
            for location in find_symbol_locations(&doc_uri, &doc_text, &names) {
                let key = location_key(&location);
                if seen.insert(key) {
                    locations.push(location);
                }
            }
        }

        if params.context.include_declaration
            && let Some(location) = declaration
        {
            let key = location_key(&location);
            if seen.insert(key) {
                locations.push(location);
            }
        }

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(locations))
        }
    }
}

impl Backend {
    /// Source text keyed by `Path::display()` string, for ariadne-style diagnostics in the LSP (no ANSI).
    fn lsp_error_sources(uri: &Url, text: &str) -> AHashMap<String, String> {
        let mut sources = AHashMap::new();
        if let Ok(path) = uri.to_file_path() {
            sources.insert(path.display().to_string(), text.to_string());
        }
        sources
    }

    /// Add source content for any file referenced by an error location that
    /// isn't already in the cache. Mirrors the lazy-load strategy used by
    /// `Engine::check_sources_pipeline_diagnostics` so diagnostics for
    /// dependency / cross-file locations still render with the ariadne body.
    fn ensure_error_sources(sources: &mut AHashMap<String, String>, errors: &[CompilerError]) {
        for error in errors {
            if let Some(loc) = error.location()
                && let Some(file) = loc.file.as_ref()
            {
                let key = file.display().to_string();
                if !sources.contains_key(&key)
                    && let Ok(content) = std::fs::read_to_string(file)
                {
                    sources.insert(key, content);
                }
            }
        }
    }

    fn parse_error_diagnostic(
        text: &str,
        parse_error: &hot::lang::parser::ParseError,
    ) -> Diagnostic {
        let loc = &parse_error.location;
        let start_line = loc.line.saturating_sub(1) as u32;
        let start_char = loc.column.saturating_sub(1) as u32;
        let end_char = start_char.saturating_add(loc.length.max(1) as u32);
        let message = parse_error
            .format_error(text, false)
            .unwrap_or_else(|| parse_error.to_string());
        Diagnostic {
            range: Range {
                start: Position {
                    line: start_line,
                    character: start_char,
                },
                end: Position {
                    line: start_line,
                    character: end_char,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(NumberOrString::String("parse-error".to_string())),
            code_description: None,
            source: Some("hot".to_string()),
            message,
            related_information: None,
            tags: None,
            data: None,
        }
    }

    /// Analyze Hot file and cache the compiled program and symbol index
    async fn analyze_and_cache(
        &self,
        conf: &Val,
        src_paths: &[PathBuf],
        uri: Url,
        text: String,
    ) -> Vec<Diagnostic> {
        // Build symbol index for completions
        self.build_symbol_index().await;

        let lsp_sources = Self::lsp_error_sources(&uri, &text);

        // Parse the current file (with path so errors and ariadne reports resolve source)
        let current_program = match uri.to_file_path() {
            Ok(ref path) => {
                let path_str = path.to_string_lossy();
                match hot::lang::parser::parse_hot_file(&text, path_str.as_ref()) {
                    Ok(program) => program,
                    Err(ref parse_error) => {
                        return vec![Self::parse_error_diagnostic(&text, parse_error)];
                    }
                }
            }
            Err(_) => match hot::lang::parser::parse_hot(&text) {
                Ok(program) => program,
                Err(ref parse_error) => {
                    return vec![Self::parse_error_diagnostic(&text, parse_error)];
                }
            },
        };

        // Add the current file to the symbol index
        self.add_current_file_to_symbol_index(&current_program)
            .await;

        // Create a comprehensive program that includes dependencies
        let mut comprehensive_program = match self
            .build_comprehensive_program(conf, src_paths, &current_program)
            .await
        {
            Ok(program) => program,
            Err(e) => {
                tracing::warn!("Failed to build comprehensive program: {}", e);
                // Fall back to just the current file
                current_program
            }
        };

        // Store the comprehensive program for type lookups
        {
            let mut program_lock = self.program.lock().await;
            *program_lock = Some(comprehensive_program.clone());
        }

        // Run type checking with validation on the comprehensive program
        let mut compiler = Compiler::new();
        if let Ok(path) = uri.to_file_path() {
            compiler.add_source_file(path, text.clone());
        }
        match compiler.compile_program(&mut comprehensive_program) {
            Ok(_) => Vec::new(), // No errors
            Err(compiler_errors) => {
                // Lazy-load source for any cross-file location referenced
                // by the errors so ariadne can render snippets even when
                // the location isn't in the open buffer.
                let mut lsp_sources = lsp_sources;
                Self::ensure_error_sources(&mut lsp_sources, &compiler_errors.errors);

                compiler_errors
                    .errors
                    .iter()
                    .filter_map(|error| {
                        if self.error_is_from_current_file(error, &uri) {
                            self.compiler_error_to_diagnostic(error, &uri, &lsp_sources)
                        } else {
                            None
                        }
                    })
                    .collect()
            }
        }
    }

    /// Build a comprehensive program that includes hot-std, project dependencies, and the current file
    async fn build_comprehensive_program(
        &self,
        conf: &Val,
        src_paths: &[PathBuf],
        current_program: &hot::lang::ast::Program,
    ) -> Result<hot::lang::ast::Program, String> {
        use hot::lang::ast::Program;

        let mut comprehensive_program = Program {
            namespaces: Default::default(), // Use Default::default() for IndexMap
            current_namespace: current_program.current_namespace.clone(),
        };

        // 1. Load hot-std package
        if let Ok(hot_std_program) = self.discover_and_parse_hot_files().await {
            for (ns_path, namespace) in hot_std_program.namespaces {
                comprehensive_program.namespaces.insert(ns_path, namespace);
            }
        }

        // 2. Load project dependencies from hot.hot config
        let project_name = hot::project::get_default_project_name(conf);
        if let Ok(resolved_deps) =
            hot::project::get_resolved_project_dependencies(conf, &project_name)
        {
            for dep in &resolved_deps {
                if let Ok(files) =
                    hot::lang::engine::Engine::discover_dependency_source_files(&dep.resolved_path)
                {
                    for file_path in files {
                        if let Ok(content) = std::fs::read_to_string(&file_path)
                            && let Ok(parsed_program) = hot::lang::parser::parse_hot(&content)
                        {
                            for (ns_path, namespace) in parsed_program.namespaces {
                                comprehensive_program.namespaces.insert(ns_path, namespace);
                            }
                        }
                    }
                }
            }
        }

        // 3. Load source files from src_paths
        for src_path in src_paths {
            if let Some(src_path_str) = src_path.to_str()
                && let Ok(files) = self.discover_hot_files_in_dir(src_path_str)
            {
                for file_path in files {
                    if let Ok(content) = std::fs::read_to_string(&file_path)
                        && let Ok(parsed_program) = hot::lang::parser::parse_hot(&content)
                    {
                        for (ns_path, namespace) in parsed_program.namespaces {
                            comprehensive_program.namespaces.insert(ns_path, namespace);
                        }
                    }
                }
            }
        }

        // 4. Add the current file's program
        for (ns_path, namespace) in &current_program.namespaces {
            comprehensive_program
                .namespaces
                .insert(ns_path.clone(), namespace.clone());
        }

        Ok(comprehensive_program)
    }

    /// Check if an error is from the current file being analyzed.
    ///
    /// Uses the shared `CompilerError::location()` accessor so it stays in
    /// sync with new error variants automatically.
    fn error_is_from_current_file(&self, error: &CompilerError, uri: &Url) -> bool {
        if let Ok(current_file_path) = uri.to_file_path()
            && let Some(loc) = error.location()
            && let Some(error_file) = &loc.file
        {
            return error_file == &current_file_path;
        }

        // If we can't determine the file, show the error (better to show too many than too few)
        true
    }

    /// Convert a CompilerError to an LSP Diagnostic (ariadne-style body, no ANSI).
    ///
    /// Uses the shared `CompilerError::location()` / `CompilerError::code()`
    /// accessors and the central ariadne formatter so the LSP message matches
    /// what `hot check --check.format json` emits.
    fn compiler_error_to_diagnostic(
        &self,
        error: &CompilerError,
        _uri: &Url,
        lsp_sources: &AHashMap<String, String>,
    ) -> Option<Diagnostic> {
        let range = if let Some(loc) = error.location() {
            let start_line = (loc.line.saturating_sub(1)) as u32;
            let start_char = (loc.column.saturating_sub(1)) as u32;
            let end_char = start_char.saturating_add(loc.length.max(1) as u32);
            Range {
                start: Position {
                    line: start_line,
                    character: start_char,
                },
                end: Position {
                    line: start_line,
                    character: end_char,
                },
            }
        } else {
            Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 0,
                },
            }
        };

        let mut batch = CompilerErrors::new();
        batch.add(error.clone());
        for (path, content) in lsp_sources {
            batch.add_source(path.clone(), content.clone());
        }
        let message = batch.format_error(false);

        Some(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(NumberOrString::String(error.code().to_string())),
            code_description: None,
            source: Some("hot".to_string()),
            message,
            related_information: None,
            tags: None,
            data: None,
        })
    }

    /// Build symbol index and unified resolver from available Hot standard library and project files
    async fn build_symbol_index(&self) {
        let mut index = SymbolIndex::default();
        let mut namespace_registry = NamespaceRegistry::new();
        let mut core_variables = CoreVariableRegistry::new();

        // Discover and parse Hot standard library files
        if let Ok(program) = self.discover_and_parse_hot_files().await {
            // Extract namespaces and their variables from the parsed program
            for (ns_path, namespace) in &program.namespaces {
                let ns_string = ns_path.to_string();
                index.namespaces.push(ns_string.clone());

                // Extract variables/functions from this namespace
                let mut vars = Vec::new();
                for (var, var_value) in &namespace.scope.vars {
                    let var_name_str = var.sym.name();

                    // Extract function signature and documentation from metadata
                    let (detail, doc_markdown) = self.extract_var_metadata(var, var_value);

                    vars.push(SymbolVar {
                        name: var_name_str.to_string(),
                        kind: CompletionItemKind::FUNCTION,
                        detail: detail.clone(),
                        doc_markdown: doc_markdown.clone(),
                        ast_value: var_value.clone(),
                        def_source: var.src.clone(),
                    });

                    // Add to namespace registry for UnifiedResolver
                    use hot::lang::bytecode::{VariableInfo, VariableType};
                    let var_info = VariableInfo {
                        name: var_name_str.to_string(),
                        var_type: if matches!(var_value, hot::lang::ast::Value::Fn(_)) {
                            VariableType::Function
                        } else {
                            VariableType::Value
                        },
                        metadata: var.meta.as_ref().map(|m| m.val.clone()),
                        function_id: None,
                    };
                    namespace_registry.add_variable(&ns_string, var_info);

                    // Check if this is a core variable and add to core registry
                    let is_core = self.is_core_variable_from_metadata(var)
                        || self.is_core_type_def(var, var_value);
                    if is_core {
                        use hot::lang::compiler::core_registry::CoreVariableType;
                        let variable_type = if matches!(var_value, hot::lang::ast::Value::Fn(_)) {
                            CoreVariableType::Function
                        } else if matches!(var_value, hot::lang::ast::Value::TypeDef(_)) {
                            CoreVariableType::TypeConstructor
                        } else {
                            CoreVariableType::Constant
                        };
                        core_variables.add_core_variable(
                            var_name_str.to_string(),
                            var_value.clone(),
                            ns_string.clone(),
                            variable_type,
                        );

                        // Also add to index.core_vars for LSP completions
                        let kind = match var_value {
                            hot::lang::ast::Value::Fn(_) => CompletionItemKind::FUNCTION,
                            hot::lang::ast::Value::TypeDef(_) => CompletionItemKind::STRUCT,
                            _ => CompletionItemKind::CONSTANT,
                        };
                        index.core_vars.push(SymbolVar {
                            name: var_name_str.to_string(),
                            kind,
                            detail: detail.clone(),
                            doc_markdown: doc_markdown.clone(),
                            ast_value: var_value.clone(),
                            def_source: var.src.clone(),
                        });
                    }
                }

                if !vars.is_empty() {
                    index.ns_to_vars.insert(ns_string, vars);
                }
            }

            // Create UnifiedResolver with the populated registries
            let resolver = UnifiedResolver::with_registries(namespace_registry, core_variables);
            let mut unified_resolver = self.unified_resolver.lock().await;
            *unified_resolver = Some(resolver);

            // Store the program for type lookup
            let mut program_lock = self.program.lock().await;
            *program_lock = Some(program);
        }

        let mut symbol_index = self.symbol_index.lock().await;
        *symbol_index = Some(index);
    }

    /// Discover and parse all Hot files (standard library + project files)
    async fn discover_and_parse_hot_files(&self) -> Result<Program, String> {
        // Start with hot-std files
        let mut all_hot_files = Vec::new();

        // Find hot-std directory
        // Resolution order: HOT_HOME > macOS package > Linux package > development paths
        let mut hot_std_paths = Vec::new();

        // Check HOT_HOME first
        if let Ok(home) = std::env::var("HOT_HOME") {
            hot_std_paths.push(format!("{}/pkg/hot-std/src", home));
        }

        // Add macOS and Linux package installation paths
        hot_std_paths.push("/usr/local/share/hot/pkg/hot-std/src".to_string());
        hot_std_paths.push("/usr/share/hot/pkg/hot-std/src".to_string());

        // Add development fallback paths
        hot_std_paths.extend(vec![
            "hot/pkg/hot-std/src".to_string(),
            "../hot/pkg/hot-std/src".to_string(),
            "../../hot/pkg/hot-std/src".to_string(),
            "./hot/pkg/hot-std/src".to_string(),
        ]);

        for hot_std_path in &hot_std_paths {
            if std::path::Path::new(hot_std_path).exists()
                && let Ok(files) = self.discover_hot_files_in_dir(hot_std_path)
            {
                all_hot_files.extend(files);
                break;
            }
        }

        // Load project dependency source files
        {
            let conf = self.conf.lock().await;
            let project_name = hot::project::get_default_project_name(&conf);
            if let Ok(resolved_deps) =
                hot::project::get_resolved_project_dependencies(&conf, &project_name)
            {
                for dep in &resolved_deps {
                    if let Ok(files) = hot::lang::engine::Engine::discover_dependency_source_files(
                        &dep.resolved_path,
                    ) {
                        all_hot_files.extend(files);
                    }
                }
            }
        }

        // Also include project source files if available
        let src_paths = self.src_paths.lock().await;
        for src_path in src_paths.iter() {
            if let Some(src_path_str) = src_path.to_str()
                && let Ok(files) = self.discover_hot_files_in_dir(src_path_str)
            {
                all_hot_files.extend(files);
            }
        }

        // Parse all discovered files into a single program
        let mut program = Program {
            namespaces: Default::default(), // Use Default::default() for IndexMap
            current_namespace: hot::lang::ast::NsPath::from_vec(vec![
                hot::lang::ast::NsPathPart::Sym(hot::lang::ast::Sym::String("hot".to_string())),
            ]),
        };

        for file_path in all_hot_files {
            if let Ok(content) = std::fs::read_to_string(&file_path)
                && let Ok(parsed_program) = hot::lang::parser::parse_hot(&content)
            {
                // Merge namespaces from this file
                for (ns_path, namespace) in parsed_program.namespaces {
                    program.namespaces.insert(ns_path, namespace);
                }
            }
        }

        Ok(program)
    }

    /// Discover `.hot` files under a directory.
    ///
    /// Routes through `hot::discovery::discover` so the LSP honors
    /// `.gitignore` / `.hotignore` / default hard-excludes uniformly with
    /// the rest of the toolchain.
    fn discover_hot_files_in_dir(&self, dir_path: &str) -> Result<Vec<String>, String> {
        let opts = hot::discovery::DiscoveryOpts::for_extension("hot");
        Ok(hot::discovery::discover_paths(
            &[std::path::PathBuf::from(dir_path)],
            &opts,
        ))
    }

    /// Extract metadata (function signature and documentation) from a variable
    fn extract_var_metadata(
        &self,
        var: &hot::lang::ast::Var,
        var_value: &hot::lang::ast::Value,
    ) -> (Option<String>, Option<String>) {
        // Try to extract documentation from metadata
        let doc_markdown = if let Some(meta) = self.get_var_meta(var) {
            self.extract_doc_from_meta(&meta)
        } else {
            None
        };

        // Try to extract function signature
        let detail = self.extract_function_signature(var_value);

        (detail, doc_markdown)
    }

    /// Complete struct/type fields for type constructor calls like Person({
    fn complete_type_fields(
        &self,
        index: &SymbolIndex,
        type_name: &str,
        file_content: &str,
    ) -> Option<Vec<CompletionItem>> {
        // First, try to find the type in core_vars
        for var in &index.core_vars {
            if var.name == type_name
                && let hot::lang::ast::Value::TypeDef(type_def) = &var.ast_value
            {
                return self.fields_to_completion_items(type_def);
            }
        }

        // Then, try to find in all namespaces
        // Handle namespaced type names like ::myapp/Person
        let base_type_name = if let Some(slash_pos) = type_name.rfind('/') {
            &type_name[slash_pos + 1..]
        } else {
            type_name
        };

        // Extract namespace aliases for resolution
        let aliases = self.extract_namespace_aliases(file_content);

        // If it's a qualified type name, resolve the namespace
        if (type_name.contains('/') || type_name.contains("::"))
            && let Some(slash_pos) = type_name.rfind('/')
        {
            let ns_part = &type_name[..slash_pos];
            let resolved_ns = self.resolve_namespace_alias(ns_part, &aliases);

            if let Some(vars) = index.ns_to_vars.get(&resolved_ns) {
                for var in vars {
                    if var.name == base_type_name
                        && let hot::lang::ast::Value::TypeDef(type_def) = &var.ast_value
                    {
                        return self.fields_to_completion_items(type_def);
                    }
                }
            }
        }

        // Search all namespaces for the type
        for vars in index.ns_to_vars.values() {
            for var in vars {
                if var.name == base_type_name
                    && let hot::lang::ast::Value::TypeDef(type_def) = &var.ast_value
                {
                    return self.fields_to_completion_items(type_def);
                }
            }
        }

        None
    }

    /// Convert TypeDef fields to CompletionItems
    fn fields_to_completion_items(
        &self,
        type_def: &hot::lang::ast::TypeDef,
    ) -> Option<Vec<CompletionItem>> {
        let fields = type_def.fields.as_ref()?;

        let items: Vec<CompletionItem> = fields
            .iter()
            .map(|field| {
                let field_name = field.name.name();
                CompletionItem {
                    label: field_name.to_string(),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: Some(format!("{}: {}", field_name, field.type_annotation)),
                    // Insert the field name followed by ": " for convenience
                    insert_text: Some(format!("{}: ", field_name)),
                    insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                    ..Default::default()
                }
            })
            .collect();

        if items.is_empty() { None } else { Some(items) }
    }

    /// Get metadata from a variable (the Var itself, not the Value)
    fn get_var_meta(&self, var: &hot::lang::ast::Var) -> Option<hot::val::Val> {
        var.meta.as_ref().map(|m| m.val.clone())
    }

    /// Extract documentation string from metadata
    fn extract_doc_from_meta(&self, meta: &hot::val::Val) -> Option<String> {
        use hot::val::Val;

        // Metadata should now be a proper Val::Map thanks to parser fix
        if let Val::Map(map) = meta
            && let Some(Val::Str(doc)) = map.get(&Val::from("doc"))
        {
            return Some(doc.trim().to_string());
        }

        None
    }

    /// Get flow modifier string for display
    fn get_flow_modifier(flow_type: &hot::lang::ast::FlowType) -> &'static str {
        match flow_type {
            hot::lang::ast::FlowType::Serial => "",
            hot::lang::ast::FlowType::Parallel => " parallel",
            hot::lang::ast::FlowType::Cond => " cond",
            hot::lang::ast::FlowType::CondAll => " cond-all",
            hot::lang::ast::FlowType::SerialShort => " serial",
            hot::lang::ast::FlowType::ParallelShort => " parallel",
            hot::lang::ast::FlowType::CondShort => " cond",
            hot::lang::ast::FlowType::CondAllShort => " cond-all",
            hot::lang::ast::FlowType::Pipe => " pipe",
            hot::lang::ast::FlowType::Match => " match",
            hot::lang::ast::FlowType::MatchAll => " match-all",
            hot::lang::ast::FlowType::MatchShort => " match",
            hot::lang::ast::FlowType::MatchAllShort => " match-all",
        }
    }

    /// Extract function signature from a function definition
    fn extract_function_signature(&self, var_value: &hot::lang::ast::Value) -> Option<String> {
        use hot::lang::ast::Value;
        match var_value {
            Value::Fn(fn_defs) => {
                // Handle multiple function definitions (different arities)
                if fn_defs.is_empty() {
                    return Some("fn()".to_string());
                }

                // Build signatures for all arities
                let signatures: Vec<String> = fn_defs
                    .iter()
                    .map(|fn_def| {
                        // Determine the flow type from the function body
                        let flow_modifier = if let Value::Flow(flow) = &fn_def.body {
                            Self::get_flow_modifier(&flow.flow_type)
                        } else {
                            ""
                        };

                        let args: Vec<String> = fn_def
                            .args
                            .args
                            .iter()
                            .map(|arg| {
                                let name = arg.var.sym.name();
                                let lazy_prefix = if arg.lazy { "lazy " } else { "" };

                                if let Some(type_annotation) = &arg.type_annotation {
                                    // Format the type annotation to be more readable
                                    let formatted_type =
                                        self.format_type_annotation(type_annotation);
                                    format!("{}{}: {}", lazy_prefix, name, formatted_type)
                                } else {
                                    format!("{}{}", lazy_prefix, name)
                                }
                            })
                            .collect();
                        format!("fn{}({})", flow_modifier, args.join(", "))
                    })
                    .collect();

                // Join all signatures with newlines for multi-arity functions
                Some(signatures.join("\n"))
            }
            _ => None,
        }
    }

    /// Format type annotation from debug format to readable Hot syntax
    fn format_type_annotation(&self, type_annotation: &str) -> String {
        // Convert debug format like "Union([Builtin(Int), Builtin(Dec)])" to "Int | Dec"
        if type_annotation.starts_with("Union([") && type_annotation.ends_with("])") {
            // Extract the inner types from Union([...])
            let inner = &type_annotation[7..type_annotation.len() - 2]; // Remove "Union([" and "])"
            let types: Vec<&str> = inner.split(", ").collect();
            let formatted_types: Vec<String> =
                types.iter().map(|t| self.format_single_type(t)).collect();
            formatted_types.join(" | ")
        } else {
            // Handle single types or other formats
            self.format_single_type(type_annotation)
        }
    }

    /// Format a single type from debug format to readable format
    fn format_single_type(&self, type_str: &str) -> String {
        if type_str.starts_with("Builtin(") && type_str.ends_with(")") {
            // Extract type from Builtin(TypeName)
            let inner = &type_str[8..type_str.len() - 1]; // Remove "Builtin(" and ")"
            inner.to_string()
        } else if type_str.starts_with("Named(\"") && type_str.ends_with("\")") {
            // Extract type from Named("TypeName")
            let inner = &type_str[7..type_str.len() - 2]; // Remove "Named(\"" and "\")"
            inner.to_string()
        } else {
            // Return as-is for other formats
            type_str.to_string()
        }
    }

    /// Look up type definition and format its fields for display
    async fn get_type_fields_info(&self, type_name: &str) -> Option<String> {
        tracing::debug!("get_type_fields_info called for type: {}", type_name);

        let program = self.program.lock().await;
        if let Some(prog) = &*program {
            tracing::debug!(
                "Program is available, checking {} namespaces",
                prog.namespaces.len()
            );

            // Search all namespaces for the type definition
            for (ns_path, namespace) in &prog.namespaces {
                tracing::debug!("Checking namespace: {}", ns_path);

                for (var, value) in &namespace.scope.vars {
                    // Check if this is a type definition with matching name
                    if let hot::lang::ast::Value::TypeDef(type_def) = value {
                        let type_def_name = type_def.name.name();
                        tracing::debug!(
                            "Found TypeDef: {} (var name: {})",
                            type_def_name,
                            var.sym.name()
                        );

                        // Match simple name or fully qualified name
                        if type_def_name == type_name
                            || type_name.ends_with(&format!("/{}", type_def_name))
                        {
                            tracing::debug!("Type name matches! Looking for constructor");

                            // Extract fields from constructor function
                            if let Some(constructors) = &type_def.constructor_functions {
                                tracing::debug!("Found {} constructors", constructors.len());

                                if let Some(first_constructor) = constructors.first() {
                                    tracing::debug!(
                                        "First constructor has {} args",
                                        first_constructor.args.args.len()
                                    );

                                    let fields: Vec<String> = first_constructor
                                        .args
                                        .args
                                        .iter()
                                        .map(|arg| {
                                            let field_name = arg.var.sym.name();
                                            if let Some(field_type) = &arg.type_annotation {
                                                let formatted_type =
                                                    self.format_type_annotation(field_type);
                                                format!("{}: {}", field_name, formatted_type)
                                            } else {
                                                field_name.to_string()
                                            }
                                        })
                                        .collect();

                                    if !fields.is_empty() {
                                        let result = format!("{{ {} }}", fields.join(", "));
                                        tracing::debug!("Returning fields: {}", result);
                                        return Some(result);
                                    } else {
                                        tracing::debug!("Fields vector is empty");
                                    }
                                } else {
                                    tracing::debug!("No first constructor");
                                }
                            } else {
                                tracing::debug!("No constructor functions");
                            }
                        }
                    }
                }
            }
            tracing::debug!("Type {} not found in any namespace", type_name);
        } else {
            tracing::debug!("Program is None");
        }
        None
    }

    /// Format comprehensive type hover information
    async fn format_type_hover(
        &self,
        fully_qualified_name: &str,
        type_def: &hot::lang::ast::TypeDef,
        doc_markdown: &Option<String>,
        var_meta: &Option<hot::val::Val>,
    ) -> String {
        let mut content = format!("### {}\n```hot\ntype\n```\n\n", fully_qualified_name);

        // Add documentation if available
        if let Some(doc) = doc_markdown {
            content.push_str(doc);
            content.push_str("\n\n");
        }

        // Show type alias if present (e.g., literal unions like "apple" | "banana")
        if let Some(type_alias) = &type_def.type_alias {
            content.push_str("#### Type Alias\n\n");
            content.push_str(&format!("`{}`\n\n", type_alias));
        }

        // Show variants if present (e.g., variant unions like | Up | Down)
        if let Some(variants) = &type_def.variants {
            content.push_str("#### Variants\n\n");
            for variant in variants {
                if let Some(type_ref) = &variant.type_ref {
                    content.push_str(&format!("- `{}({})`\n", variant.name, type_ref));
                } else {
                    content.push_str(&format!("- `{}`\n", variant.name));
                }
            }
            content.push('\n');
        }

        // Show fields if they exist (from inline struct definition)
        let has_fields = type_def.fields.is_some() && !type_def.fields.as_ref().unwrap().is_empty();

        if has_fields {
            content.push_str("#### Fields\n\n");
            if let Some(fields) = &type_def.fields {
                for field in fields {
                    let field_name = field.name.name();
                    let field_type = &field.type_annotation;
                    content.push_str(&format!(
                        "- `{}`: `{}`\n",
                        field_name,
                        self.format_type_annotation(field_type)
                    ));
                }
            }
            content.push('\n');
        }

        // Show function signatures (constructors and implementations)
        let mut signatures = Vec::new();

        // 1. Default constructor (for types with fields)
        if has_fields {
            signatures.push("fn(map: Map)".to_string());
        }

        // 2. Explicit constructor functions
        if let Some(constructors) = &type_def.constructor_functions {
            for constructor in constructors.iter() {
                // Determine the flow type from the function body
                let flow_modifier = if let hot::lang::ast::Value::Flow(flow) = &constructor.body {
                    Self::get_flow_modifier(&flow.flow_type)
                } else {
                    ""
                };

                let args: Vec<String> = constructor
                    .args
                    .args
                    .iter()
                    .map(|arg| {
                        let name = arg.var.sym.name();
                        let lazy_prefix = if arg.lazy { "lazy " } else { "" };

                        if let Some(type_annotation) = &arg.type_annotation {
                            let formatted_type = self.format_type_annotation(type_annotation);
                            format!("{}{}: {}", lazy_prefix, name, formatted_type)
                        } else {
                            format!("{}{}", lazy_prefix, name)
                        }
                    })
                    .collect();
                signatures.push(format!("fn{}({})", flow_modifier, args.join(", ")));
            }
        }

        // 3. Type implementation signatures (with blank line separator)
        // Note: The parser fills in inferred types, so type_annotation is always present
        let implementations = self.get_type_implementations(type_def.name.name()).await;
        if !implementations.is_empty() {
            // Add blank line before implementations
            if !signatures.is_empty() {
                signatures.push(String::new());
            }

            for (target_type, impl_fn) in &implementations {
                // Extract signature from implementation function
                if let hot::lang::ast::Value::Fn(fn_defs) = impl_fn
                    && let Some(first_fn) = fn_defs.first()
                {
                    // Determine the flow type from the function body
                    let flow_modifier = if let hot::lang::ast::Value::Flow(flow) = &first_fn.body {
                        Self::get_flow_modifier(&flow.flow_type)
                    } else {
                        ""
                    };

                    let args: Vec<String> = first_fn
                        .args
                        .args
                        .iter()
                        .map(|arg| {
                            let name = arg.var.sym.name();
                            let lazy_prefix = if arg.lazy { "lazy " } else { "" };

                            if let Some(type_annotation) = &arg.type_annotation {
                                let formatted_type = self.format_type_annotation(type_annotation);
                                format!("{}{}: {}", lazy_prefix, name, formatted_type)
                            } else {
                                format!("{}{}", lazy_prefix, name)
                            }
                        })
                        .collect();

                    // Get return type - for implements, it's the target type
                    signatures.push(format!(
                        "implements {} fn{}({}): {}",
                        target_type,
                        flow_modifier,
                        args.join(", "),
                        target_type
                    ));
                }
            }
        }

        // 4. Implementations FROM other types TO this type (e.g., PlainDate implements Str)
        // Note: The parser fills in inferred types, so type_annotation is always present
        let incoming_implementations = self.get_implementations_to_type(type_def.name.name()).await;
        if !incoming_implementations.is_empty() {
            // Add blank line if there were previous signatures
            if !signatures.is_empty() && !implementations.is_empty() {
                // Already have blank line from implementations
            } else if !signatures.is_empty() {
                signatures.push(String::new());
            }

            for (source_type, impl_fn) in &incoming_implementations {
                // Extract signature from implementation function
                if let hot::lang::ast::Value::Fn(fn_defs) = impl_fn
                    && let Some(first_fn) = fn_defs.first()
                {
                    // Determine the flow type from the function body
                    let flow_modifier = if let hot::lang::ast::Value::Flow(flow) = &first_fn.body {
                        Self::get_flow_modifier(&flow.flow_type)
                    } else {
                        ""
                    };

                    let args: Vec<String> = first_fn
                        .args
                        .args
                        .iter()
                        .map(|arg| {
                            let name = arg.var.sym.name();
                            let lazy_prefix = if arg.lazy { "lazy " } else { "" };

                            if let Some(type_annotation) = &arg.type_annotation {
                                let formatted_type = self.format_type_annotation(type_annotation);
                                format!("{}{}: {}", lazy_prefix, name, formatted_type)
                            } else {
                                format!("{}{}", lazy_prefix, name)
                            }
                        })
                        .collect();

                    // Show as: SourceType -> TargetType fn(...): TargetType
                    signatures.push(format!(
                        "{} -> {} fn{}({}): {}",
                        source_type,
                        type_def.name.name(),
                        flow_modifier,
                        args.join(", "),
                        type_def.name.name()
                    ));
                }
            }
        }

        // Output all signatures
        if !signatures.is_empty() {
            content.push_str("```hot\n");
            for sig in signatures {
                content.push_str(&sig);
                content.push('\n');
            }
            content.push_str("```\n\n");
        }

        // Add metadata section (excluding "doc")
        if let Some(meta) = var_meta {
            let meta_section = self.format_meta_section(meta);
            if !meta_section.is_empty() {
                content.push_str(&meta_section);
            }
        }

        content
    }

    /// Get all type implementations for a given type
    async fn get_type_implementations(
        &self,
        type_name: &str,
    ) -> Vec<(String, hot::lang::ast::Value)> {
        let program = self.program.lock().await;
        let mut implementations = Vec::new();

        if let Some(prog) = &*program {
            // Search for implementations of this type
            for (_ns_path, namespace) in &prog.namespaces {
                for (_var, value) in &namespace.scope.vars {
                    if let hot::lang::ast::Value::TypeImplementation(type_impl) = value
                        && type_impl.source_type == type_name
                    {
                        implementations.push((
                            type_impl.target_type.clone(),
                            type_impl.implementation.clone(),
                        ));
                    }
                }
            }
        }

        implementations
    }

    /// Get all type implementations that target a given type (e.g., all types that implement Str)
    async fn get_implementations_to_type(
        &self,
        type_name: &str,
    ) -> Vec<(String, hot::lang::ast::Value)> {
        let program = self.program.lock().await;
        let mut implementations = Vec::new();

        if let Some(prog) = &*program {
            // Search for implementations where target_type matches this type
            for (_ns_path, namespace) in &prog.namespaces {
                for (_var, value) in &namespace.scope.vars {
                    if let hot::lang::ast::Value::TypeImplementation(type_impl) = value
                        && type_impl.target_type == type_name
                    {
                        implementations.push((
                            type_impl.source_type.clone(),
                            type_impl.implementation.clone(),
                        ));
                    }
                }
            }
        }

        implementations
    }

    /// Get metadata Val from the index for a given namespace and variable name
    async fn get_var_metadata_from_index(
        &self,
        namespace: &str,
        var_name: &str,
        _index: &SymbolIndex,
    ) -> Option<hot::val::Val> {
        // Get the program to access the actual Var structure
        let program = self.program.lock().await;
        if let Some(prog) = &*program {
            // Search for the variable in the namespace
            for (ns_path, ns_data) in &prog.namespaces {
                if ns_path.to_string() == namespace {
                    for (var, _value) in &ns_data.scope.vars {
                        if var.sym.name() == var_name {
                            return self.get_var_meta(var);
                        }
                    }
                }
            }
        }
        None
    }

    /// Format metadata section for hover display (excluding "doc")
    fn format_meta_section(&self, meta: &hot::val::Val) -> String {
        use hot::val::Val;

        let mut result = String::new();

        match meta {
            Val::Map(map) => {
                let mut non_doc_items = Vec::new();

                for (key, value) in map.iter() {
                    if let Val::Str(key_str) = key {
                        // Skip "doc" field
                        if &**key_str != "doc" {
                            let formatted_value = self.format_val_for_display(value);
                            non_doc_items.push(format!("- **{}**: {}", key_str, formatted_value));
                        }
                    }
                }

                if !non_doc_items.is_empty() {
                    result.push_str("#### Meta\n\n");
                    result.push_str(&non_doc_items.join("\n"));
                    result.push('\n');
                }
            }
            Val::Vec(tags) => {
                // For vector metadata (tags), show them as a list
                let mut tag_items = Vec::new();
                for tag in tags {
                    let formatted = self.format_val_for_display(tag);
                    tag_items.push(format!("- {}", formatted));
                }

                if !tag_items.is_empty() {
                    result.push_str("#### Meta\n\n");
                    result.push_str(&tag_items.join("\n"));
                    result.push('\n');
                }
            }
            // Single string metadata (e.g., #"core")
            Val::Str(s) if !s.is_empty() => {
                result.push_str("#### Meta\n\n");
                result.push_str(&format!("- `\"{}\"`\n", s));
            }
            _ => {}
        }

        result
    }

    /// Format a Val for display in metadata
    #[allow(clippy::only_used_in_recursion)]
    fn format_val_for_display(&self, val: &hot::val::Val) -> String {
        use hot::val::Val;

        match val {
            Val::Str(s) => format!("`\"{}\"`", s),
            Val::Int(i) => format!("`{}`", i),
            Val::Dec(d) => format!("`{}`", d),
            Val::Bool(b) => format!("`{}`", b),
            Val::Null => "`null`".to_string(),
            Val::Vec(v) => {
                let items: Vec<String> = v
                    .iter()
                    .map(|item| self.format_val_for_display(item))
                    .collect();
                format!("`[{}]`", items.join(", "))
            }
            Val::Map(m) => {
                let items: Vec<String> = m
                    .iter()
                    .map(|(k, v)| {
                        format!(
                            "{}: {}",
                            self.format_val_for_display(k),
                            self.format_val_for_display(v)
                        )
                    })
                    .collect();
                format!("`{{{}}}`", items.join(", "))
            }
            _ => "`<complex value>`".to_string(),
        }
    }

    /// Extract type information from AST Value (much cleaner than string parsing!)
    /// Extract type information from AST Value (much cleaner than string parsing!)
    /// Note: The parser fills in inferred types for TypeImplementations, so type annotations are always present
    async fn extract_type_info_from_ast(&self, ast_value: &hot::lang::ast::Value) -> String {
        use hot::lang::ast::Value;

        // Handle TypeImplementation - extract from the inner function
        if let Value::TypeImplementation(type_impl) = ast_value {
            return Box::pin(self.extract_type_info_from_ast(&type_impl.implementation)).await;
        }

        if let Value::Fn(fn_defs) = ast_value
            && let Some(first_fn) = fn_defs.first()
        {
            let mut type_infos = Vec::new();

            for arg in &first_fn.args.args {
                if let Some(type_annotation) = &arg.type_annotation {
                    let type_name = self.format_type_annotation(type_annotation);
                    let param_name = arg.var.sym.name();

                    // Check if this is a custom type (not a builtin)
                    let is_custom_type = !matches!(
                        type_name.as_str(),
                        "Str" | "Int" | "Dec" | "Bool" | "Vec" | "Map" | "Null" | "Any"
                    ) && !type_name.ends_with('?');

                    if is_custom_type {
                        // Look up the type definition
                        if let Some(fields) = self.get_type_fields_info(&type_name).await {
                            type_infos
                                .push(format!("- `{}` ({}): {}", param_name, type_name, fields));
                        }
                    }
                }
            }

            if !type_infos.is_empty() {
                return type_infos.join("\n");
            }
        }

        String::new()
    }

    /// Get hover information for a symbol with file context for proper lexical scoping
    async fn get_hover_for_symbol_with_context(
        &self,
        symbol: &str,
        _uri: &tower_lsp::lsp_types::Url,
        file_content: &str,
        cursor_line: u32,
        cursor_col: u32,
    ) -> Option<String> {
        let (resolved_namespace, resolved_name, detail, doc_markdown, ast_value, var_meta) = {
            let symbol_index = self.symbol_index.lock().await;
            if let Some(index) = &*symbol_index {
                let current_namespace = self.extract_namespace_from_file(file_content);

                if let Some((resolved_namespace, resolved_name, var_info)) = self
                    .resolve_symbol_with_context(
                        symbol,
                        index,
                        &current_namespace,
                        file_content,
                        cursor_line,
                        cursor_col,
                    )
                    .await
                {
                    let var_meta = self
                        .get_var_metadata_from_index(&resolved_namespace, &resolved_name, index)
                        .await;

                    (
                        resolved_namespace,
                        resolved_name,
                        var_info.detail.clone(),
                        var_info.doc_markdown.clone(),
                        var_info.ast_value.clone(),
                        var_meta,
                    )
                } else {
                    return None;
                }
            } else {
                return None;
            }
        };

        use hot::lang::ast::{Ref, Value};
        if let Value::Ref(Ref::Ns(ns_ref)) = &ast_value
            && let Some(function_name) = &ns_ref.function_name
        {
            let fully_qualified_target = format!("{}/{}", ns_ref.ns, function_name);
            return Box::pin(self.get_hover_for_symbol_with_context(
                &fully_qualified_target,
                _uri,
                file_content,
                cursor_line,
                cursor_col,
            ))
            .await;
        }

        // Build fully qualified name
        let fully_qualified = format!("{}/{}", resolved_namespace, resolved_name);

        // Now build the content without holding the lock

        // Check if this is a type definition
        if let Value::TypeDef(type_def) = &ast_value {
            // For types, show comprehensive type information
            let content = self
                .format_type_hover(&fully_qualified, type_def, &doc_markdown, &var_meta)
                .await;
            return Some(content);
        }

        // For functions and other values
        let mut content = format!("### {}\n", fully_qualified);

        if let Some(detail) = &detail {
            content.push_str(&format!("```hot\n{}\n```\n\n", detail));
        }

        if let Some(doc) = &doc_markdown {
            content.push_str(doc);
            content.push_str("\n\n");
        }

        // Extract type information directly from AST (no string parsing!)
        let type_info = self.extract_type_info_from_ast(&ast_value).await;
        if !type_info.is_empty() {
            content.push_str(&format!("#### Argument Types\n\n{}\n\n", type_info));
        }

        // Add metadata section (excluding "doc")
        if let Some(meta) = &var_meta {
            let meta_section = self.format_meta_section(meta);
            if !meta_section.is_empty() {
                content.push_str(&meta_section);
            }
        }

        Some(content)
    }

    /// Get hover information for a symbol using proper Hot variable resolution (legacy method)
    async fn get_hover_for_symbol(&self, symbol: &str) -> Option<String> {
        let symbol_index = self.symbol_index.lock().await;
        if let Some(index) = &*symbol_index {
            // Use Hot's variable resolution logic
            if let Some((resolved_namespace, resolved_name, var_info)) =
                self.resolve_symbol(symbol, index).await
            {
                let mut content = format!("### {}\n", resolved_name);
                if let Some(detail) = &var_info.detail {
                    content.push_str(&format!("```hot\n{}\n```\n\n", detail));
                }
                if let Some(doc) = &var_info.doc_markdown {
                    content.push_str(doc);
                }

                // Indicate if this is a core function
                if self.is_core_function(&resolved_namespace, &resolved_name, index) {
                    content.push_str(&format!(
                        "\n\n*Core function from namespace: {}*",
                        resolved_namespace
                    ));
                } else {
                    content.push_str(&format!("\n\n*From namespace: {}*", resolved_namespace));
                }

                return Some(content);
            }
        }
        None
    }

    /// Add the current file's symbols to the symbol index for hover and completion
    async fn add_current_file_to_symbol_index(&self, program: &hot::lang::ast::Program) {
        let mut symbol_index = self.symbol_index.lock().await;
        if let Some(index) = symbol_index.as_mut() {
            // Add symbols from the current file to the index
            for (ns_path, namespace) in &program.namespaces {
                let ns_string = ns_path.to_string();

                // Add namespace if not already present
                if !index.namespaces.contains(&ns_string) {
                    index.namespaces.push(ns_string.clone());
                }

                // Extract variables/functions from this namespace
                let mut vars = Vec::new();
                for (var, var_value) in &namespace.scope.vars {
                    let var_name_str = var.sym.name();

                    // Extract function signature and documentation from metadata
                    let (detail, doc_markdown) = self.extract_var_metadata(var, var_value);

                    vars.push(SymbolVar {
                        name: var_name_str.to_string(),
                        kind: if matches!(var_value, hot::lang::ast::Value::Fn(_)) {
                            CompletionItemKind::FUNCTION
                        } else {
                            CompletionItemKind::VARIABLE
                        },
                        detail,
                        doc_markdown,
                        ast_value: var_value.clone(),
                        def_source: var.src.clone(),
                    });
                }

                // Only update if we have variables to add
                // This replaces any existing variables in this namespace from the current file
                if !vars.is_empty() {
                    index.ns_to_vars.insert(ns_string, vars);
                }
            }
        }
    }

    /// Extract the namespace declaration from a Hot file
    fn extract_namespace_from_file(&self, file_content: &str) -> String {
        for line in file_content.lines() {
            let line = line.trim();
            // New syntax: ::path ns or ::path meta {"doc": ...} ns
            if line.starts_with("::") && (line.ends_with(" ns") || line.contains("#")) {
                // Extract namespace like "::test ns" -> "::test"
                // or "::test meta {doc: "..."} ns" -> "::test"
                let ns_end = if let Some(meta_pos) = line.find('#') {
                    meta_pos
                } else if let Some(ns_pos) = line.rfind(" ns") {
                    ns_pos
                } else {
                    continue;
                };
                let ns = line[..ns_end].trim();
                if ns.starts_with("::") {
                    return ns.to_string();
                }
            }
        }
        // Default to root namespace if no ns declaration found
        "::".to_string()
    }

    /// Extract namespace aliases from file content
    /// Aliases are declared as: `::alias ::source::namespace`
    fn extract_namespace_aliases(&self, file_content: &str) -> AHashMap<String, String> {
        let mut aliases = AHashMap::new();

        for line in file_content.lines() {
            let line = line.trim();
            // Skip comments and empty lines
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            // Skip ns declarations (::path ns or ::path meta {"doc": ...} ns)
            if line.starts_with("::") && (line.ends_with(" ns") || line.contains("#")) {
                continue;
            }
            // Look for alias pattern: ::alias ::source::path
            // Must start with :: and have two namespace paths separated by whitespace
            if line.starts_with("::") {
                // Split by whitespace
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() == 2
                    && parts[0].starts_with("::")
                    && parts[1].starts_with("::")
                    && !parts[0].contains('/')
                    && !parts[1].contains('/')
                {
                    // This looks like an alias: ::alias ::source::path
                    aliases.insert(parts[0].to_string(), parts[1].to_string());
                }
            }
        }

        aliases
    }

    /// Resolve a namespace alias to its full path
    /// If the namespace starts with an alias, replace it with the source path
    fn resolve_namespace_alias(
        &self,
        namespace: &str,
        aliases: &AHashMap<String, String>,
    ) -> String {
        // Check if the namespace matches an alias directly
        if let Some(source) = aliases.get(namespace) {
            return source.clone();
        }
        // No alias found, return original
        namespace.to_string()
    }

    /// Extract var imports from file content.
    /// Var imports have the form: `name ::namespace/function`
    /// Returns a map from local name to fully qualified target.
    fn extract_var_imports(&self, file_content: &str) -> AHashMap<String, String> {
        let mut imports = AHashMap::new();

        for line in file_content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            // Skip ns declarations
            if line.starts_with("::") {
                continue;
            }
            // Var import pattern: `identifier ::namespace/function`
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() == 2
                && !parts[0].starts_with("::")
                && parts[1].starts_with("::")
                && parts[1].contains('/')
            {
                // Looks like a var import
                let local_name = parts[0];
                let target = parts[1];
                // Only treat as import if local_name is a simple identifier
                if local_name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    imports.insert(local_name.to_string(), target.to_string());
                }
            }
        }

        imports
    }

    /// Collect names visible in lexical scope at the given cursor position.
    /// Uses the AST Program to find the enclosing function and extract parameter
    /// names plus local variable bindings before the cursor.
    async fn collect_lexical_scope_at_position(
        &self,
        file_content: &str,
        cursor_line: u32,
        _cursor_col: u32,
        current_namespace: &str,
    ) -> Vec<String> {
        let mut scope_names = Vec::new();

        let program_guard = self.program.lock().await;
        let program = match program_guard.as_ref() {
            Some(p) => p,
            None => return scope_names,
        };

        // Find namespace matching the current file
        for (ns_path, namespace) in &program.namespaces {
            let ns_string = ns_path.to_string();
            if ns_string != current_namespace {
                continue;
            }

            // Check each function in this namespace
            for (var, value) in &namespace.scope.vars {
                let fn_defs = match value {
                    hot::lang::ast::Value::Fn(defs) => defs,
                    _ => continue,
                };

                let var_src = match &var.src {
                    Some(src) => src,
                    None => continue,
                };

                // Source lines in the AST are 1-based; cursor_line is 0-based
                let def_line_0based = var_src.line.saturating_sub(1);

                if (cursor_line as usize) < def_line_0based {
                    continue;
                }

                if let Some((body_start, body_end)) =
                    find_function_body_range(file_content, def_line_0based)
                    && (cursor_line as usize) >= body_start
                    && (cursor_line as usize) <= body_end
                {
                    for fn_def in fn_defs {
                        for arg in &fn_def.args.args {
                            let name = arg.var.sym.name().to_string();
                            if !scope_names.contains(&name) {
                                scope_names.push(name);
                            }
                        }

                        collect_local_bindings_from_ast(
                            &fn_def.body,
                            cursor_line,
                            &mut scope_names,
                        );
                    }
                }
            }
        }

        scope_names
    }

    /// Resolve a symbol with proper lexical scoping context.
    ///
    /// Resolution order (matches VM's unified_function_lookup / unified_variable_lookup):
    /// 1. Fully qualified names (::namespace/function)
    /// 2. Lexical scope (function params + local bindings at cursor position)
    /// 3. Current namespace variables
    /// 4. Core functions/variables (auto-imported with core: true)
    /// 5. Imported namespace functions (via aliases and var imports)
    /// 6. Deterministic fallback (prefer ::hot:: namespaces)
    async fn resolve_symbol_with_context<'a>(
        &self,
        symbol: &str,
        index: &'a SymbolIndex,
        current_namespace: &str,
        file_content: &str,
        cursor_line: u32,
        cursor_col: u32,
    ) -> Option<(String, String, &'a SymbolVar)> {
        let aliases = self.extract_namespace_aliases(file_content);

        // 1. FULLY QUALIFIED NAMES - Direct namespace lookup (highest priority)
        if symbol.contains('/')
            && symbol.starts_with("::")
            && let Some((namespace, function_name)) = symbol.rsplit_once('/')
        {
            let resolved_namespace = self.resolve_namespace_alias(namespace, &aliases);

            if let Some(vars) = index.ns_to_vars.get(&resolved_namespace) {
                for var in vars {
                    if var.name == function_name {
                        return Some((resolved_namespace.clone(), function_name.to_string(), var));
                    }
                }
            }

            if resolved_namespace != namespace
                && let Some(vars) = index.ns_to_vars.get(namespace)
            {
                for var in vars {
                    if var.name == function_name {
                        return Some((namespace.to_string(), function_name.to_string(), var));
                    }
                }
            }
        }

        // 2. LEXICAL SCOPE - Function params and local bindings at cursor position
        let lexical_names = self
            .collect_lexical_scope_at_position(
                file_content,
                cursor_line,
                cursor_col,
                current_namespace,
            )
            .await;

        if lexical_names.contains(&symbol.to_string()) {
            // Symbol is a local binding — check if it's also defined in the current namespace
            // (the namespace-level definition is what the index tracks)
            if let Some(vars) = index.ns_to_vars.get(current_namespace) {
                for var in vars {
                    if var.name == symbol {
                        return Some((current_namespace.to_string(), symbol.to_string(), var));
                    }
                }
            }
            // It's a function parameter or local binding without an index entry;
            // return None so the caller can show "local variable" info instead
            // of incorrectly resolving to a different namespace's symbol.
            return None;
        }

        // 3. CURRENT NAMESPACE - Check namespace-level definitions
        if let Some(vars) = index.ns_to_vars.get(current_namespace) {
            for var in vars {
                if var.name == symbol {
                    return Some((current_namespace.to_string(), symbol.to_string(), var));
                }
            }
        }

        // 4. CORE FUNCTIONS - Auto-imported functions/variables (core: true metadata)
        for var in &index.core_vars {
            if var.name == symbol {
                // Find which namespace this core var belongs to (prefer ::hot:: namespaces)
                for (ns, vars) in &index.ns_to_vars {
                    for v in vars {
                        if v.name == symbol && ns.starts_with("::hot::") {
                            return Some((ns.clone(), symbol.to_string(), v));
                        }
                    }
                }
                // Core var found but no ::hot:: namespace match — use the core_vars entry info
                // Try all namespaces for a match
                for (ns, vars) in &index.ns_to_vars {
                    for v in vars {
                        if v.name == symbol {
                            return Some((ns.clone(), symbol.to_string(), v));
                        }
                    }
                }
            }
        }

        // 5. IMPORTED NAMESPACES - Check functions available through namespace aliases
        let var_imports = self.extract_var_imports(file_content);

        // 5a. Var imports: `sha256 ::hot::hash/sha256` makes `sha256` resolve to that target
        if let Some(target) = var_imports.get(symbol)
            && let Some((namespace, function_name)) = target.rsplit_once('/')
        {
            let resolved_ns = self.resolve_namespace_alias(namespace, &aliases);
            if let Some(vars) = index.ns_to_vars.get(&resolved_ns) {
                for var in vars {
                    if var.name == function_name {
                        return Some((resolved_ns.clone(), function_name.to_string(), var));
                    }
                }
            }
        }

        // 5b. Namespace aliases: check if this symbol exists in any aliased namespace
        for (_alias, source_ns) in &aliases {
            if let Some(vars) = index.ns_to_vars.get(source_ns) {
                for var in vars {
                    if var.name == symbol {
                        return Some((source_ns.clone(), symbol.to_string(), var));
                    }
                }
            }
        }

        // No fallback: if the symbol wasn't found through the proper resolution chain
        // (qualified, lexical, namespace, core, imports), it genuinely can't be resolved.
        // This matches the VM behavior which never scans all namespaces blindly.
        None
    }

    /// Resolve a symbol using Hot's UnifiedResolver (same logic as compiler/VM)
    /// Returns (namespace, function_name, var_info) if found
    async fn resolve_symbol<'a>(
        &self,
        symbol: &str,
        index: &'a SymbolIndex,
    ) -> Option<(String, String, &'a SymbolVar)> {
        // Get the UnifiedResolver
        let resolver_guard = self.unified_resolver.lock().await;
        if let Some(resolver) = resolver_guard.as_ref() {
            // Use UnifiedResolver to check if the symbol can be resolved
            let can_resolve_var = resolver.can_resolve_variable(symbol);
            let can_resolve_func = resolver.can_resolve_function(symbol, None);

            if can_resolve_var || can_resolve_func {
                // Get detailed resolution info for debugging
                let resolution_info = resolver.get_resolution_info(symbol);

                // Now find the symbol in our index to get the metadata
                return self
                    .find_symbol_in_index(symbol, index, &resolution_info)
                    .await;
            }
        }

        // Fallback to simple index lookup if UnifiedResolver is not available
        self.fallback_symbol_lookup(symbol, index).await
    }

    /// Find a symbol in the symbol index based on resolution info
    async fn find_symbol_in_index<'a>(
        &self,
        symbol: &str,
        index: &'a SymbolIndex,
        resolution_info: &hot::lang::runtime::resolution::ResolutionInfo,
    ) -> Option<(String, String, &'a SymbolVar)> {
        // 1. FULLY QUALIFIED NAMES - Direct namespace lookup
        if symbol.contains('/')
            && symbol.starts_with("::")
            && let Some((namespace, function_name)) = symbol.rsplit_once('/')
            && let Some(vars) = index.ns_to_vars.get(namespace)
        {
            for var in vars {
                if var.name == function_name {
                    return Some((namespace.to_string(), function_name.to_string(), var));
                }
            }
        }

        // 2. CORE FUNCTIONS - Prioritize core functions for unqualified names
        if resolution_info.found_as_core {
            for (ns, vars) in &index.ns_to_vars {
                for var in vars {
                    if var.name == symbol && ns.starts_with("::hot::") {
                        return Some((ns.clone(), symbol.to_string(), var));
                    }
                }
            }
        }

        // 3. OTHER NAMESPACES - Check all namespaces for the function
        for (ns, vars) in &index.ns_to_vars {
            for var in vars {
                if var.name == symbol {
                    return Some((ns.clone(), symbol.to_string(), var));
                }
            }
        }

        None
    }

    /// Fallback symbol lookup when UnifiedResolver is not available
    async fn fallback_symbol_lookup<'a>(
        &self,
        symbol: &str,
        index: &'a SymbolIndex,
    ) -> Option<(String, String, &'a SymbolVar)> {
        // 1. FULLY QUALIFIED NAMES - Direct namespace lookup
        if symbol.contains('/')
            && symbol.starts_with("::")
            && let Some((namespace, function_name)) = symbol.rsplit_once('/')
            && let Some(vars) = index.ns_to_vars.get(namespace)
        {
            for var in vars {
                if var.name == function_name {
                    return Some((namespace.to_string(), function_name.to_string(), var));
                }
            }
        }

        // 2. CORE FUNCTIONS - Check for core functions across all namespaces
        for (ns, vars) in &index.ns_to_vars {
            for var in vars {
                if var.name == symbol && self.is_core_function(ns, &var.name, index) {
                    return Some((ns.clone(), symbol.to_string(), var));
                }
            }
        }

        // 3. OTHER NAMESPACES - Check all namespaces for the function
        for (ns, vars) in &index.ns_to_vars {
            for var in vars {
                if var.name == symbol {
                    return Some((ns.clone(), symbol.to_string(), var));
                }
            }
        }

        None
    }

    /// Check if a function is marked as core (auto-imported)
    fn is_core_function(
        &self,
        namespace: &str,
        _function_name: &str,
        _index: &SymbolIndex,
    ) -> bool {
        // For now, consider functions in ::hot:: namespaces as core.
        // Parsed-AST metadata can make this more precise later.
        namespace.starts_with("::hot::")
    }

    /// Check if a variable is marked as core based on its metadata
    /// Supports multiple metadata formats:
    /// - meta ["core"] - vector containing "core" string
    /// - #"core" - direct "core" string
    /// - meta {core: true} - map with core key set to true
    fn is_core_variable_from_metadata(&self, var: &hot::lang::ast::Var) -> bool {
        if let Some(meta) = &var.meta {
            match &meta.val {
                // Check for meta ["core"] - vector containing "core" string
                hot::val::Val::Vec(tags) => tags.iter().any(|tag| {
                    if let hot::val::Val::Str(tag_str) = tag {
                        &**tag_str == "core"
                    } else {
                        false
                    }
                }),
                // Check for #"core" - direct "core" string
                hot::val::Val::Str(tag_str) => &**tag_str == "core",
                // Check for meta {core: true} - map with core key set to true
                hot::val::Val::Map(map) => {
                    if let Some(core_val) = map.get(&hot::val::Val::from("core")) {
                        matches!(core_val, hot::val::Val::Bool(true))
                    } else {
                        false
                    }
                }
                _ => false,
            }
        } else {
            false
        }
    }

    /// Check if a Value is a TypeDef with core metadata (on the var)
    fn is_core_type_def(&self, var: &hot::lang::ast::Var, value: &hot::lang::ast::Value) -> bool {
        if !matches!(value, hot::lang::ast::Value::TypeDef(_)) {
            return false;
        }
        if let Some(meta) = &var.meta {
            match &meta.val {
                hot::val::Val::Vec(tags) => tags.iter().any(|tag| {
                    if let hot::val::Val::Str(tag_str) = tag {
                        tag_str.as_ref() == "core"
                    } else {
                        false
                    }
                }),
                hot::val::Val::Str(tag_str) => tag_str.as_ref() == "core",
                hot::val::Val::Map(map) => {
                    if let Some(core_val) = map.get(&hot::val::Val::from("core")) {
                        matches!(core_val, hot::val::Val::Bool(true))
                    } else {
                        false
                    }
                }
                _ => false,
            }
        } else {
            false
        }
    }

    /// Extract signature information from hover content for signature help
    fn extract_signature_from_hover(&self, hover_content: &str) -> Option<SignatureInformation> {
        // Look for function signature in code blocks
        for line in hover_content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("fn(") || trimmed.contains(" fn(") {
                // Extract the function signature
                let signature = trimmed.to_string();

                // Parse parameters from the signature
                let mut parameters = Vec::new();
                if let Some(params_start) = signature.find('(')
                    && let Some(params_end) = signature.find(')')
                {
                    let params_str = &signature[params_start + 1..params_end];
                    if !params_str.trim().is_empty() {
                        for param in params_str.split(',') {
                            let param_trimmed = param.trim();
                            if !param_trimmed.is_empty() {
                                parameters.push(ParameterInformation {
                                    label: ParameterLabel::Simple(param_trimmed.to_string()),
                                    documentation: None,
                                });
                            }
                        }
                    }
                }

                return Some(SignatureInformation {
                    label: signature,
                    documentation: None,
                    parameters: Some(parameters),
                    active_parameter: None,
                });
            }
        }

        None
    }

    /// Get all function signatures for a symbol (all arities)
    /// This handles both regular functions and type constructors
    async fn get_all_function_signatures(&self, symbol: &str) -> Option<Vec<SignatureInformation>> {
        let mut signatures = Vec::new();

        // Try to find the function definition from the cached program
        // Direct lookup from parsed Hot files
        if let Some(fn_defs) = self.find_function_defs(symbol).await {
            // Build signature information for each arity
            for fn_def in fn_defs {
                let args: Vec<String> = fn_def
                    .args
                    .args
                    .iter()
                    .map(|arg| {
                        let name = arg.var.sym.name();
                        let lazy_prefix = if arg.lazy { "lazy " } else { "" };
                        if let Some(type_annotation) = &arg.type_annotation {
                            let formatted_type = self.format_type_annotation(type_annotation);
                            format!("{}{}: {}", lazy_prefix, name, formatted_type)
                        } else {
                            format!("{}{}", lazy_prefix, name)
                        }
                    })
                    .collect();

                let signature_label = format!("fn({})", args.join(", "));

                let parameters: Vec<ParameterInformation> = args
                    .iter()
                    .map(|arg| ParameterInformation {
                        label: ParameterLabel::Simple(arg.clone()),
                        documentation: None,
                    })
                    .collect();

                signatures.push(SignatureInformation {
                    label: signature_label,
                    documentation: None,
                    parameters: Some(parameters),
                    active_parameter: None,
                });
            }
        }

        // Also check for TypeDef (type constructors)
        if let Some(type_signatures) = self.find_type_constructor_signatures(symbol).await {
            signatures.extend(type_signatures);
        }

        if !signatures.is_empty() {
            Some(signatures)
        } else {
            None
        }
    }

    /// Find type constructor signatures for a symbol
    /// Returns signatures for struct-style types and their custom constructors
    async fn find_type_constructor_signatures(
        &self,
        symbol: &str,
    ) -> Option<Vec<SignatureInformation>> {
        // First, check the symbol index for TypeDefs
        let symbol_index = self.symbol_index.lock().await;
        if let Some(index) = &*symbol_index {
            // Check core_vars first
            for var in &index.core_vars {
                if var.name == symbol
                    && let hot::lang::ast::Value::TypeDef(type_def) = &var.ast_value
                {
                    return Some(self.type_def_to_signatures(type_def));
                }
            }

            // Check all namespaces
            for vars in index.ns_to_vars.values() {
                for var in vars {
                    if var.name == symbol
                        && let hot::lang::ast::Value::TypeDef(type_def) = &var.ast_value
                    {
                        return Some(self.type_def_to_signatures(type_def));
                    }
                }
            }
        }

        None
    }

    /// Convert a TypeDef to signature information
    fn type_def_to_signatures(
        &self,
        type_def: &hot::lang::ast::TypeDef,
    ) -> Vec<SignatureInformation> {
        let mut signatures = Vec::new();
        let type_name = type_def.name.name();

        // If the type has fields, show the struct constructor signature
        if let Some(fields) = &type_def.fields {
            let field_strs: Vec<String> = fields
                .iter()
                .map(|f| format!("{}: {}", f.name.name(), f.type_annotation))
                .collect();

            let signature_label = format!("{}({{ {} }})", type_name, field_strs.join(", "));

            // Create parameter info for each field
            let parameters: Vec<ParameterInformation> = fields
                .iter()
                .map(|f| ParameterInformation {
                    label: ParameterLabel::Simple(format!(
                        "{}: {}",
                        f.name.name(),
                        f.type_annotation
                    )),
                    documentation: None,
                })
                .collect();

            signatures.push(SignatureInformation {
                label: signature_label,
                documentation: Some(Documentation::String(
                    "Struct constructor - pass a map with the required fields".to_string(),
                )),
                parameters: Some(parameters),
                active_parameter: None,
            });
        }

        // If the type has custom constructor functions, include those too
        if let Some(constructors) = &type_def.constructor_functions {
            for fn_def in constructors {
                let args: Vec<String> = fn_def
                    .args
                    .args
                    .iter()
                    .map(|arg| {
                        let name = arg.var.sym.name();
                        let lazy_prefix = if arg.lazy { "lazy " } else { "" };
                        if let Some(type_annotation) = &arg.type_annotation {
                            let formatted_type = self.format_type_annotation(type_annotation);
                            format!("{}{}: {}", lazy_prefix, name, formatted_type)
                        } else {
                            format!("{}{}", lazy_prefix, name)
                        }
                    })
                    .collect();

                let signature_label = format!("{}({})", type_name, args.join(", "));

                let parameters: Vec<ParameterInformation> = args
                    .iter()
                    .map(|arg| ParameterInformation {
                        label: ParameterLabel::Simple(arg.clone()),
                        documentation: None,
                    })
                    .collect();

                signatures.push(SignatureInformation {
                    label: signature_label,
                    documentation: Some(Documentation::String("Custom constructor".to_string())),
                    parameters: Some(parameters),
                    active_parameter: None,
                });
            }
        }

        // If no fields or constructors but it's a type alias or variant, show basic signature
        if signatures.is_empty() {
            if type_def.type_alias.is_some() {
                signatures.push(SignatureInformation {
                    label: format!("{}(value)", type_name),
                    documentation: Some(Documentation::String(
                        "Type alias constructor".to_string(),
                    )),
                    parameters: Some(vec![ParameterInformation {
                        label: ParameterLabel::Simple("value".to_string()),
                        documentation: None,
                    }]),
                    active_parameter: None,
                });
            } else if type_def.variants.is_some() {
                signatures.push(SignatureInformation {
                    label: format!("{}(variant)", type_name),
                    documentation: Some(Documentation::String(
                        "Variant type - use a variant constructor".to_string(),
                    )),
                    parameters: Some(vec![ParameterInformation {
                        label: ParameterLabel::Simple("variant".to_string()),
                        documentation: None,
                    }]),
                    active_parameter: None,
                });
            }
        }

        signatures
    }

    /// Find function definitions for a symbol (searches all namespaces)
    async fn find_function_defs(&self, symbol: &str) -> Option<Vec<hot::lang::ast::FnDef>> {
        // Try to parse Hot files to get the actual function definitions
        if let Ok(program) = self.discover_and_parse_hot_files().await {
            // First, check if it's a fully qualified name
            if symbol.starts_with("::") {
                // Parse namespace and variable name
                let parts: Vec<&str> = symbol.rsplitn(2, '/').collect();
                if parts.len() == 2 {
                    let var_name = parts[0];
                    let namespace = parts[1];

                    return self.find_function_in_namespace(&program, namespace, var_name);
                }
            } else {
                // Search through all namespaces for this symbol
                // First check core variables
                for (_ns_path, ns) in &program.namespaces {
                    for (var, value) in &ns.scope.vars {
                        if var.sym.name() == symbol {
                            // Check if it's a core variable
                            if self.is_core_variable_from_metadata(var)
                                && let hot::lang::ast::Value::Fn(fn_defs) = value
                            {
                                return Some(fn_defs.clone());
                            }
                        }
                    }
                }

                // If not found as core, search all namespaces
                for (_ns_path, ns) in &program.namespaces {
                    for (var, value) in &ns.scope.vars {
                        if var.sym.name() == symbol
                            && let hot::lang::ast::Value::Fn(fn_defs) = value
                        {
                            return Some(fn_defs.clone());
                        }
                    }
                }
            }
        }

        None
    }

    /// Find function in a specific namespace
    fn find_function_in_namespace(
        &self,
        program: &hot::lang::ast::Program,
        namespace: &str,
        var_name: &str,
    ) -> Option<Vec<hot::lang::ast::FnDef>> {
        for (ns_path, ns) in &program.namespaces {
            if ns_path.to_string() == namespace {
                for (var, value) in &ns.scope.vars {
                    if var.sym.name() == var_name
                        && let hot::lang::ast::Value::Fn(fn_defs) = value
                    {
                        return Some(fn_defs.clone());
                    }
                }
            }
        }
        None
    }

    /// Evaluate Hot code using a stateful REPL session
    pub async fn eval_code(&self, params: EvalParams) -> EvalResult {
        // Get or create the REPL session
        let mut session_guard = self.repl_session.lock().await;

        if session_guard.is_none() {
            // Initialize the REPL session with project configuration
            let conf = self.conf.lock().await.clone();
            let src_paths = self.src_paths.lock().await.clone();

            // Get project name for dependency resolution (hot-std, etc.)
            let project_name = hot::project::get_default_project_name(&conf);

            // Get merged src paths including project paths
            let merged_src_paths: Vec<String> = {
                let mut paths = hot::project::get_project_src_paths(&conf, &project_name);
                paths.extend(
                    src_paths
                        .iter()
                        .filter_map(|p| p.to_str().map(String::from)),
                );
                paths
            };

            // Get test paths
            let test_paths = hot::project::get_project_test_paths(&conf, &project_name);

            let repl_config = ReplConfig {
                src_paths: merged_src_paths,
                test_paths,
                conf: Some(conf),
                project_name: Some(project_name),
                context_storage: None,
            };

            *session_guard = Some(ReplSession::new(repl_config));
        }

        let session = session_guard.as_mut().unwrap();

        // If a namespace was specified, switch to it first
        if let Some(ns) = &params.namespace
            && !ns.is_empty()
            && ns != session.current_namespace()
        {
            // Switch namespace by evaluating a namespace directive (e.g., "::my::namespace ns")
            let _ = session.eval(&format!("{} ns", ns));
        }

        // Start capturing stdout to prevent corrupting LSP protocol
        hot::lang::hot::io::start_capture_stdout();

        // Evaluate the code
        let eval_result = session.eval(&params.code);

        // Stop capturing and get any output
        let captured_stdout = hot::lang::hot::io::release_capture_stdout();

        match eval_result {
            Ok(result) => {
                let result_str = format!("{}", result);
                EvalResult {
                    success: true,
                    result: result_str,
                    error: None,
                    namespace: session.current_namespace().to_string(),
                    stdout: captured_stdout,
                }
            }
            Err(e) => EvalResult {
                success: false,
                result: String::new(),
                error: Some(e),
                namespace: session.current_namespace().to_string(),
                stdout: captured_stdout,
            },
        }
    }

    /// Reset the REPL session (clear all state)
    pub async fn reset_repl_session(&self) {
        let mut session_guard = self.repl_session.lock().await;
        *session_guard = None;
    }

    /// Handle the hot/eval custom LSP request
    async fn handle_eval(&self, params: EvalParams) -> JsonRpcResult<EvalResult> {
        Ok(self.eval_code(params).await)
    }

    /// Handle the hot/resetRepl custom LSP request
    async fn handle_reset_repl(&self, _params: ()) -> JsonRpcResult<()> {
        self.reset_repl_session().await;
        Ok(())
    }
}

// Context structures for completion
struct NamespaceContext {
    prefix_segments: Vec<String>,
}

struct VariableContext {
    namespace: String,
}

/// Context for struct field completion (e.g., "Person({" or "User({name:")
struct TypeFieldContext {
    type_name: String,
}

/// Extract namespace completion context from prefix (e.g., "::hot::" or "::hot::math")
fn extract_namespace_context(prefix: &str) -> Option<NamespaceContext> {
    // Check if this contains a '/' which indicates variable context, not namespace
    if prefix.contains('/') {
        return None;
    }

    // Must start with "::" to be a namespace
    if !prefix.starts_with("::") {
        return None;
    }

    // Remove the leading "::" and split by "::"
    let without_prefix = &prefix[2..];
    let segments: Vec<String> = if without_prefix.is_empty() {
        Vec::new()
    } else {
        without_prefix.split("::").map(|s| s.to_string()).collect()
    };

    Some(NamespaceContext {
        prefix_segments: segments,
    })
}

/// Extract variable completion context from prefix (e.g., "::hot/")
fn extract_variable_context(prefix: &str) -> Option<VariableContext> {
    if let Some(slash_pos) = prefix.rfind('/') {
        let before = &prefix[..slash_pos];
        if let Some(chain_start) = before.find("::") {
            let ns = &before[chain_start..];
            if !ns.is_empty() {
                return Some(VariableContext {
                    namespace: ns.to_string(),
                });
            }
        }
    }
    None
}

/// Extract type field completion context from prefix
/// Matches patterns like "Person({" or "User({name: 1, " for struct field completion
fn extract_type_field_context(prefix: &str) -> Option<TypeFieldContext> {
    // Look for pattern: TypeName({ or TypeName({field: value,
    // We need to find an opening brace that's preceded by a type constructor call

    // Find the last unmatched opening brace
    let mut brace_depth = 0;
    let mut last_open_brace_pos = None;

    for (i, ch) in prefix.char_indices().rev() {
        match ch {
            '}' => brace_depth += 1,
            '{' => {
                if brace_depth == 0 {
                    last_open_brace_pos = Some(i);
                    break;
                }
                brace_depth -= 1;
            }
            _ => {}
        }
    }

    let open_brace_pos = last_open_brace_pos?;

    // Check if it's preceded by ( which indicates a type constructor call
    let before_brace = prefix[..open_brace_pos].trim_end();
    if !before_brace.ends_with('(') {
        return None;
    }

    // Extract the type name before the (
    let before_paren = &before_brace[..before_brace.len() - 1].trim_end();

    // Extract identifier going backwards (type names start with uppercase)
    let mut type_name = String::new();
    for ch in before_paren.chars().rev() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            type_name.insert(0, ch);
        } else if ch == '/' || ch == ':' {
            // Handle namespaced types like ::myapp/Person
            type_name.insert(0, ch);
        } else {
            break;
        }
    }

    // Type names typically start with uppercase
    if type_name.is_empty() {
        return None;
    }

    // Get the base type name (after any namespace prefix)
    let base_name = if let Some(slash_pos) = type_name.rfind('/') {
        &type_name[slash_pos + 1..]
    } else {
        &type_name
    };

    // Check if it starts with uppercase (type convention)
    if base_name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
    {
        Some(TypeFieldContext { type_name })
    } else {
        None
    }
}

/// Extract a bare identifier prefix for core function completion
/// Returns the word being typed if it's a simple identifier (not namespaced)
fn extract_bare_identifier_prefix(prefix: &str) -> Option<String> {
    // Don't suggest core completions if we're in a namespace context
    if prefix.contains("::") {
        return None;
    }

    // Don't suggest if we're inside a brace (that's struct field territory)
    let mut brace_depth: i32 = 0;
    for ch in prefix.chars() {
        match ch {
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
    }
    if brace_depth > 0 {
        return None;
    }

    // Extract the current word being typed
    let mut word = String::new();
    for ch in prefix.chars().rev() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            word.insert(0, ch);
        } else {
            break;
        }
    }

    if word.is_empty() { None } else { Some(word) }
}

/// Extract symbol at cursor position
fn extract_symbol_at_position(left: &str, right: &str) -> Option<String> {
    // Enhanced symbol extraction for Hot language - handles namespaced identifiers
    let mut symbol = String::new();

    // Collect characters to the left, including Hot namespace syntax
    for ch in left.chars().rev() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == ':' || ch == '/' {
            symbol.insert(0, ch);
        } else {
            break;
        }
    }

    // Collect characters to the right, including Hot namespace syntax
    for ch in right.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == ':' || ch == '/' {
            symbol.push(ch);
        } else {
            break;
        }
    }

    // Clean up the symbol - ensure it's a valid Hot identifier
    if symbol.is_empty() {
        return None;
    }

    // If we have a partial namespace, try to extract the full qualified name
    // This handles cases where cursor is on part of "::hot::math/add"
    if symbol.contains(':') || symbol.contains('/') {
        // Return the full qualified name as-is
        Some(symbol)
    } else {
        // For simple identifiers, return as-is
        Some(symbol)
    }
}

fn active_parameter_from_call_args(left: &str) -> u32 {
    let Some(paren_pos) = left.rfind('(') else {
        return 0;
    };

    let mut active = 0;
    let mut depth = 0i32;
    let mut in_string: Option<char> = None;
    let mut escaped = false;

    for ch in left[paren_pos + 1..].chars() {
        if let Some(quote) = in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                in_string = None;
            }
            continue;
        }

        match ch {
            '"' | '\'' | '`' => in_string = Some(ch),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => active += 1,
            _ => {}
        }
    }

    active
}

fn source_to_location(src: &hot::lang::ast::Source) -> Option<Location> {
    let file_path = src.file.as_ref()?;
    let uri = Url::from_file_path(file_path).ok()?;
    let line = src.line.saturating_sub(1) as u32;
    let col = src.column.saturating_sub(1) as u32;
    Some(Location::new(
        uri,
        Range::new(
            Position::new(line, col),
            Position::new(line, col + src.length as u32),
        ),
    ))
}

fn location_key(location: &Location) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        location.uri,
        location.range.start.line,
        location.range.start.character,
        location.range.end.line,
        location.range.end.character
    )
}

fn find_symbol_locations(uri: &Url, text: &str, names: &[String]) -> Vec<Location> {
    let mut locations = Vec::new();
    for name in names {
        let mut search_start = 0;
        while let Some(relative_pos) = text[search_start..].find(name) {
            let start = search_start + relative_pos;
            let end = start + name.len();
            if is_symbol_boundary(text, start, end) {
                let (start_line, start_col) = byte_offset_to_position(text, start);
                let (end_line, end_col) = byte_offset_to_position(text, end);
                locations.push(Location::new(
                    uri.clone(),
                    Range::new(
                        Position::new(start_line, start_col),
                        Position::new(end_line, end_col),
                    ),
                ));
            }
            search_start = end;
        }
    }
    locations
}

fn is_symbol_boundary(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    !before.is_some_and(is_hot_symbol_char) && !after.is_some_and(is_hot_symbol_char)
}

fn is_hot_symbol_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == ':' || ch == '/'
}

fn byte_offset_to_position(text: &str, offset: usize) -> (u32, u32) {
    let mut line = 0u32;
    let mut col = 0u32;
    for (idx, ch) in text.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Extract function name when cursor is positioned right after opening parenthesis
/// For example: "add(" -> returns "add" or "::hot::math/add()" with cursor after ( -> returns "::hot::math/add"
fn extract_function_before_paren(left: &str) -> Option<String> {
    // Find the last opening parenthesis in the text
    if let Some(paren_pos) = left.rfind('(') {
        // Get the text before the opening parenthesis
        let before_paren = left[..paren_pos].trim_end();

        // Extract the function name (could be simple name or namespace qualified)
        let mut function_name = String::new();
        let mut collecting_namespace = false;

        // Collect characters from right to left
        for ch in before_paren.chars().rev() {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                function_name.insert(0, ch);
            } else if ch == '/' && !collecting_namespace {
                // This could be a namespace separator like ::hot::math/add
                function_name.insert(0, ch);
                collecting_namespace = true;
            } else if ch == ':' && collecting_namespace {
                // Continue collecting namespace parts
                function_name.insert(0, ch);
            } else if ch.is_ascii_alphabetic() && collecting_namespace {
                // Continue collecting namespace name
                function_name.insert(0, ch);
            } else {
                // Stop at any other character
                break;
            }
        }

        if function_name.is_empty() {
            None
        } else {
            Some(function_name)
        }
    } else {
        None
    }
}

/// Complete namespaces based on context
fn complete_namespaces(
    index: &SymbolIndex,
    ctx: &NamespaceContext,
    _position: Position,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut seen = AHashSet::new();

    // Build the current prefix as a namespace path
    let current_prefix = if ctx.prefix_segments.is_empty() {
        "::".to_string()
    } else {
        format!("::{}", ctx.prefix_segments.join("::"))
    };

    for ns in &index.namespaces {
        // Only show namespaces that start with the current prefix
        if ns.starts_with(&current_prefix) && ns != &current_prefix {
            // Extract the next segment after the current prefix
            let remaining = &ns[current_prefix.len()..];
            let remaining = remaining.trim_start_matches("::");

            if let Some(next_segment) = remaining.split("::").next()
                && !next_segment.is_empty()
            {
                let next_ns = if current_prefix == "::" {
                    format!("::{}", next_segment)
                } else {
                    format!("{}::{}", current_prefix, next_segment)
                };

                // Only add each namespace once
                if seen.insert(next_ns.clone()) {
                    items.push(CompletionItem {
                        label: next_segment.to_string(),
                        kind: Some(CompletionItemKind::MODULE),
                        detail: Some(format!("namespace {}", next_ns)),
                        insert_text: Some(format!("{}::", next_segment)),
                        ..Default::default()
                    });
                }
            }
        }
    }

    items
}

/// Complete variables based on context
fn complete_variables(index: &SymbolIndex, ctx: &VariableContext) -> Vec<CompletionItem> {
    if let Some(vars) = index.ns_to_vars.get(&ctx.namespace) {
        return vars
            .iter()
            .map(|v| CompletionItem {
                label: v.name.clone(),
                kind: Some(v.kind),
                detail: v.detail.clone(),
                documentation: v.doc_markdown.as_ref().map(|md| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: md.clone(),
                    })
                }),
                ..Default::default()
            })
            .collect();
    }
    Vec::new()
}

/// Find the brace-delimited body range of a function/flow starting at or after `def_line`.
/// Returns (body_start_line, body_end_line) using 0-based line numbers.
fn find_function_body_range(file_content: &str, def_line: usize) -> Option<(usize, usize)> {
    let lines: Vec<&str> = file_content.lines().collect();
    let mut brace_depth: i32 = 0;
    let mut body_start: Option<usize> = None;

    for (line_idx, line) in lines.iter().enumerate().skip(def_line) {
        for ch in line.chars() {
            if ch == '{' {
                brace_depth += 1;
                if body_start.is_none() {
                    body_start = Some(line_idx);
                }
            } else if ch == '}' {
                brace_depth -= 1;
                if let Some(start) = body_start
                    && brace_depth == 0
                {
                    return Some((start, line_idx));
                }
            }
        }
    }
    None
}

/// Collect local variable bindings from a flow body AST that appear before the cursor line.
/// Detects the `Ref::Var` + value pair pattern used for assignments in Hot.
fn collect_local_bindings_from_ast(
    body: &hot::lang::ast::Value,
    cursor_line: u32,
    scope_names: &mut Vec<String>,
) {
    use hot::lang::ast::{Ref, Value};

    let flow = match body {
        Value::Flow(f) => f,
        _ => return,
    };

    let mut i = 0;
    while i < flow.expressions.len() {
        let expr = &flow.expressions[i];

        // Detect "name value" assignment pattern: Ref::Var followed by a value expression
        if i + 1 < flow.expressions.len()
            && let Value::Ref(Ref::Var(var_ref)) = expr
        {
            let include = match &var_ref.src {
                Some(src) => (src.line as u32) <= cursor_line,
                None => true,
            };
            if include {
                let name = var_ref.var.sym.name().to_string();
                if !scope_names.contains(&name) {
                    scope_names.push(name);
                }
            }
            i += 2;
            continue;
        }

        // Recurse into nested flows to find named branch bindings (parallel, cond-all)
        if let Value::Flow(nested_flow) = expr {
            for nested_expr in &nested_flow.expressions {
                if let Value::Cond(Some(label), _, inner_flow) = nested_expr
                    && let Some(first_expr) = inner_flow.expressions.first()
                    && let Some(src) = get_value_source(first_expr)
                    && (src.line as u32) <= cursor_line
                    && !scope_names.contains(label)
                {
                    scope_names.push(label.clone());
                }
            }
        }

        i += 1;
    }
}

/// Extract source info from a Value if available.
fn get_value_source(value: &hot::lang::ast::Value) -> Option<&hot::lang::ast::Source> {
    use hot::lang::ast::{Ref, Value};
    match value {
        Value::Ref(Ref::Var(vr)) => vr.src.as_ref(),
        Value::FnCall(fc) => fc.src.as_ref(),
        Value::Flow(f) => f.src.as_ref(),
        _ => None,
    }
}

pub async fn run_stdio() -> anyhow::Result<()> {
    let (stdin, stdout) = (tokio::io::stdin(), tokio::io::stdout());

    // Load configuration
    let mut conf = create_default_conf();
    let in_project = std::path::Path::new("hot.hot").exists();

    if in_project {
        match load_conf_file("hot.hot") {
            Ok(hot_conf) => {
                conf = conf.merge(&hot_conf);
            }
            Err(_e) => {}
        }
    }

    let project_conf = hot::project::get_resolved_conf(conf.clone());
    conf = conf.merge(&project_conf);

    // Apply emitter defaults (LSP is always runtime context for eval)
    let emitter_conf_from_user = conf.get("emitter").unwrap_or_else(Val::map_empty);
    let resolved_emitter_conf =
        hot::lang::emitter::get_resolved_conf(emitter_conf_from_user, in_project);
    conf = conf.set("emitter", resolved_emitter_conf);

    // Apply queue defaults (LSP is always runtime context for eval)
    let queue_conf_from_user = conf.get("queue").unwrap_or_else(Val::map_empty);
    let resolved_queue_conf = hot::queue::get_resolved_conf(queue_conf_from_user, in_project);
    conf = conf.set("queue", resolved_queue_conf);

    let project_name = hot::project::get_default_project_name(&conf);
    let src_paths: Vec<PathBuf> = hot::project::get_project_src_paths(&conf, &project_name)
        .into_iter()
        .map(PathBuf::from)
        .collect();

    let (service, socket) = LspService::build(|client| Backend {
        client,
        conf: std::sync::Arc::new(tokio::sync::Mutex::new(conf.clone())),
        src_paths: std::sync::Arc::new(tokio::sync::Mutex::new(src_paths.clone())),
        docs: tokio::sync::Mutex::new(AHashMap::new()),
        symbol_index: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        unified_resolver: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        program: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        repl_session: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
    })
    .custom_method("hot/eval", Backend::handle_eval)
    .custom_method("hot/resetRepl", Backend::handle_reset_repl)
    .finish();

    Server::new(stdin, stdout, socket).serve(service).await;
    Ok(())
}

// Helper function to create default configuration
fn create_default_conf() -> Val {
    let mut conf = Val::map_empty();
    let cache_conf = hot::cache::get_resolved_conf(Val::map_empty());
    conf = conf.set("cache", cache_conf);
    conf
}

// Helper function to load configuration file by evaluating it as Hot code
fn load_conf_file(conf_path: &str) -> Result<Val, String> {
    let path = Path::new(conf_path);
    if !path.exists() {
        return Err(format!("Configuration file not found: {}", conf_path));
    }

    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            return Err(format!(
                "Failed to read configuration file {}: {}",
                conf_path, e
            ));
        }
    };

    if content.trim().is_empty() {
        return Ok(Val::map_empty());
    }

    let namespaced_content = if content.contains("::hot::conf ns") {
        content
    } else {
        format!("::hot::conf ns\n\n{}", content)
    };

    match hot::lang::engine::Engine::execute_eval_and_retain_state(
        &namespaced_content,
        &[],
        &[],
        None,
        None,
        false,
    ) {
        Ok(executed_engine) => {
            if let Some(hot_var) = executed_engine.extract_var(Some("::hot::conf"), "hot") {
                Ok(hot_var)
            } else {
                Ok(Val::map_empty())
            }
        }
        Err(e) => {
            tracing::warn!("Failed to evaluate config {}: {}", conf_path, e);
            Ok(Val::map_empty())
        }
    }
}
