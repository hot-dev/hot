//! The Hot language server.
//!
//! * Diagnostics come from the same check `hot check --check.format json`
//!   runs, over the whole project with open buffers in place of their files
//!   ([`analysis`]). A check runs shortly after the last edit, and its
//!   results replace the diagnostics of every file, so a file's diagnostics
//!   follow edits to the files it depends on.
//! * Navigation, hover, completion and signature help answer from the
//!   definitions the last successful check parsed ([`index`]) and the tokens
//!   of the document ([`syntax`]), which stay usable while the text doesn't
//!   parse.
//! * Positions are UTF-16 on the wire and converted through [`text`]
//!   before they touch the text.
//! * Every handler runs behind a panic guard: a bug in one request fails
//!   that request instead of the server.

mod analysis;
mod features;
mod index;
mod render;
mod syntax;
mod text;
mod transport;
mod workspace;

use ahash::{AHashMap, AHashSet};
use hot::lang::repl::{ReplConfig, ReplSession};
use index::{DocContext, Index};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::task::Poll;
use std::time::Duration;
use text::Document;
use tokio::io::{AsyncRead, AsyncWrite};
use tower_lsp_server::jsonrpc::{Error as JsonRpcError, Result as JsonRpcResult};
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer, LspService, Server};
use workspace::Workspace;

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

/// How long to wait after an edit before checking, so a burst of edits
/// runs one check.
const EDIT_DEBOUNCE: Duration = Duration::from_millis(200);

/// Run a future, turning a panic inside it into an error message.
async fn catch_panic<T>(fut: impl Future<Output = T>) -> Result<T, String> {
    let mut fut = Box::pin(fut);
    std::future::poll_fn(move |cx| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(panic_message(payload.as_ref()))),
        }
    })
    .await
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_string()
    }
}

/// Read a lock even if a panic poisoned it: the data is plain state that a
/// panicking writer leaves consistent.
fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

/// The path a `file:` URI names.
///
/// Paths and URIs convert through `url`, as they did before `Uri`. `Uri`'s
/// own conversions keep the `\\?\` prefix that `canonicalize` adds on
/// Windows, which gives a URI no client matches, and take a URI of any
/// scheme for a path.
pub(crate) fn file_path(uri: &Uri) -> Option<PathBuf> {
    url::Url::parse(uri.as_str()).ok()?.to_file_path().ok()
}

/// The `file:` URI of an absolute path.
pub(crate) fn file_uri(path: &Path) -> Option<Uri> {
    url::Url::from_file_path(path).ok()?.as_str().parse().ok()
}

/// The canonical path of a document URI.
fn uri_path(uri: &Uri) -> Option<PathBuf> {
    let path = file_path(uri)?;
    Some(std::fs::canonicalize(&path).unwrap_or(path))
}

fn is_hot(uri: &Uri) -> bool {
    uri.path().as_str().ends_with(".hot")
}

/// What the last successful check parsed.
struct Snapshot {
    index: Index,
    /// Project source files, as the check reported them.
    project_files_raw: AHashSet<String>,
    /// Project source files, canonical.
    project_files: Vec<PathBuf>,
}

impl Snapshot {
    fn empty() -> Self {
        Self {
            index: Index::new(hot::lang::ast::Program {
                namespaces: Default::default(),
                current_namespace: hot::lang::ast::NsPath::hot_main(),
            }),
            project_files_raw: AHashSet::new(),
            project_files: Vec::new(),
        }
    }
}

#[derive(Default)]
struct Published {
    /// Diagnostics last published, by URI.
    diagnostics: AHashMap<Uri, Vec<Diagnostic>>,
    /// URIs whose diagnostics came from a check that stopped at parse errors.
    from_parse_failure: AHashSet<Uri>,
    /// Window messages last shown, so a repeated one isn't shown again.
    messages: Vec<String>,
}

struct State {
    client: Client,
    /// Open documents.
    docs: RwLock<AHashMap<Uri, Arc<Document>>>,
    workspace: RwLock<Arc<Workspace>>,
    snapshot: RwLock<Arc<Snapshot>>,
    published: tokio::sync::Mutex<Published>,
    /// Bumped by every change; a scheduled check runs only if no later
    /// change superseded it.
    generation: AtomicU64,
    /// Held while a check or a workspace reload runs.
    analysis: tokio::sync::Mutex<()>,
    /// Whether the client can register file watchers.
    dynamic_watchers: AtomicBool,
    /// Whether the client sent `shutdown` (then `exit` is a clean exit).
    shutdown: Arc<AtomicBool>,
    /// REPL session for stateful evaluation
    repl_session: tokio::sync::Mutex<Option<ReplSession>>,
}

pub struct Backend {
    state: Arc<State>,
}

impl Backend {
    fn new(client: Client, shutdown: Arc<AtomicBool>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            state: Arc::new(State {
                client,
                docs: RwLock::new(AHashMap::new()),
                workspace: RwLock::new(Arc::new(Workspace::empty(&cwd))),
                snapshot: RwLock::new(Arc::new(Snapshot::empty())),
                published: tokio::sync::Mutex::new(Published::default()),
                generation: AtomicU64::new(0),
                analysis: tokio::sync::Mutex::new(()),
                dynamic_watchers: AtomicBool::new(false),
                shutdown,
                repl_session: tokio::sync::Mutex::new(None),
            }),
        }
    }

    /// Run a request handler behind the panic guard.
    async fn guard<T>(
        &self,
        name: &str,
        fut: impl Future<Output = JsonRpcResult<T>>,
    ) -> JsonRpcResult<T> {
        match catch_panic(fut).await {
            Ok(result) => result,
            Err(message) => {
                let message = format!("hot-lsp: {name} failed: {message}");
                tracing::error!("{message}");
                self.state
                    .client
                    .log_message(MessageType::ERROR, &message)
                    .await;
                Err(JsonRpcError {
                    code: tower_lsp_server::jsonrpc::ErrorCode::InternalError,
                    message: message.into(),
                    data: None,
                })
            }
        }
    }

    /// Run a notification handler behind the panic guard.
    async fn guard_notification(&self, name: &str, fut: impl Future<Output = ()>) {
        if let Err(message) = catch_panic(fut).await {
            let message = format!("hot-lsp: {name} failed: {message}");
            tracing::error!("{message}");
            self.state
                .client
                .log_message(MessageType::ERROR, &message)
                .await;
        }
    }

    fn doc(&self, uri: &Uri) -> Option<Arc<Document>> {
        read(&self.state.docs).get(uri).cloned()
    }

    fn snapshot(&self) -> Arc<Snapshot> {
        read(&self.state.snapshot).clone()
    }

    fn workspace(&self) -> Arc<Workspace> {
        read(&self.state.workspace).clone()
    }

    /// Open documents by canonical path.
    fn open_by_path(&self) -> AHashMap<PathBuf, (Uri, Arc<Document>)> {
        read(&self.state.docs)
            .iter()
            .filter_map(|(uri, doc)| Some((uri_path(uri)?, (uri.clone(), doc.clone()))))
            .collect()
    }

    /// The document, its token context and the cursor's char offset.
    fn at(&self, uri: &Uri, position: Position) -> Option<(Arc<Document>, DocContext, usize)> {
        if !is_hot(uri) {
            return None;
        }
        let doc = self.doc(uri)?;
        let ctx = DocContext::new(&doc.text);
        let offset = doc.char_offset(position);
        Some((doc, ctx, offset))
    }
}

impl State {
    /// Check the project after `delay`, unless a later change supersedes it.
    fn schedule_analysis(self: &Arc<Self>, delay: Duration) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let state = self.clone();
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if state.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            let _running = state.analysis.lock().await;
            if state.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            let input = analysis::Input {
                workspace: read(&state.workspace).clone(),
                open: read(&state.docs)
                    .iter()
                    .filter(|(uri, _)| is_hot(uri))
                    .filter_map(|(uri, doc)| Some((uri_path(uri)?, doc.text.clone())))
                    .collect(),
            };
            match tokio::task::spawn_blocking(move || analysis::run(input)).await {
                Ok(output) => state.apply(output).await,
                Err(e) => {
                    let message = format!("hot-lsp: check failed: {e}");
                    tracing::error!("{message}");
                    state.client.log_message(MessageType::ERROR, message).await;
                }
            }
        });
    }

    /// Publish a check's diagnostics and keep its program for navigation.
    async fn apply(&self, output: analysis::Output) {
        if let Some(index) = output.index {
            *write(&self.snapshot) = Arc::new(Snapshot {
                index,
                project_files_raw: output.project_files_raw.into_iter().collect(),
                project_files: output.project_files.clone(),
            });
        }

        let mut published = self.published.lock().await;
        // A check that stopped at parse errors says nothing about other
        // files: keep their diagnostics, but drop parse errors it no longer
        // reports.
        let mut next = if output.parse_failed {
            let mut kept = published.diagnostics.clone();
            for uri in &published.from_parse_failure {
                kept.remove(uri);
            }
            kept
        } else {
            AHashMap::new()
        };
        let mut from_parse_failure = AHashSet::new();
        let open: AHashMap<PathBuf, Uri> = read(&self.docs)
            .keys()
            .filter_map(|uri| Some((uri_path(uri)?, uri.clone())))
            .collect();
        for (path, diagnostics) in output.by_file {
            let uri = match open.get(&path) {
                Some(uri) => uri.clone(),
                None => match file_uri(&path) {
                    Some(uri) => uri,
                    None => continue,
                },
            };
            if output.parse_failed {
                from_parse_failure.insert(uri.clone());
            }
            next.insert(uri, diagnostics);
        }
        // Open documents get a publish from every check, so the client
        // always sees the result for what it edits.
        for uri in open.values() {
            next.entry(uri.clone()).or_default();
        }
        for (uri, diagnostics) in &next {
            let is_open = open.values().any(|u| u == uri);
            if is_open || published.diagnostics.get(uri) != Some(diagnostics) {
                self.client
                    .publish_diagnostics(uri.clone(), diagnostics.clone(), None)
                    .await;
            }
        }
        for uri in published.diagnostics.keys() {
            if !next.contains_key(uri) {
                self.client
                    .publish_diagnostics(uri.clone(), Vec::new(), None)
                    .await;
            }
        }
        // Problems that belong to no file.
        for message in &output.messages {
            if !published.messages.contains(message) {
                self.client
                    .show_message(MessageType::ERROR, format!("Hot: {message}"))
                    .await;
            }
        }
        published.messages = output.messages;
        next.retain(|_, diagnostics| !diagnostics.is_empty());
        published.diagnostics = next;
        published.from_parse_failure = from_parse_failure;
    }

    /// Load the project at `root` and make it current.
    async fn load_workspace(self: &Arc<Self>, root: PathBuf) {
        let _running = self.analysis.lock().await;
        if root.is_dir()
            && let Err(e) = std::env::set_current_dir(&root)
        {
            tracing::warn!("Could not change to {}: {}", root.display(), e);
        }
        let loaded = tokio::task::spawn_blocking(move || Workspace::load(&root)).await;
        match loaded {
            Ok(workspace) => {
                if let Some(error) = &workspace.conf_error {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("hot-lsp: configuration error: {error}"),
                        )
                        .await;
                }
                *write(&self.workspace) = Arc::new(workspace);
                *self.repl_session.lock().await = None;
            }
            Err(e) => {
                self.client
                    .log_message(
                        MessageType::ERROR,
                        format!("hot-lsp: loading the project failed: {e}"),
                    )
                    .await;
            }
        }
    }
}

/// The project root a client names: the first workspace folder that has a
/// `hot.hot`, else the first folder or the root URI, else the working
/// directory.
fn client_root(params: &InitializeParams) -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for folder in params.workspace_folders.iter().flatten() {
        if let Some(path) = file_path(&folder.uri) {
            candidates.push(path);
        }
    }
    #[allow(deprecated)]
    if let Some(path) = params.root_uri.as_ref().and_then(file_path) {
        candidates.push(path);
    }
    #[allow(deprecated)]
    if let Some(path) = params.root_path.as_ref() {
        candidates.push(PathBuf::from(path));
    }
    candidates
        .iter()
        .find(|p| p.join(workspace::CONF_FILE).is_file())
        .or(candidates.first())
        .cloned()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> JsonRpcResult<InitializeResult> {
        self.guard("initialize", async {
            let dynamic = params
                .capabilities
                .workspace
                .as_ref()
                .and_then(|w| w.did_change_watched_files.as_ref())
                .and_then(|w| w.dynamic_registration)
                .unwrap_or(false);
            self.state.dynamic_watchers.store(dynamic, Ordering::SeqCst);
            self.state.load_workspace(client_root(&params)).await;

            Ok(InitializeResult {
                capabilities: ServerCapabilities {
                    // UTF-16 is the default, and the only encoding the server
                    // speaks.
                    position_encoding: Some(PositionEncodingKind::UTF16),
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
                            "{".to_string(),
                        ]),
                        ..Default::default()
                    }),
                    hover_provider: Some(HoverProviderCapability::Simple(true)),
                    signature_help_provider: Some(SignatureHelpOptions {
                        trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                        retrigger_characters: Some(vec![")".to_string()]),
                        work_done_progress_options: Default::default(),
                    }),
                    definition_provider: Some(OneOf::Left(true)),
                    references_provider: Some(OneOf::Left(true)),
                    document_formatting_provider: Some(OneOf::Left(true)),
                    workspace_symbol_provider: Some(OneOf::Left(true)),
                    ..Default::default()
                },
                server_info: Some(ServerInfo {
                    name: "hot-lsp".to_string(),
                    version: Some(env!("HOT_VERSION").to_string()),
                }),
                offset_encoding: None,
            })
        })
        .await
    }

    async fn initialized(&self, _: InitializedParams) {
        self.guard_notification("initialized", async {
            self.state
                .client
                .log_message(MessageType::INFO, "hot-lsp initialized")
                .await;
            if self.state.dynamic_watchers.load(Ordering::SeqCst) {
                let options = DidChangeWatchedFilesRegistrationOptions {
                    watchers: vec![FileSystemWatcher {
                        glob_pattern: GlobPattern::String("**/*.hot".to_string()),
                        kind: None,
                    }],
                };
                if let Ok(options) = serde_json::to_value(options) {
                    let registration = Registration {
                        id: "hot-watched-files".to_string(),
                        method: "workspace/didChangeWatchedFiles".to_string(),
                        register_options: Some(options),
                    };
                    let client = self.state.client.clone();
                    // The client answers after this notification returns.
                    tokio::spawn(async move {
                        if let Err(e) = client.register_capability(vec![registration]).await {
                            tracing::debug!("File watcher registration failed: {e}");
                        }
                    });
                }
            }
            self.state.schedule_analysis(Duration::ZERO);
        })
        .await
    }

    async fn shutdown(&self) -> JsonRpcResult<()> {
        self.state.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.guard_notification("didOpen", async {
            let uri = params.text_document.uri;
            if !is_hot(&uri) {
                return;
            }
            write(&self.state.docs).insert(
                uri.clone(),
                Arc::new(Document::new(
                    params.text_document.text,
                    params.text_document.version,
                )),
            );
            // A file in a project the server didn't start in: switch to it.
            let workspace = self.workspace();
            if !workspace.has_conf
                && let Some(path) = uri_path(&uri)
                && let Some(root) = workspace::find_root(&path)
            {
                self.state.load_workspace(root).await;
            }
            self.state.schedule_analysis(Duration::ZERO);
        })
        .await
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.guard_notification("didChange", async {
            let uri = params.text_document.uri;
            if !is_hot(&uri) {
                return;
            }
            // Full sync: the last change is the whole text, even when empty.
            let Some(change) = params.content_changes.into_iter().last() else {
                return;
            };
            {
                let mut docs = write(&self.state.docs);
                let newer = docs
                    .get(&uri)
                    .is_none_or(|doc| doc.version <= params.text_document.version);
                if newer {
                    docs.insert(
                        uri,
                        Arc::new(Document::new(change.text, params.text_document.version)),
                    );
                }
            }
            self.state.schedule_analysis(EDIT_DEBOUNCE);
        })
        .await
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.guard_notification("didClose", async {
            let uri = params.text_document.uri;
            if write(&self.state.docs).remove(&uri).is_some() {
                self.state.schedule_analysis(Duration::ZERO);
            }
        })
        .await
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.guard_notification("didSave", async {
            let uri = params.text_document.uri;
            if !is_hot(&uri) {
                return;
            }
            let workspace = self.workspace();
            if let Some(path) = uri_path(&uri)
                && workspace.is_config_file(&path)
            {
                self.state.load_workspace(workspace.root.clone()).await;
            }
            self.state.schedule_analysis(Duration::ZERO);
        })
        .await
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        self.guard_notification("didChangeWatchedFiles", async {
            let workspace = self.workspace();
            let mut reload = false;
            let mut changed = false;
            for change in &params.changes {
                if !is_hot(&change.uri) {
                    continue;
                }
                changed = true;
                if let Some(path) = file_path(&change.uri)
                    && workspace.is_config_file(&path)
                {
                    reload = true;
                }
            }
            if reload {
                self.state.load_workspace(workspace.root.clone()).await;
            }
            if changed {
                self.state.schedule_analysis(Duration::ZERO);
            }
        })
        .await
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> JsonRpcResult<Option<CompletionResponse>> {
        self.guard("completion", async {
            let pos = params.text_document_position;
            let Some((_, ctx, offset)) = self.at(&pos.text_document.uri, pos.position) else {
                return Ok(None);
            };
            let snapshot = self.snapshot();
            Ok(features::completion(&snapshot.index, &ctx, offset)
                .filter(|items| !items.is_empty())
                .map(CompletionResponse::Array))
        })
        .await
    }

    async fn hover(&self, params: HoverParams) -> JsonRpcResult<Option<Hover>> {
        self.guard("hover", async {
            let pos = params.text_document_position_params;
            let Some((doc, ctx, offset)) = self.at(&pos.text_document.uri, pos.position) else {
                return Ok(None);
            };
            let snapshot = self.snapshot();
            Ok(features::hover(&snapshot.index, &doc, &ctx, offset))
        })
        .await
    }

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> JsonRpcResult<Option<SignatureHelp>> {
        self.guard("signatureHelp", async {
            let pos = params.text_document_position_params;
            let Some((_, ctx, offset)) = self.at(&pos.text_document.uri, pos.position) else {
                return Ok(None);
            };
            let snapshot = self.snapshot();
            Ok(features::signature_help(&snapshot.index, &ctx, offset))
        })
        .await
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> JsonRpcResult<Option<GotoDefinitionResponse>> {
        self.guard("definition", async {
            let pos = params.text_document_position_params;
            let uri = pos.text_document.uri;
            let Some((doc, ctx, offset)) = self.at(&uri, pos.position) else {
                return Ok(None);
            };
            let snapshot = self.snapshot();
            let workspace = self.workspace();
            let open = self.open_by_path();
            let mut files = features::Files::new(&workspace.root, &open);
            Ok(
                features::definition(&snapshot.index, &uri, &doc, &ctx, offset, &mut files)
                    .map(GotoDefinitionResponse::Scalar),
            )
        })
        .await
    }

    async fn references(&self, params: ReferenceParams) -> JsonRpcResult<Option<Vec<Location>>> {
        self.guard("references", async {
            let pos = params.text_document_position;
            let uri = pos.text_document.uri;
            let Some((doc, ctx, offset)) = self.at(&uri, pos.position) else {
                return Ok(None);
            };
            let snapshot = self.snapshot();
            let workspace = self.workspace();
            let open = self.open_by_path();
            let mut files = features::Files::new(&workspace.root, &open);
            let Some((target, declaration)) =
                features::reference_target(&snapshot.index, &uri, &doc, &ctx, offset, &mut files)
            else {
                return Ok(None);
            };

            // Every project file, open or not, and every open document.
            let mut paths: Vec<PathBuf> = snapshot.project_files.clone();
            for path in open.keys() {
                if !paths.contains(path) {
                    paths.push(path.clone());
                }
            }
            let mut locations = Vec::new();
            for path in &paths {
                if let Some((file_uri, text)) = files.text(path) {
                    features::references_in(
                        &snapshot.index,
                        &file_uri,
                        &text,
                        &target,
                        &mut locations,
                    );
                }
            }
            let mut seen = AHashSet::new();
            locations.retain(|l| {
                seen.insert((l.uri.clone(), l.range.start.line, l.range.start.character))
            });
            match declaration {
                Some(decl) if !params.context.include_declaration => {
                    locations.retain(|l| !(l.uri == decl.uri && l.range.start == decl.range.start));
                }
                Some(decl)
                    if !locations
                        .iter()
                        .any(|l| l.uri == decl.uri && l.range.start == decl.range.start) =>
                {
                    locations.push(decl)
                }
                _ => {}
            }
            Ok((!locations.is_empty()).then_some(locations))
        })
        .await
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> JsonRpcResult<Option<Vec<TextEdit>>> {
        self.guard("formatting", async {
            let uri = params.text_document.uri;
            let Some(doc) = self.doc(&uri).filter(|_| is_hot(&uri)) else {
                return Ok(None);
            };
            let indent = params.options.tab_size.clamp(1, 8) as usize;
            let indent = if params.options.insert_spaces {
                indent
            } else {
                4
            };
            match hot::lang::fmt::format_str(&doc.text, indent) {
                Ok(formatted) if formatted == doc.text => Ok(Some(Vec::new())),
                // Never replace text that parses with text that doesn't.
                Ok(formatted)
                    if hot::lang::parser::parse_hot(&formatted).is_err()
                        && hot::lang::parser::parse_hot(&doc.text).is_ok() =>
                {
                    self.state
                        .client
                        .log_message(
                            MessageType::WARNING,
                            format!(
                                "hot-lsp: not formatting {}: the result would not parse",
                                uri.as_str()
                            ),
                        )
                        .await;
                    Ok(None)
                }
                Ok(formatted) => Ok(Some(vec![TextEdit::new(doc.full_range(), formatted)])),
                Err(e) => {
                    self.state
                        .client
                        .log_message(
                            MessageType::WARNING,
                            format!("hot-lsp: not formatting {}: {e}", uri.as_str()),
                        )
                        .await;
                    Ok(None)
                }
            }
        })
        .await
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> JsonRpcResult<Option<WorkspaceSymbolResponse>> {
        self.guard("workspace/symbol", async {
            let snapshot = self.snapshot();
            let workspace = self.workspace();
            let open = self.open_by_path();
            let mut files = features::Files::new(&workspace.root, &open);
            Ok(Some(WorkspaceSymbolResponse::Flat(
                features::workspace_symbols(
                    &snapshot.index,
                    &params.query,
                    &snapshot.project_files_raw,
                    &mut files,
                ),
            )))
        })
        .await
    }
}

impl Backend {
    /// Evaluate Hot code using a stateful REPL session
    pub async fn eval_code(&self, params: EvalParams) -> EvalResult {
        // Get or create the REPL session
        let mut session_guard = self.state.repl_session.lock().await;

        if session_guard.is_none() {
            // Initialize the REPL session with project configuration
            let workspace = self.workspace();
            let conf = workspace.conf.clone();
            let project_name = workspace
                .project_name
                .clone()
                .unwrap_or_else(|| hot::project::get_default_project_name(&conf));
            let repl_config = ReplConfig {
                src_paths: workspace.src_paths.clone(),
                test_paths: hot::project::get_project_test_paths(&conf, &project_name),
                conf: Some(conf),
                project_name: Some(project_name),
                context_storage: None,
            };
            *session_guard = Some(ReplSession::new(repl_config));
        }

        let Some(session) = session_guard.as_mut() else {
            return EvalResult {
                success: false,
                result: String::new(),
                error: Some("No REPL session".to_string()),
                namespace: String::new(),
                stdout: None,
            };
        };

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
        let eval_result = std::panic::catch_unwind(AssertUnwindSafe(|| session.eval(&params.code)));
        // Stop capturing and get any output
        let captured_stdout = hot::lang::hot::io::release_capture_stdout();

        match eval_result {
            Ok(Ok(result)) => EvalResult {
                success: true,
                result: format!("{}", result),
                error: None,
                namespace: session.current_namespace().to_string(),
                stdout: captured_stdout,
            },
            Ok(Err(e)) => EvalResult {
                success: false,
                result: String::new(),
                error: Some(e),
                namespace: session.current_namespace().to_string(),
                stdout: captured_stdout,
            },
            Err(payload) => {
                let message = panic_message(payload.as_ref());
                // The session may be mid-update; start a fresh one next time.
                *session_guard = None;
                EvalResult {
                    success: false,
                    result: String::new(),
                    error: Some(format!("evaluation panicked: {message}")),
                    namespace: String::new(),
                    stdout: captured_stdout,
                }
            }
        }
    }

    /// Reset the REPL session (clear all state)
    pub async fn reset_repl_session(&self) {
        *self.state.repl_session.lock().await = None;
    }

    /// Handle the hot/eval custom LSP request
    async fn handle_eval(&self, params: EvalParams) -> JsonRpcResult<EvalResult> {
        self.guard("hot/eval", async { Ok(self.eval_code(params).await) })
            .await
    }

    /// Handle the hot/resetRepl custom LSP request
    async fn handle_reset_repl(&self, _params: ()) -> JsonRpcResult<()> {
        self.reset_repl_session().await;
        Ok(())
    }
}

/// Serve the language server over the given streams until the client sends
/// `exit` or closes the input. Returns the exit code the protocol asks for:
/// 0 after a `shutdown` request, 1 otherwise.
pub async fn serve<I, O>(input: I, output: O) -> i32
where
    I: AsyncRead + Unpin,
    O: AsyncWrite,
{
    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = shutdown.clone();
    let (service, socket) = LspService::build(move |client| Backend::new(client, flag))
        .custom_method("hot/eval", Backend::handle_eval)
        .custom_method("hot/resetRepl", Backend::handle_reset_repl)
        .finish();
    Server::new(transport::ExitAware::new(input), output, socket)
        .serve(service)
        .await;
    if shutdown.load(Ordering::SeqCst) {
        0
    } else {
        1
    }
}

/// Serve the language server over stdin and stdout. Returns the process
/// exit code.
pub async fn run_stdio() -> anyhow::Result<i32> {
    Ok(serve(tokio::io::stdin(), tokio::io::stdout()).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panic_becomes_an_error() {
        let result: Result<u32, String> = catch_panic(async {
            tokio::task::yield_now().await;
            let v: Vec<u32> = Vec::new();
            v[3]
        })
        .await;
        assert!(result.unwrap_err().contains("index out of bounds"));
        assert_eq!(catch_panic(async { 7 }).await, Ok(7));
    }

    #[test]
    fn file_uris_round_trip() {
        let path = std::env::temp_dir().join("a dir").join("ü.hot");
        let uri = file_uri(&path).unwrap();
        assert!(uri.as_str().starts_with("file:///"), "{}", uri.as_str());
        assert_eq!(file_path(&uri), Some(path));
    }

    #[test]
    fn only_file_uris_name_paths() {
        let uri: Uri = "untitled:Untitled-1.hot".parse().unwrap();
        assert_eq!(file_path(&uri), None);
    }

    #[cfg(windows)]
    #[test]
    fn canonical_windows_paths_get_plain_uris() {
        let uri = file_uri(Path::new(r"\\?\C:\Users\me\hot.hot")).unwrap();
        assert_eq!(uri.as_str(), "file:///C:/Users/me/hot.hot");
        let uri = file_uri(Path::new(r"\\?\UNC\server\share\hot.hot")).unwrap();
        assert_eq!(uri.as_str(), "file://server/share/hot.hot");
    }
}
