use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use led_core::{CanonPath, CharOffset, Col, Doc, EditOp, LanguageId, PersistedContentHash, Row};
use lsp_types::{
    CodeActionOrCommand, CodeActionParams, CodeActionResponse, CompletionParams,
    CompletionResponse, DocumentFormattingParams, FormattingOptions, GotoDefinitionParams,
    GotoDefinitionResponse, InlayHintParams, NumberOrString, Range, RenameParams,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, TextEdit, WorkspaceEdit,
};
use serde_json::Value;

use crate::convert::{
    apply_edits_to_disk, code_action_titles, convert_completion_response, convert_diagnostics,
    convert_inlay_hints, definition_response_to_locations, doc_full_text, doc_line, lsp_pos,
    lsp_text_edit_to_domain, uri_from_path, workspace_edit_to_file_edits,
};
use crate::registry::LspRegistry;
use crate::server::LanguageServer;
use crate::transport::LspNotification;
use crate::{FileEdit, LspIn, LspOut};

// ── Diagnostic source ──

/// Diagnostic mode, decided from server capabilities on startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagMode {
    /// Server pushes diagnostics via publishDiagnostics (default).
    Push,
    /// Server supports pull via textDocument/diagnostic.
    Pull,
}

/// Normalizes push/pull LSP diagnostic delivery.
///
/// Mode is decided once on server startup from capabilities:
/// - Push: cache incoming pushes, forward during propagation window.
/// - Pull: freeze, pull all paths, forward, unfreeze.
///
/// Never mixes results from both methods.
struct DiagnosticSource {
    mode: DiagMode,

    /// Whether the server advertised pull capability (diagnosticProvider).
    /// Stays true even after we switch to push mode — we still pull for
    /// validation/merging when a window opens.
    has_pull_capability: bool,

    /// Whether the server supports quiescence (experimental/serverStatus).
    has_quiescence: bool,

    /// True once the server has been quiescent at least once.
    lsp_ready: bool,

    /// A RequestDiagnostics arrived during init, waiting for first quiescence.
    init_delayed_request: bool,

    /// Latest push diagnostics per path (always updated).
    push_cache: HashMap<CanonPath, Vec<crate::Diagnostic>>,

    /// Propagation window state (None = closed).
    window: Option<DiagWindow>,
}

struct DiagWindow {
    /// Content hash snapshot for every opened doc at window open time.
    hash_snapshot: HashMap<CanonPath, PersistedContentHash>,
    /// Pull mode only: paths still awaiting pull response.
    pending_pulls: HashSet<CanonPath>,
    /// Pull mode only: whether we're still in the freeze phase.
    frozen: bool,
    /// Pull mode: hard timeout for the freeze. Push mode: not used.
    deadline: Option<tokio::time::Instant>,
}

/// What the manager should do after on_push.
enum DiagPushResult {
    /// Forward this diagnostic to the model with the snapshot hash.
    Forward(CanonPath, Vec<crate::Diagnostic>, PersistedContentHash),
    /// Forward an empty diagnostic list (clearing) to the model.
    /// Window is closed, so the manager must look up the current doc hash.
    ForwardClearing(CanonPath),
    /// Mode switched from pull to push — manager should restart the window.
    RestartWindow,
    /// Ignore (wrong mode, or non-clearing push outside a window).
    Ignore,
}

impl DiagnosticSource {
    fn new() -> Self {
        Self {
            mode: DiagMode::Push,
            has_pull_capability: false,
            has_quiescence: false,
            lsp_ready: true,
            init_delayed_request: false,
            push_cache: HashMap::new(),
            window: None,
        }
    }

    /// Set mode from server capabilities. Called once on ServerStarted.
    fn set_mode(&mut self, mode: DiagMode) {
        log::info!("diag: mode set to {:?}", mode);
        self.mode = mode;
        if mode == DiagMode::Pull {
            self.has_pull_capability = true;
        }
    }

    /// Whether a RequestDiagnostics should be deferred until the LSP is ready.
    /// For quiescence servers: ready after first quiescent=true.
    /// For others: ready immediately on ServerStarted.
    fn should_defer_request(&self) -> bool {
        !self.lsp_ready
    }

    /// Called when experimental/serverStatus quiescent=true arrives.
    /// Returns true if a deferred request should now be fulfilled.
    fn on_quiescence(&mut self) -> bool {
        self.lsp_ready = true;
        if self.init_delayed_request {
            self.init_delayed_request = false;
            true
        } else {
            false
        }
    }

    /// Whether cmd_rx should be frozen.
    fn is_frozen(&self) -> bool {
        self.window.as_ref().is_some_and(|w| w.frozen)
    }

    /// Deadline for the freeze (pull mode only).
    fn deadline(&self) -> Option<tokio::time::Instant> {
        if self.is_frozen() {
            self.window.as_ref().and_then(|w| w.deadline)
        } else {
            Option::None
        }
    }

    /// Open a diagnostic window. Snapshots content hashes.
    /// Returns paths to pull (pull mode) or empty (push mode).
    fn open_window(
        &mut self,
        docs: &HashMap<CanonPath, Arc<dyn Doc>>,
        opened_docs: &HashSet<CanonPath>,
    ) -> Vec<CanonPath> {
        let hash_snapshot: HashMap<CanonPath, PersistedContentHash> = docs
            .iter()
            .map(|(p, d)| (p.clone(), PersistedContentHash(d.content_hash().0)))
            .collect();

        match self.mode {
            DiagMode::Push => {
                // Push mode: do not freeze. If server has pull capability,
                // issue pulls only for paths without cached push results.
                let pull_paths: Vec<CanonPath> = if self.has_pull_capability {
                    opened_docs
                        .iter()
                        .filter(|p| !self.push_cache.contains_key(*p))
                        .cloned()
                        .collect()
                } else {
                    Vec::new()
                };
                log::trace!(
                    "diag: window open (push), {} docs snapshotted, {} to pull",
                    hash_snapshot.len(),
                    pull_paths.len(),
                );
                self.window = Some(DiagWindow {
                    hash_snapshot,
                    pending_pulls: pull_paths.iter().cloned().collect(),
                    frozen: false,
                    deadline: None,
                });
                pull_paths
            }
            DiagMode::Pull => {
                let pull_paths: Vec<CanonPath> = opened_docs.iter().cloned().collect();
                log::trace!(
                    "diag: window open (pull), {} docs, {} to pull",
                    hash_snapshot.len(),
                    pull_paths.len(),
                );
                self.window = Some(DiagWindow {
                    hash_snapshot,
                    pending_pulls: pull_paths.iter().cloned().collect(),
                    frozen: true,
                    deadline: Some(tokio::time::Instant::now() + std::time::Duration::from_secs(5)),
                });
                pull_paths
            }
        }
    }

    /// Push mode: get all cached diagnostics to forward at window open.
    fn drain_cache_for_window(
        &self,
    ) -> Vec<(CanonPath, Vec<crate::Diagnostic>, PersistedContentHash)> {
        let Some(window) = &self.window else {
            return vec![];
        };
        log::trace!(
            "diag: reading cache for window, {} entries: [{}]",
            self.push_cache.len(),
            self.push_cache
                .iter()
                .map(|(p, d)| format!("{}={}", p.display(), d.len()))
                .collect::<Vec<_>>()
                .join(", "),
        );
        self.push_cache
            .iter()
            .map(|(path, diags)| {
                let h = window
                    .hash_snapshot
                    .get(path)
                    .copied()
                    .unwrap_or(PersistedContentHash(0));
                (path.clone(), diags.clone(), h)
            })
            .collect()
    }

    /// Push notification arrived. Always updates cache.
    /// If we were in pull mode, switch to push permanently.
    /// If window is open in push mode, returns the diagnostic to forward.
    fn on_push(&mut self, path: CanonPath, diags: Vec<crate::Diagnostic>) -> DiagPushResult {
        if self.mode == DiagMode::Pull {
            log::info!("diag: received push, switching from pull to push mode");
            self.mode = DiagMode::Push;
            let had_window = self.window.is_some();
            self.window = None;
            self.push_cache.insert(path.clone(), diags.clone());
            return if had_window {
                DiagPushResult::RestartWindow
            } else {
                DiagPushResult::Ignore
            };
        }
        log::trace!(
            "diag: cache update for {}, {} diags, window_open={}",
            path.display(),
            diags.len(),
            self.window.is_some(),
        );
        let is_clearing = diags.is_empty();
        self.push_cache.insert(path.clone(), diags.clone());

        if let Some(window) = &self.window {
            // Window open: forward all push diagnostics tagged with snapshot hash.
            let h = window
                .hash_snapshot
                .get(&path)
                .copied()
                .unwrap_or(PersistedContentHash(0));
            DiagPushResult::Forward(path, diags, h)
        } else if is_clearing {
            // Window closed but the push clears errors — forward with current
            // doc hash (clearing is always safe to propagate).
            // The hash will need to be supplied by the caller since we don't
            // have access to docs here.
            DiagPushResult::ForwardClearing(path)
        } else {
            DiagPushResult::Ignore
        }
    }

    /// Pull response arrived. Merges with cached push diagnostics for the
    /// same path: pull is the source of truth for which (line, code) pairs
    /// have errors; cache provides the more detailed diagnostic when present.
    /// Returns the merged result to forward and whether all pulls are done.
    fn on_pull_response(
        &mut self,
        path: CanonPath,
        pull_diags: Vec<crate::Diagnostic>,
    ) -> (
        Option<(CanonPath, Vec<crate::Diagnostic>, PersistedContentHash)>,
        bool,
    ) {
        let Some(window) = &mut self.window else {
            return (None, false);
        };
        if !window.pending_pulls.remove(&path) {
            return (None, false);
        }
        let h = window
            .hash_snapshot
            .get(&path)
            .copied()
            .unwrap_or(PersistedContentHash(0));
        let all_done = window.pending_pulls.is_empty();
        if all_done {
            window.frozen = false;
            window.deadline = None;
        }

        // Pull is a fallback only: if cache has push results for this path,
        // use those (push is more detailed). Pull never modifies the cache.
        let result = if let Some(cached) = self.push_cache.get(&path) {
            log::trace!(
                "diag: pull for {} using cached push ({} diags)",
                path.display(),
                cached.len(),
            );
            cached.clone()
        } else {
            log::trace!(
                "diag: pull for {} using pull result ({} diags)",
                path.display(),
                pull_diags.len(),
            );
            pull_diags
        };

        (Some((path, result, h)), all_done)
    }

    /// Check if a BufferChanged should close the window.
    /// Compares the doc's current ephemeral hash against the snapshot.
    fn should_close_window(&self, path: &CanonPath, doc: &Arc<dyn Doc>) -> bool {
        let Some(window) = &self.window else {
            return false;
        };
        let Some(snapshot_hash) = window.hash_snapshot.get(path) else {
            return false;
        };
        snapshot_hash.0 != doc.content_hash().0
    }

    /// Invalidate the push cache entry for a path. Called when content
    /// diverges from any previous state — the cached push is stale and
    /// will be replaced by the next push from the server.
    fn invalidate_cache(&mut self, path: &CanonPath) {
        if self.push_cache.remove(path).is_some() {
            log::trace!("diag: cache invalidated for {}", path.display());
        }
    }

    /// Close the propagation window.
    fn close_window(&mut self) {
        if self.window.is_some() {
            log::trace!("diag: window closed (content changed)");
            self.window = None;
        }
    }

    /// Cancel the freeze on timeout (pull mode).
    fn cancel_freeze(&mut self) {
        if let Some(window) = &mut self.window {
            log::warn!("diag: pull freeze timeout");
            window.frozen = false;
            window.deadline = None;
            window.pending_pulls.clear();
        }
    }

    #[cfg(test)]
    fn has_window(&self) -> bool {
        self.window.is_some()
    }
}

// ── Internal event types ──

enum ManagerEvent {
    ServerStarted {
        language: LanguageId,
        server: Arc<LanguageServer>,
    },
    ServerError {
        error: String,
        not_found: bool,
    },
    Notification(LanguageId, LspNotification),
    RequestResult(RequestResult),
    FileChanged(CanonPath, FileChangeKind),
}

enum RequestResult {
    GotoDefinition {
        locations: Vec<(CanonPath, Row, Col)>,
    },
    Format {
        path: CanonPath,
        edits: Vec<TextEdit>,
    },
    Rename {
        file_edits: Vec<FileEdit>,
    },
    CodeActions {
        path: CanonPath,
        raw: Vec<CodeActionOrCommand>,
    },
    CodeActionResolved {
        file_edits: Vec<FileEdit>,
    },
    InlayHints {
        path: CanonPath,
        hints: Vec<lsp_types::InlayHint>,
    },
    Diagnostics {
        path: CanonPath,
        raw: Vec<lsp_types::Diagnostic>,
    },
    Completion {
        path: CanonPath,
        response: CompletionResponse,
        row: Row,
        col: Col,
        seq: u64,
    },
    CompletionResolved {
        additional_edits: Vec<crate::TextEdit>,
    },
    FormatRaw {
        path: CanonPath,
        edits: Vec<crate::TextEdit>,
    },
    FormatDone,
    Error {
        message: String,
    },
}

#[derive(Clone, Copy)]
enum FileChangeKind {
    Created,
    Changed,
    Deleted,
}

struct ProgressState {
    title: String,
    message: Option<String>,
    percentage: Option<u32>,
}

enum ProgressUpdate {
    Begin {
        title: String,
        message: Option<String>,
        percentage: Option<u32>,
    },
    Report {
        message: Option<String>,
        percentage: Option<u32>,
    },
    End,
}

// ── Manager ──

struct LspManager {
    registry: LspRegistry,
    servers: HashMap<LanguageId, Arc<LanguageServer>>,
    root: CanonPath,
    event_tx: tokio::sync::mpsc::UnboundedSender<ManagerEvent>,
    pending_starts: HashSet<LanguageId>,
    opened_docs: HashSet<CanonPath>,
    pending_opens: HashSet<CanonPath>,
    docs: HashMap<CanonPath, Arc<dyn Doc>>,
    /// Per-buffer pre-resolved language. Populated on `BufferOpened`;
    /// consulted by `server_for_path` and `send_did_open` so language
    /// detection happens once (in `BufferState::new`) rather than on
    /// every LSP call.
    languages: HashMap<CanonPath, Option<LanguageId>>,
    doc_versions: HashMap<CanonPath, i32>,
    pending_code_actions: HashMap<CanonPath, Vec<CodeActionOrCommand>>,
    completion_items: Vec<lsp_types::CompletionItem>,
    /// Active completion session for re-filtering on BufferChanged
    completion_path: Option<CanonPath>,
    completion_row: Row,
    completion_prefix_start_col: Col,
    /// Domain items from last server response (unfiltered)
    completion_domain_items: Vec<crate::CompletionItem>,
    progress_tokens: HashMap<String, ProgressState>,
    quiescent: HashMap<LanguageId, bool>,
    /// Normalizes push/pull diagnostic delivery into a pull-all model.
    /// Also manages the diagnostic cycle (freeze) state.
    diag_source: DiagnosticSource,
    _file_watcher: Option<notify::RecommendedWatcher>,
    file_watcher_globs: Option<globset::GlobSet>,
    /// Rate-limit progress updates to the UI.
    last_progress_sent: std::time::Instant,
    /// Trigger characters from server capabilities (e.g. [".", ":", "("])
    trigger_characters: Vec<String>,
    /// Monotonic counter for completion requests — ignore stale responses
    completion_seq: u64,
}

/// Entry point: runs the manager loop.
pub(crate) async fn run(
    mut cmd_rx: tokio::sync::mpsc::Receiver<LspOut>,
    result_tx: tokio::sync::mpsc::Sender<LspIn>,
    server_override: Option<String>,
) {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();

    let mut mgr = LspManager {
        registry: LspRegistry::new(server_override),
        servers: HashMap::new(),
        root: CanonPath::default(),
        event_tx,
        pending_starts: HashSet::new(),
        opened_docs: HashSet::new(),
        pending_opens: HashSet::new(),
        docs: HashMap::new(),
        languages: HashMap::new(),
        doc_versions: HashMap::new(),
        pending_code_actions: HashMap::new(),
        completion_items: Vec::new(),
        completion_path: None,
        completion_row: Row(0),
        completion_prefix_start_col: Col(0),
        completion_domain_items: Vec::new(),
        progress_tokens: HashMap::new(),
        quiescent: HashMap::new(),
        diag_source: DiagnosticSource::new(),
        _file_watcher: None,
        file_watcher_globs: None,
        last_progress_sent: std::time::Instant::now(),
        trigger_characters: Vec::new(),
        completion_seq: 0,
    };

    loop {
        if let Some(deadline) = mgr.diag_source.deadline() {
            // Frozen (pull mode): only read server events, with timeout
            tokio::select! {
                event = event_rx.recv() => {
                    let Some(event) = event else { break };
                    mgr.handle_event(event, &result_tx).await;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    mgr.diag_source.cancel_freeze();
                }
            }
        } else {
            // Normal: read both
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    mgr.handle_command(cmd, &result_tx).await;
                }
                event = event_rx.recv() => {
                    let Some(event) = event else { break };
                    mgr.handle_event(event, &result_tx).await;
                }
            }
        }
    }
}

impl LspManager {
    // ── Command dispatch ──

    async fn handle_command(&mut self, cmd: LspOut, result_tx: &tokio::sync::mpsc::Sender<LspIn>) {
        match cmd {
            LspOut::Init { root } => {
                self.root = root;
            }
            LspOut::Shutdown => {
                self.shutdown_all().await;
            }
            LspOut::BufferOpened {
                path,
                language,
                doc,
            } => {
                self.docs.insert(path.clone(), doc);
                self.languages.insert(path.clone(), language);
                self.ensure_server_for_path(&path, language);
                self.send_did_open(&path, language);
            }
            LspOut::BufferChanged {
                path,
                doc,
                edit_ops,
                do_save,
            } => {
                // Check if this edit diverges from the diagnostic snapshot
                if self.diag_source.should_close_window(&path, &doc) {
                    self.diag_source.close_window();
                    // Cached push for this path is now stale (content changed).
                    self.diag_source.invalidate_cache(&path);
                }
                let old_doc = self.docs.insert(path.clone(), doc);
                self.send_did_change(&path, &edit_ops, old_doc.as_deref());
                if do_save {
                    self.send_did_save(&path);
                }
                // Check if last edit was a trigger character → fresh completion
                let triggered = self.check_trigger_char(&path, &edit_ops);
                if triggered {
                    self.completion_path = None;
                    self.completion_domain_items.clear();
                    if let Some(op) = edit_ops.last() {
                        let new_doc = self.docs.get(&path);
                        if let Some(d) = new_doc {
                            let cursor_offset =
                                CharOffset(op.offset.0 + op.new_text.chars().count());
                            let row = d.char_to_line(cursor_offset);
                            let col = Col(cursor_offset.0 - d.line_to_char(row).0);
                            self.spawn_completion(path.clone(), row, col);
                        }
                    }
                } else {
                    self.refilter_completion(&path, &edit_ops, result_tx).await;
                }
            }
            LspOut::RequestDiagnostics => {
                log::trace!("diag: RequestDiagnostics received");
                if self.diag_source.should_defer_request() {
                    log::trace!("diag: deferring request until quiescence");
                    self.diag_source.init_delayed_request = true;
                } else {
                    self.open_diag_window(result_tx).await;
                }
            }
            LspOut::BufferClosed { path } => {
                self.send_did_close(&path);
                self.docs.remove(&path);
                self.doc_versions.remove(&path);
            }
            LspOut::GotoDefinition { path, row, col } => {
                self.spawn_goto_definition(path, row, col);
            }
            LspOut::Complete { path, row, col } => {
                self.spawn_completion(path, row, col);
            }
            LspOut::CompleteAccept { index } => {
                self.spawn_completion_resolve(index, result_tx).await;
            }
            LspOut::Rename {
                path,
                row,
                col,
                new_name,
            } => {
                self.spawn_rename(path, row, col, new_name);
            }
            LspOut::CodeAction {
                path,
                start_row,
                start_col,
                end_row,
                end_col,
            } => {
                self.spawn_code_action(path, start_row, start_col, end_row, end_col);
            }
            LspOut::CodeActionSelect { index } => {
                self.spawn_code_action_resolve(index);
            }
            LspOut::Format { path } => {
                let doc_content = self.docs.get(&path).map(|doc| {
                    let mut buf = Vec::new();
                    let _ = doc.write_to(&mut buf);
                    buf
                });
                self.spawn_format(path, doc_content);
            }
            LspOut::InlayHints {
                path,
                start_row,
                end_row,
            } => {
                self.spawn_inlay_hints(path, start_row, end_row);
            }
        }
    }

    // ── Event dispatch ──

    async fn handle_event(
        &mut self,
        event: ManagerEvent,
        result_tx: &tokio::sync::mpsc::Sender<LspIn>,
    ) {
        match event {
            ManagerEvent::ServerStarted { language, server } => {
                log::info!("LSP server started: {:?}", language);
                self.pending_starts.remove(&language);
                self.servers.insert(language, server.clone());

                // Detect diagnostic mode from server capabilities.
                // Start with pull if advertised, otherwise assume push.
                // If we ever receive a publishDiagnostics, switch to push.
                {
                    let caps = server.capabilities.lock().unwrap();
                    let has_pull = caps
                        .as_ref()
                        .is_some_and(|c| c.diagnostic_provider.is_some());
                    let mode = if has_pull {
                        DiagMode::Pull
                    } else {
                        DiagMode::Push
                    };
                    self.diag_source.set_mode(mode);

                    // Quiescence (experimental/serverStatus) is detected
                    // at runtime when the first notification arrives.
                }

                // Extract trigger characters from server capabilities
                let trigger_chars = {
                    let caps = server.capabilities.lock().unwrap();
                    caps.as_ref()
                        .and_then(|c| c.completion_provider.as_ref())
                        .and_then(|cp| cp.trigger_characters.clone())
                };
                if let Some(triggers) = trigger_chars {
                    self.trigger_characters = triggers.clone();
                    let extensions = self.registry.extensions_for_language(language);
                    let _ = result_tx
                        .send(LspIn::TriggerChars {
                            extensions,
                            triggers,
                        })
                        .await;
                }

                // Flush pending opens
                let pending: Vec<CanonPath> = self.pending_opens.drain().collect();
                for path in pending {
                    let lang = self.languages.get(&path).copied().flatten();
                    self.send_did_open(&path, lang);
                }
            }
            ManagerEvent::ServerError { error, not_found } => {
                if not_found {
                    log::info!("{}", error);
                } else {
                    log::error!("LSP server error: {}", error);
                    let _ = result_tx.send(LspIn::Error { message: error }).await;
                }
            }
            ManagerEvent::Notification(language, notif) => {
                self.handle_notification(language, notif, result_tx).await;
            }
            ManagerEvent::RequestResult(result) => {
                self.handle_request_result(result, result_tx).await;
            }
            ManagerEvent::FileChanged(path, kind) => {
                self.send_file_changed(&path, kind);
            }
        }
    }

    // ── Server lifecycle ──

    fn ensure_server_for_path(&mut self, _path: &CanonPath, language: Option<LanguageId>) {
        let Some(language) = language else { return };
        let Some(config) = self.registry.config_for_language(language).cloned() else {
            return;
        };

        if self.servers.contains_key(&language) || self.pending_starts.contains(&language) {
            return;
        }

        log::info!("LSP starting server for language: {:?}", language);
        self.pending_starts.insert(language);

        let root = self.root.clone();
        let event_tx = self.event_tx.clone();

        // Notification channel
        let (notif_tx, mut notif_rx) = tokio::sync::mpsc::unbounded_channel::<LspNotification>();
        let event_tx2 = event_tx.clone();
        tokio::spawn(async move {
            while let Some(notif) = notif_rx.recv().await {
                let _ = event_tx2.send(ManagerEvent::Notification(language, notif));
            }
        });

        tokio::spawn(async move {
            match LanguageServer::start(&config, &root, notif_tx).await {
                Ok(server) => {
                    let _ = event_tx.send(ManagerEvent::ServerStarted { language, server });
                }
                Err(e) => {
                    let _ = event_tx.send(ManagerEvent::ServerError {
                        error: e.message,
                        not_found: e.not_found,
                    });
                }
            }
        });
    }

    fn server_for_path(&self, path: &CanonPath) -> Option<Arc<LanguageServer>> {
        let language = (*self.languages.get(path)?)?;
        self.servers.get(&language).cloned()
    }

    async fn shutdown_all(&mut self) {
        for (_, server) in self.servers.drain() {
            server.shutdown().await;
        }
    }

    // ── Document sync (full-text) ──

    fn send_did_open(&mut self, path: &CanonPath, language: Option<LanguageId>) {
        if self.opened_docs.contains(path) {
            return;
        }
        let Some(server) = self.server_for_path(path) else {
            self.pending_opens.insert(path.clone());
            return;
        };
        let Some(uri) = uri_from_path(path) else {
            return;
        };
        let lang_id = language.map(|l| l.as_lsp_str()).unwrap_or("plaintext");

        let text = self
            .docs
            .get(path)
            .map(|d| doc_full_text(&**d))
            .unwrap_or_else(|| std::fs::read_to_string(path.as_path()).unwrap_or_default());

        let version = self.next_version(path);

        log::trace!("diag: sending didOpen for {}", path.display());
        server.notify(
            "textDocument/didOpen",
            &lsp_types::DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id: lang_id.to_string(),
                    version,
                    text,
                },
            },
        );
        self.opened_docs.insert(path.clone());
    }

    fn send_did_change(
        &mut self,
        path: &CanonPath,
        edit_ops: &[EditOp],
        old_doc: Option<&dyn Doc>,
    ) {
        if !self.opened_docs.contains(path) {
            return;
        }
        let Some(server) = self.server_for_path(path) else {
            return;
        };
        let Some(uri) = uri_from_path(path) else {
            return;
        };
        let version = self.next_version(path);

        let content_changes = match (edit_ops, old_doc) {
            ([op], Some(old)) => {
                // Single edit — use incremental sync
                let start_line = old.char_to_line(op.offset);
                let start_col = op.offset.0 - old.line_to_char(start_line).0;
                let old_end = op.offset.0 + op.old_text.chars().count();
                let last_row = Row(old.line_count().saturating_sub(1));
                let last_char = old.line_to_char(last_row).0 + old.line_len(last_row);
                let old_end_clamped = CharOffset(old_end.min(last_char));
                let end_line = old.char_to_line(old_end_clamped);
                let end_col = old_end_clamped.0 - old.line_to_char(end_line).0;
                let start_line_text = doc_line(old, start_line);
                let end_line_text = if end_line == start_line {
                    start_line_text.clone()
                } else {
                    doc_line(old, end_line)
                };
                vec![lsp_types::TextDocumentContentChangeEvent {
                    range: Some(lsp_types::Range {
                        start: lsp_pos(start_line, Col(start_col), start_line_text.as_deref()),
                        end: lsp_pos(end_line, Col(end_col), end_line_text.as_deref()),
                    }),
                    range_length: None,
                    text: op.new_text.clone(),
                }]
            }
            _ => {
                // Multiple edits or no old doc — full-text sync
                let Some(doc) = self.docs.get(path) else {
                    return;
                };
                vec![lsp_types::TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: doc_full_text(&**doc),
                }]
            }
        };

        log::trace!(
            "diag: sending didChange for {}, {} ops, {} changes",
            path.display(),
            edit_ops.len(),
            content_changes.len(),
        );
        server.notify(
            "textDocument/didChange",
            &lsp_types::DidChangeTextDocumentParams {
                text_document: lsp_types::VersionedTextDocumentIdentifier { uri, version },
                content_changes,
            },
        );
    }

    fn send_did_save(&mut self, path: &CanonPath) {
        let Some(server) = self.server_for_path(path) else {
            return;
        };
        let Some(uri) = uri_from_path(path) else {
            return;
        };
        let text = std::fs::read_to_string(path.as_path()).unwrap_or_default();
        log::trace!("diag: sending didSave for {}", path.display());
        server.notify(
            "textDocument/didSave",
            &lsp_types::DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier { uri },
                text: Some(text),
            },
        );
    }

    fn send_did_close(&mut self, path: &CanonPath) {
        if !self.opened_docs.remove(path) {
            return;
        }
        let Some(server) = self.server_for_path(path) else {
            return;
        };
        let Some(uri) = uri_from_path(path) else {
            return;
        };
        server.notify(
            "textDocument/didClose",
            &lsp_types::DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri },
            },
        );
    }

    fn next_version(&mut self, path: &CanonPath) -> i32 {
        let v = self.doc_versions.entry(path.clone()).or_insert(0);
        *v += 1;
        *v
    }

    // ── Feature requests ──

    fn spawn_goto_definition(&self, path: CanonPath, row: Row, col: Col) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let cursor_line = self.line_at(&path, row);

        tokio::spawn(async move {
            let Some(uri) = uri_from_path(&path) else {
                return;
            };
            let params = GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: lsp_pos(row, col, cursor_line.as_deref()),
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };

            let result: Result<Option<GotoDefinitionResponse>, _> =
                server.request("textDocument/definition", &params).await;

            let locations = match result {
                Ok(Some(resp)) => definition_response_to_locations(resp),
                Ok(None) => vec![],
                Err(e) => {
                    log::error!("LSP goto definition failed: {}", e.message);
                    vec![]
                }
            };

            let _ = event_tx.send(ManagerEvent::RequestResult(RequestResult::GotoDefinition {
                locations,
            }));
        });
    }

    fn spawn_completion(&mut self, path: CanonPath, row: Row, col: Col) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        self.completion_seq += 1;
        let seq = self.completion_seq;
        let event_tx = self.event_tx.clone();
        let cursor_line = self.line_at(&path, row);

        tokio::spawn(async move {
            let Some(uri) = uri_from_path(&path) else {
                return;
            };
            let params = CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: lsp_pos(row, col, cursor_line.as_deref()),
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: Some(lsp_types::CompletionContext {
                    trigger_kind: lsp_types::CompletionTriggerKind::INVOKED,
                    trigger_character: None,
                }),
            };

            let result: Result<Option<CompletionResponse>, _> =
                server.request("textDocument/completion", &params).await;

            match result {
                Ok(Some(response)) => {
                    let _ = event_tx.send(ManagerEvent::RequestResult(RequestResult::Completion {
                        path,
                        response,
                        row,
                        col,
                        seq,
                    }));
                }
                Ok(None) => {}
                Err(e) => {
                    log::error!("LSP completion failed: {}", e.message);
                }
            }
        });
    }

    async fn spawn_completion_resolve(
        &mut self,
        index: usize,
        _result_tx: &tokio::sync::mpsc::Sender<LspIn>,
    ) {
        let Some(item) = self.completion_items.get(index).cloned() else {
            return;
        };

        // Try to find a server for the resolve. Use the first server that has completion capability.
        let server = self.servers.values().next().cloned();
        let Some(server) = server else { return };

        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            let result: Result<lsp_types::CompletionItem, _> =
                server.request("completionItem/resolve", &item).await;

            if let Ok(resolved) = result {
                if let Some(edits) = resolved.additional_text_edits {
                    if !edits.is_empty() {
                        let domain_edits: Vec<crate::TextEdit> = edits
                            .iter()
                            .map(|e| lsp_text_edit_to_domain(e, &|_| None))
                            .collect();
                        let _ = event_tx.send(ManagerEvent::RequestResult(
                            RequestResult::CompletionResolved {
                                additional_edits: domain_edits,
                            },
                        ));
                    }
                }
            }
        });
    }

    fn spawn_rename(&self, path: CanonPath, row: Row, col: Col, new_name: String) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let cursor_line = self.line_at(&path, row);

        tokio::spawn(async move {
            let Some(uri) = uri_from_path(&path) else {
                return;
            };
            let params = RenameParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: lsp_pos(row, col, cursor_line.as_deref()),
                },
                new_name,
                work_done_progress_params: Default::default(),
            };

            let result: Result<Option<WorkspaceEdit>, _> =
                server.request("textDocument/rename", &params).await;

            match result {
                Ok(Some(edit)) => {
                    let file_edits = workspace_edit_to_file_edits(&edit);
                    let _ = event_tx.send(ManagerEvent::RequestResult(RequestResult::Rename {
                        file_edits,
                    }));
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = event_tx.send(ManagerEvent::RequestResult(RequestResult::Error {
                        message: e.message,
                    }));
                }
            }
        });
    }

    fn spawn_code_action(
        &self,
        path: CanonPath,
        start_row: Row,
        start_col: Col,
        end_row: Row,
        end_col: Col,
    ) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let start_line = self.line_at(&path, start_row);
        let end_line = if end_row == start_row {
            start_line.clone()
        } else {
            self.line_at(&path, end_row)
        };

        tokio::spawn(async move {
            let Some(uri) = uri_from_path(&path) else {
                return;
            };
            let params = CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range: Range {
                    start: lsp_pos(start_row, start_col, start_line.as_deref()),
                    end: lsp_pos(end_row, end_col, end_line.as_deref()),
                },
                context: lsp_types::CodeActionContext {
                    diagnostics: vec![],
                    only: None,
                    trigger_kind: Some(lsp_types::CodeActionTriggerKind::INVOKED),
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };

            let result: Result<Option<CodeActionResponse>, _> =
                server.request("textDocument/codeAction", &params).await;

            match result {
                Ok(Some(actions)) => {
                    let _ =
                        event_tx.send(ManagerEvent::RequestResult(RequestResult::CodeActions {
                            path,
                            raw: actions,
                        }));
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = event_tx.send(ManagerEvent::RequestResult(RequestResult::Error {
                        message: e.message,
                    }));
                }
            }
        });
    }

    fn spawn_code_action_resolve(&self, index: usize) {
        // Find the action to resolve — use the last set of code actions
        let Some((path, actions)) = self.pending_code_actions.iter().next() else {
            return;
        };
        let path = path.clone();
        let Some(action) = actions.get(index).cloned() else {
            return;
        };
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            match action {
                CodeActionOrCommand::CodeAction(mut ca) => {
                    if ca.edit.is_none() {
                        match server
                            .request::<_, lsp_types::CodeAction>("codeAction/resolve", &ca)
                            .await
                        {
                            Ok(resolved) => ca = resolved,
                            Err(e) => {
                                let _ = event_tx.send(ManagerEvent::RequestResult(
                                    RequestResult::Error { message: e.message },
                                ));
                                return;
                            }
                        }
                    }
                    if let Some(edit) = ca.edit {
                        let file_edits = workspace_edit_to_file_edits(&edit);
                        let _ = event_tx.send(ManagerEvent::RequestResult(
                            RequestResult::CodeActionResolved { file_edits },
                        ));
                    }
                }
                CodeActionOrCommand::Command(_) => {
                    // Command execution not supported
                }
            }
        });
    }

    fn spawn_format(&self, path: CanonPath, doc_content: Option<Vec<u8>>) {
        let Some(server) = self.server_for_path(&path) else {
            // No server → send FormatDone immediately so save proceeds
            let _ = self
                .event_tx
                .send(ManagerEvent::RequestResult(RequestResult::FormatDone));
            return;
        };
        let event_tx = self.event_tx.clone();
        let prettier = find_prettier(&path);

        tokio::spawn(async move {
            let Some(uri) = uri_from_path(&path) else {
                return;
            };
            let t0 = std::time::Instant::now();

            // Format document
            let t1 = std::time::Instant::now();
            if let Some(ref prettier_bin) = prettier {
                // Prettier: pipe buffer content through CLI
                if let Some(content) = doc_content {
                    if let Some(edits) = run_prettier(prettier_bin, &path, &content).await {
                        let _ =
                            event_tx.send(ManagerEvent::RequestResult(RequestResult::FormatRaw {
                                path: path.clone(),
                                edits,
                            }));
                    }
                }
            } else {
                // LSP formatting
                let fmt_params = DocumentFormattingParams {
                    text_document: TextDocumentIdentifier { uri },
                    options: FormattingOptions {
                        tab_size: 4,
                        insert_spaces: true,
                        ..Default::default()
                    },
                    work_done_progress_params: Default::default(),
                };

                let result: Result<Option<Vec<TextEdit>>, _> =
                    server.request("textDocument/formatting", &fmt_params).await;

                match result {
                    Ok(Some(edits)) if !edits.is_empty() => {
                        let _ = event_tx.send(ManagerEvent::RequestResult(RequestResult::Format {
                            path: path.clone(),
                            edits,
                        }));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let _ = event_tx.send(ManagerEvent::RequestResult(RequestResult::Error {
                            message: e.message,
                        }));
                    }
                }
            }

            log::info!(
                "format: formatting took {:?}, total {:?}",
                t1.elapsed(),
                t0.elapsed()
            );
            // Always signal format done so save-after-format can proceed
            let _ = event_tx.send(ManagerEvent::RequestResult(RequestResult::FormatDone));
        });
    }

    fn spawn_inlay_hints(&self, path: CanonPath, start_row: Row, end_row: Row) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            let Some(uri) = uri_from_path(&path) else {
                return;
            };
            let params = InlayHintParams {
                text_document: TextDocumentIdentifier { uri },
                range: Range {
                    start: lsp_pos(start_row, Col(0), None),
                    end: lsp_pos(end_row, Col(0), None),
                },
                work_done_progress_params: Default::default(),
            };

            let result: Result<Option<Vec<lsp_types::InlayHint>>, _> =
                server.request("textDocument/inlayHint", &params).await;

            let hints = match result {
                Ok(Some(hints)) => hints,
                _ => vec![],
            };

            let _ = event_tx.send(ManagerEvent::RequestResult(RequestResult::InlayHints {
                path,
                hints,
            }));
        });
    }

    // ── Notification handling ──

    async fn handle_notification(
        &mut self,
        language: LanguageId,
        notif: LspNotification,
        result_tx: &tokio::sync::mpsc::Sender<LspIn>,
    ) {
        match notif.method.as_str() {
            "textDocument/publishDiagnostics" => {
                if let Ok(params) =
                    serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(notif.params)
                {
                    if let Some(path) = crate::convert::path_from_uri(&params.uri) {
                        let line_at = |row: usize| {
                            self.docs
                                .get(&path)
                                .and_then(|d| doc_line(&**d, Row(row)))
                                .or_else(|| {
                                    std::fs::read_to_string(path.as_path())
                                        .ok()
                                        .and_then(|c| c.lines().nth(row).map(|l| l.to_string()))
                                })
                        };
                        let diagnostics = convert_diagnostics(&params.diagnostics, &line_at);
                        log::trace!(
                            "diag: push for {}, {} diags",
                            path.display(),
                            diagnostics.len(),
                        );
                        match self.diag_source.on_push(path, diagnostics) {
                            DiagPushResult::Forward(p, diags, h) => {
                                log::trace!(
                                    "diag: forwarding push {} diags for {}, hash={:#x}",
                                    diags.len(),
                                    p.display(),
                                    h.0,
                                );
                                let _ = result_tx
                                    .send(LspIn::Diagnostics {
                                        path: p,
                                        diagnostics: diags,
                                        content_hash: h,
                                    })
                                    .await;
                            }
                            DiagPushResult::RestartWindow => {
                                // Mode switched from pull to push — reopen window
                                // and drain cache (includes the push that triggered this).
                                self.diag_source.open_window(&self.docs, &self.opened_docs);
                                for (p, diags, h) in self.diag_source.drain_cache_for_window() {
                                    log::trace!(
                                        "diag: forwarding cached {} diags for {}, hash={:#x}",
                                        diags.len(),
                                        p.display(),
                                        h.0,
                                    );
                                    let _ = result_tx
                                        .send(LspIn::Diagnostics {
                                            path: p,
                                            diagnostics: diags,
                                            content_hash: h,
                                        })
                                        .await;
                                }
                            }
                            DiagPushResult::ForwardClearing(p) => {
                                // Window closed but the push clears errors —
                                // forward with current persisted hash.
                                let h = self
                                    .docs
                                    .get(&p)
                                    .map(|d| PersistedContentHash(d.content_hash().0))
                                    .unwrap_or(PersistedContentHash(0));
                                log::trace!(
                                    "diag: forwarding clearing push for {}, hash={:#x}",
                                    p.display(),
                                    h.0,
                                );
                                let _ = result_tx
                                    .send(LspIn::Diagnostics {
                                        path: p,
                                        diagnostics: vec![],
                                        content_hash: h,
                                    })
                                    .await;
                            }
                            DiagPushResult::Ignore => {}
                        }
                    }
                }
            }
            "$/progress" => {
                if let Ok(params) =
                    serde_json::from_value::<lsp_types::ProgressParams>(notif.params)
                {
                    let token = match &params.token {
                        NumberOrString::Number(n) => n.to_string(),
                        NumberOrString::String(s) => s.clone(),
                    };
                    let update = classify_progress(&params.value);
                    self.apply_progress_update(&token, update);
                    self.send_progress_throttled(result_tx).await;
                }
            }
            "experimental/serverStatus" => {
                let quiescent = notif.params.get("quiescent").and_then(|v| v.as_bool());
                log::trace!(
                    "diag: serverStatus quiescent={:?} language={:?}",
                    quiescent,
                    language,
                );
                if !self.diag_source.has_quiescence {
                    // First serverStatus → server supports quiescence, not ready yet
                    self.diag_source.has_quiescence = true;
                    self.diag_source.lsp_ready = false;
                    log::info!("diag: server supports quiescence, deferring until ready");
                }
                if let Some(q) = quiescent {
                    let was_busy = !*self.quiescent.get(&language).unwrap_or(&true);
                    self.quiescent.insert(language, q);
                    if was_busy && q && self.diag_source.on_quiescence() {
                        self.open_diag_window(result_tx).await;
                    }
                    self.send_progress_throttled(result_tx).await;
                }
            }
            "client/registerCapability" => {
                self.handle_register_capability(&notif.params);
            }
            "$/stderr" => {
                if let Some(msg) = notif.params.as_str() {
                    let _ = result_tx
                        .send(LspIn::Error {
                            message: msg.to_string(),
                        })
                        .await;
                }
            }
            _ => {
                log::debug!("LSP unhandled notification: {}", notif.method);
            }
        }
    }

    // ── Request result handling ──

    async fn handle_request_result(
        &mut self,
        result: RequestResult,
        result_tx: &tokio::sync::mpsc::Sender<LspIn>,
    ) {
        match result {
            RequestResult::GotoDefinition { locations } => {
                if let Some((path, row, col)) = locations.into_iter().next() {
                    let _ = result_tx.send(LspIn::Navigate { path, row, col }).await;
                }
            }
            RequestResult::Format { path, edits } => {
                let line_at =
                    |row: usize| self.docs.get(&path).and_then(|d| doc_line(&**d, Row(row)));
                let domain_edits: Vec<crate::TextEdit> = edits
                    .iter()
                    .map(|e| lsp_text_edit_to_domain(e, &line_at))
                    .collect();
                // Always send Edits (even empty) so format-on-save completes
                let _ = result_tx
                    .send(LspIn::Edits {
                        edits: vec![FileEdit {
                            path,
                            edits: domain_edits,
                        }],
                    })
                    .await;
            }
            RequestResult::FormatRaw { path, edits } => {
                let _ = result_tx
                    .send(LspIn::Edits {
                        edits: vec![FileEdit { path, edits }],
                    })
                    .await;
            }
            RequestResult::Rename { file_edits } => {
                // Apply non-open file edits to disk
                let mut open_edits = Vec::new();
                for fe in file_edits {
                    if self.opened_docs.contains(&fe.path) {
                        open_edits.push(fe);
                    } else {
                        apply_edits_to_disk(&fe.path, &fe.edits);
                    }
                }
                if !open_edits.is_empty() {
                    let _ = result_tx.send(LspIn::Edits { edits: open_edits }).await;
                }
            }
            RequestResult::CodeActions { path, raw } => {
                if !raw.is_empty() {
                    let titles = code_action_titles(&raw);
                    self.pending_code_actions.insert(path, raw);
                    let _ = result_tx.send(LspIn::CodeActions { actions: titles }).await;
                }
            }
            RequestResult::CodeActionResolved { file_edits } => {
                let mut open_edits = Vec::new();
                for fe in file_edits {
                    if self.opened_docs.contains(&fe.path) {
                        open_edits.push(fe);
                    } else {
                        apply_edits_to_disk(&fe.path, &fe.edits);
                    }
                }
                if !open_edits.is_empty() {
                    let _ = result_tx.send(LspIn::Edits { edits: open_edits }).await;
                }
            }
            RequestResult::InlayHints { path, hints } => {
                let line_at =
                    |row: usize| self.docs.get(&path).and_then(|d| doc_line(&**d, Row(row)));
                let domain_hints = convert_inlay_hints(hints, &line_at);
                let _ = result_tx
                    .send(LspIn::InlayHints {
                        path,
                        hints: domain_hints,
                    })
                    .await;
            }
            RequestResult::Diagnostics { path, raw } => {
                let line_at =
                    |row: usize| self.docs.get(&path).and_then(|d| doc_line(&**d, Row(row)));
                let diagnostics = convert_diagnostics(&raw, &line_at);
                let (forward, _all_done) = self.diag_source.on_pull_response(path, diagnostics);
                if let Some((p, diags, h)) = forward {
                    log::trace!(
                        "diag: forwarding pull {} diags for {}, hash={:#x}",
                        diags.len(),
                        p.display(),
                        h.0,
                    );
                    let _ = result_tx
                        .send(LspIn::Diagnostics {
                            path: p,
                            diagnostics: diags,
                            content_hash: h,
                        })
                        .await;
                }
            }
            RequestResult::Completion {
                path,
                response,
                row,
                col,
                seq,
            } => {
                // Ignore stale responses from earlier requests
                if seq != self.completion_seq {
                    return;
                }
                let line_at = |r: usize| self.docs.get(&path).and_then(|d| doc_line(&**d, Row(r)));

                // Store raw items for resolve
                let raw_items = match &response {
                    CompletionResponse::Array(items) => items.clone(),
                    CompletionResponse::List(list) => list.items.clone(),
                };
                self.completion_items = raw_items;

                let (items, prefix_start_col) =
                    convert_completion_response(response, row, col, &line_at);

                // Store session for re-filtering
                self.completion_path = Some(path.clone());
                self.completion_row = row;
                self.completion_prefix_start_col = prefix_start_col;
                self.completion_domain_items = items;

                // Apply initial filter against current doc (user may have typed
                // more chars since the request was sent)
                self.refilter_completion(&path, &[], result_tx).await;
            }
            RequestResult::CompletionResolved { additional_edits } => {
                // Find the path for these edits — use the first opened doc path
                if let Some(path) = self.opened_docs.iter().next().cloned() {
                    let _ = result_tx
                        .send(LspIn::Edits {
                            edits: vec![FileEdit {
                                path,
                                edits: additional_edits,
                            }],
                        })
                        .await;
                }
            }
            RequestResult::FormatDone => {
                // Send empty edits to trigger pending save-after-format
                let _ = result_tx.send(LspIn::Edits { edits: vec![] }).await;
            }
            RequestResult::Error { message } => {
                let _ = result_tx.send(LspIn::Error { message }).await;
            }
        }
    }

    // ── Progress ──

    fn apply_progress_update(&mut self, token: &str, update: ProgressUpdate) {
        match update {
            ProgressUpdate::Begin {
                title,
                message,
                percentage,
            } => {
                self.progress_tokens.insert(
                    token.to_string(),
                    ProgressState {
                        title,
                        message,
                        percentage,
                    },
                );
            }
            ProgressUpdate::Report {
                message,
                percentage,
            } => {
                if let Some(state) = self.progress_tokens.get_mut(token) {
                    if message.is_some() {
                        state.message = message;
                    }
                    if percentage.is_some() {
                        state.percentage = percentage;
                    }
                }
            }
            ProgressUpdate::End => {
                self.progress_tokens.remove(token);
            }
        }
    }

    fn is_busy(&self) -> bool {
        self.quiescent.values().any(|q| !q) || !self.progress_tokens.is_empty()
    }

    /// Send progress to UI at most once per 200ms to avoid flooding.
    /// Always sends immediately on busy→idle transitions.
    async fn send_progress_throttled(&mut self, result_tx: &tokio::sync::mpsc::Sender<LspIn>) {
        let now = std::time::Instant::now();
        let is_idle = !self.is_busy();
        let elapsed = now.duration_since(self.last_progress_sent);
        if is_idle || elapsed >= std::time::Duration::from_millis(200) {
            self.last_progress_sent = now;
            let lsp_in = self.progress_lsp_in();
            let _ = result_tx.send(lsp_in).await;
        }
    }

    fn progress_lsp_in(&self) -> LspIn {
        let server_name = self
            .servers
            .values()
            .next()
            .map(|s| s.name.clone())
            .unwrap_or_default();
        let busy = self.is_busy();
        let detail = self.progress_tokens.values().next().map(|p| {
            if let Some(ref msg) = p.message {
                format!("{} {}", p.title, msg)
            } else {
                p.title.clone()
            }
        });
        LspIn::Progress {
            server_name,
            busy,
            detail,
        }
    }

    // ── Completion auto-trigger ──

    fn check_trigger_char(&self, path: &CanonPath, edit_ops: &[EditOp]) -> bool {
        if self.trigger_characters.is_empty() {
            return false;
        }
        if self.server_for_path(path).is_none() {
            return false;
        }
        // Check if the last edit's new_text ends with a trigger character
        let Some(op) = edit_ops.last() else {
            return false;
        };
        if op.new_text.is_empty() {
            return false;
        }
        let tail = &op.new_text;
        self.trigger_characters
            .iter()
            .any(|t| tail.ends_with(t.as_str()))
    }

    // ── Completion re-filter ──

    async fn refilter_completion(
        &mut self,
        changed_path: &CanonPath,
        edit_ops: &[EditOp],
        result_tx: &tokio::sync::mpsc::Sender<LspIn>,
    ) {
        let Some(ref comp_path) = self.completion_path else {
            return;
        };
        if comp_path != changed_path {
            return;
        }
        if self.completion_domain_items.is_empty() {
            return;
        }
        let Some(doc) = self.docs.get(changed_path) else {
            return;
        };

        let row = self.completion_row;
        if *row >= doc.line_count() {
            self.clear_completion(result_tx).await;
            return;
        }

        let line_chars: Vec<char> = led_core::with_line_buf(|line| {
            doc.line(row, line);
            let t = line.trim_end_matches(&['\n', '\r'][..]).len();
            line.truncate(t);
            line.chars().collect()
        });
        let psc = *self.completion_prefix_start_col;

        // Line got shorter than prefix start → dismiss
        if psc > line_chars.len() {
            self.clear_completion(result_tx).await;
            return;
        }

        // Check if the edit position is within the identifier at psc.
        // If the user is editing a different part of the line (e.g., typed "if"
        // then moved on to type "LspI"), the session is stale — dismiss it.
        if let Some(op) = edit_ops.last() {
            let edit_pos = op.offset.0 + op.new_text.chars().count();
            let last_row = Row(doc.line_count().saturating_sub(1));
            let last_char = doc.line_to_char(last_row).0 + doc.line_len(last_row);
            let edit_row = doc.char_to_line(CharOffset(edit_pos.min(last_char)));
            let edit_col = edit_pos - doc.line_to_char(edit_row).0;
            // Find the end of the identifier at psc
            let mut id_end = psc;
            while id_end < line_chars.len()
                && (line_chars[id_end].is_alphanumeric() || line_chars[id_end] == '_')
            {
                id_end += 1;
            }
            // Edit is on a different row, or outside the identifier range → stale
            if edit_row != row || edit_col < psc || edit_col > id_end {
                self.clear_completion(result_tx).await;
                return;
            }
        }

        // Find the end of the identifier from prefix_start_col
        let mut cursor_col = psc;
        while cursor_col < line_chars.len()
            && (line_chars[cursor_col].is_alphanumeric() || line_chars[cursor_col] == '_')
        {
            cursor_col += 1;
        }

        let prefix: String = line_chars[psc..cursor_col].iter().collect();

        // Empty prefix → send all items unfiltered
        if prefix.is_empty() {
            let _ = result_tx
                .send(LspIn::Completion {
                    items: self.completion_domain_items.clone(),
                    prefix_start_col: Col(psc),
                })
                .await;
            return;
        }

        let filtered = fuzzy_filter_completions(&self.completion_domain_items, &prefix);

        if filtered.is_empty() {
            self.clear_completion(result_tx).await;
        } else {
            let _ = result_tx
                .send(LspIn::Completion {
                    items: filtered,
                    prefix_start_col: Col(psc),
                })
                .await;
        }
    }

    async fn clear_completion(&mut self, result_tx: &tokio::sync::mpsc::Sender<LspIn>) {
        self.completion_path = None;
        self.completion_domain_items.clear();
        let _ = result_tx
            .send(LspIn::Completion {
                items: vec![],
                prefix_start_col: Col(0),
            })
            .await;
    }

    // ── File watcher ──

    fn handle_register_capability(&mut self, params: &Value) {
        let Some(registrations) = params.get("registrations").and_then(|r| r.as_array()) else {
            return;
        };

        for reg in registrations {
            let method = reg.get("method").and_then(|m| m.as_str()).unwrap_or("");
            if method != "workspace/didChangeWatchedFiles" {
                continue;
            }

            let Some(watchers) = reg
                .get("registerOptions")
                .and_then(|o| o.get("watchers"))
                .and_then(|w| w.as_array())
            else {
                continue;
            };

            let mut builder = globset::GlobSetBuilder::new();
            for w in watchers {
                let Some(pattern) = w.get("globPattern").and_then(|g| g.as_str()) else {
                    continue;
                };
                if let Ok(glob) = globset::Glob::new(pattern) {
                    builder.add(glob);
                }
            }

            let Ok(glob_set) = builder.build() else {
                continue;
            };

            log::info!("LSP file watcher: {} patterns registered", glob_set.len());
            self.file_watcher_globs = Some(glob_set);
            self.start_file_watcher();
        }
    }

    fn start_file_watcher(&mut self) {
        use notify::{RecursiveMode, Watcher};

        let event_tx = self.event_tx.clone();
        let globs = match self.file_watcher_globs.as_ref() {
            Some(g) => g.clone(),
            None => return,
        };
        let root = self.root.clone();

        let watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
            let Ok(ev) = res else { return };
            let kind = match ev.kind {
                notify::EventKind::Create(_) => FileChangeKind::Created,
                notify::EventKind::Modify(_) => FileChangeKind::Changed,
                notify::EventKind::Remove(_) => FileChangeKind::Deleted,
                _ => return,
            };
            for path in ev.paths {
                if globs.is_match(&path) {
                    let canon = led_core::UserPath::new(path).canonicalize();
                    let _ = event_tx.send(ManagerEvent::FileChanged(canon, kind));
                }
            }
        });

        match watcher {
            Ok(mut w) => {
                if let Err(e) = w.watch(root.as_path(), RecursiveMode::Recursive) {
                    log::warn!(
                        "LSP file watcher: failed to watch {}: {}",
                        root.display(),
                        e
                    );
                    return;
                }
                log::info!("LSP file watcher: watching {}", root.display());
                self._file_watcher = Some(w);
            }
            Err(e) => {
                log::warn!("LSP file watcher: failed to create: {}", e);
            }
        }
    }

    fn send_file_changed(&self, path: &CanonPath, kind: FileChangeKind) {
        let Some(uri) = uri_from_path(path) else {
            return;
        };
        let change_type = match kind {
            FileChangeKind::Created => lsp_types::FileChangeType::CREATED,
            FileChangeKind::Changed => lsp_types::FileChangeType::CHANGED,
            FileChangeKind::Deleted => lsp_types::FileChangeType::DELETED,
        };
        for server in self.servers.values() {
            server.notify(
                "workspace/didChangeWatchedFiles",
                &lsp_types::DidChangeWatchedFilesParams {
                    changes: vec![lsp_types::FileEvent {
                        uri: uri.clone(),
                        typ: change_type,
                    }],
                },
            );
        }
    }

    // ── Diagnostic window ──

    /// Open a diagnostic window: forward cached push results immediately,
    /// issue pulls for paths without cache.
    async fn open_diag_window(&mut self, result_tx: &tokio::sync::mpsc::Sender<LspIn>) {
        let pull_paths = self.diag_source.open_window(&self.docs, &self.opened_docs);

        // Forward cached push results for all paths that have them.
        if self.diag_source.mode == DiagMode::Push {
            for (p, diags, h) in self.diag_source.drain_cache_for_window() {
                log::trace!(
                    "diag: forwarding cached {} diags for {}, hash={:#x}",
                    diags.len(),
                    p.display(),
                    h.0,
                );
                let _ = result_tx
                    .send(LspIn::Diagnostics {
                        path: p,
                        diagnostics: diags,
                        content_hash: h,
                    })
                    .await;
            }
        }

        // Issue pulls for paths without cache (or all paths in pull-only mode)
        for path in &pull_paths {
            if let Some(server) = self.server_for_path(path) {
                self.spawn_pull_diagnostics(path.clone(), server);
            }
        }
    }

    fn spawn_pull_diagnostics(&self, path: CanonPath, server: Arc<LanguageServer>) {
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            let Some(uri) = uri_from_path(&path) else {
                return;
            };
            let params = lsp_types::DocumentDiagnosticParams {
                text_document: TextDocumentIdentifier { uri },
                identifier: None,
                previous_result_id: None,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };

            let result: Result<lsp_types::DocumentDiagnosticReportResult, _> =
                server.request("textDocument/diagnostic", &params).await;

            let raw = match result {
                Ok(lsp_types::DocumentDiagnosticReportResult::Report(
                    lsp_types::DocumentDiagnosticReport::Full(report),
                )) => report.full_document_diagnostic_report.items,
                Ok(_) => vec![],
                Err(e) => {
                    log::debug!(
                        "LSP pull diagnostics failed for {}: {}",
                        path.display(),
                        e.message
                    );
                    vec![]
                }
            };

            let _ = event_tx.send(ManagerEvent::RequestResult(RequestResult::Diagnostics {
                path,
                raw,
            }));
        });
    }

    // ── Helpers ──

    fn line_at(&self, path: &CanonPath, row: Row) -> Option<String> {
        self.docs.get(path).and_then(|d| doc_line(&**d, row))
    }
}

// ── Free functions ──

fn classify_progress(value: &lsp_types::ProgressParamsValue) -> ProgressUpdate {
    match value {
        lsp_types::ProgressParamsValue::WorkDone(wd) => match wd {
            lsp_types::WorkDoneProgress::Begin(begin) => ProgressUpdate::Begin {
                title: begin.title.clone(),
                message: begin.message.clone(),
                percentage: begin.percentage,
            },
            lsp_types::WorkDoneProgress::Report(report) => {
                if report.percentage == Some(100) {
                    ProgressUpdate::End
                } else {
                    ProgressUpdate::Report {
                        message: report.message.clone(),
                        percentage: report.percentage,
                    }
                }
            }
            lsp_types::WorkDoneProgress::End(_) => ProgressUpdate::End,
        },
    }
}

fn fuzzy_filter_completions(
    items: &[crate::CompletionItem],
    query: &str,
) -> Vec<crate::CompletionItem> {
    use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher, Utf32Str};

    if query.is_empty() {
        return items.to_vec();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(
        query,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut buf = Vec::new();

    let mut scored: Vec<(usize, u32)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let text = item.filter_text.as_deref().unwrap_or(&item.label);
            let haystack = Utf32Str::new(text, &mut buf);
            let score = pattern.score(haystack, &mut matcher)?;
            Some((i, score))
        })
        .collect();

    scored.sort_by(|a, b| {
        b.1.cmp(&a.1).then_with(|| {
            let a_key = items[a.0].sort_text.as_deref().unwrap_or(&items[a.0].label);
            let b_key = items[b.0].sort_text.as_deref().unwrap_or(&items[b.0].label);
            a_key.cmp(b_key)
        })
    });

    scored.into_iter().map(|(i, _)| items[i].clone()).collect()
}

// ── Prettier integration ──

/// Walk up from the file's directory looking for `node_modules/.bin/prettier`.
fn find_prettier(file_path: &CanonPath) -> Option<std::path::PathBuf> {
    let mut dir = file_path.as_path().parent()?;
    loop {
        let bin = dir.join("node_modules/.bin/prettier");
        if bin.exists() {
            return Some(bin);
        }
        dir = dir.parent()?;
    }
}

/// Run prettier on buffer content and return a full-file replacement edit.
async fn run_prettier(
    prettier_bin: &Path,
    file_path: &CanonPath,
    content: &[u8],
) -> Option<Vec<crate::TextEdit>> {
    use tokio::process::Command;

    let mut child = Command::new(prettier_bin)
        .arg("--stdin-filepath")
        .arg(file_path.as_path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;

    // Write content to stdin
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(content).await;
        drop(stdin);
    }

    let output = child.wait_with_output().await.ok()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!("prettier failed: {}", stderr);
        return None;
    }

    let formatted = String::from_utf8(output.stdout).ok()?;

    // No change — skip
    let original = std::str::from_utf8(content).ok()?;
    if formatted == original {
        return None;
    }

    // Count lines in the original to build the replacement range
    let line_count = original.lines().count();
    let last_line = if line_count == 0 { 0 } else { line_count - 1 };
    let last_col = original
        .lines()
        .last()
        .map(|l| l.chars().count())
        .unwrap_or(0);

    Some(vec![crate::TextEdit {
        start_row: Row(0),
        start_col: Col(0),
        end_row: Row(last_line),
        end_col: Col(last_col),
        new_text: formatted,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::InertDoc;

    fn path(s: &str) -> CanonPath {
        led_core::UserPath::new(s).canonicalize()
    }

    fn mock_docs(paths: &[&str]) -> HashMap<CanonPath, Arc<dyn Doc>> {
        paths
            .iter()
            .map(|s| (path(s), Arc::new(InertDoc) as Arc<dyn Doc>))
            .collect()
    }

    fn opened(paths: &[&str]) -> HashSet<CanonPath> {
        paths.iter().map(|s| path(s)).collect()
    }

    fn diag(msg: &str) -> crate::Diagnostic {
        crate::Diagnostic {
            start_row: Row(0),
            start_col: Col(0),
            end_row: Row(0),
            end_col: Col(5),
            severity: crate::DiagnosticSeverity::Error,
            message: msg.to_string(),
            source: None,
            code: None,
        }
    }

    fn push_source() -> DiagnosticSource {
        let mut ds = DiagnosticSource::new();
        ds.set_mode(DiagMode::Push);
        ds
    }

    fn pull_source() -> DiagnosticSource {
        let mut ds = DiagnosticSource::new();
        ds.set_mode(DiagMode::Pull);
        ds
    }

    // ── Push mode tests ──

    #[test]
    fn push_always_caches() {
        let mut ds = push_source();
        ds.on_push(path("/a.rs"), vec![diag("err")]);
        assert_eq!(ds.push_cache.get(&path("/a.rs")).unwrap()[0].message, "err");
    }

    #[test]
    fn push_cache_updated_by_new_push() {
        let mut ds = push_source();
        ds.on_push(path("/a.rs"), vec![diag("old")]);
        ds.on_push(path("/a.rs"), vec![diag("new")]);
        assert_eq!(ds.push_cache.get(&path("/a.rs")).unwrap()[0].message, "new");
    }

    #[test]
    fn empty_push_clears_cache_entry() {
        let mut ds = push_source();
        ds.on_push(path("/a.rs"), vec![diag("err")]);
        ds.on_push(path("/a.rs"), vec![]);
        assert!(ds.push_cache.get(&path("/a.rs")).unwrap().is_empty());
    }

    #[test]
    fn push_ignored_without_window() {
        let mut ds = push_source();
        let result = ds.on_push(path("/a.rs"), vec![diag("err")]);
        assert!(matches!(result, DiagPushResult::Ignore));
    }

    #[test]
    fn push_forwarded_with_window() {
        let mut ds = push_source();
        let docs = mock_docs(&["/a.rs"]);
        let opened = opened(&["/a.rs"]);
        ds.open_window(&docs, &opened);

        let result = ds.on_push(path("/a.rs"), vec![diag("err")]);
        assert!(matches!(result, DiagPushResult::Forward(..)));
    }

    #[test]
    fn push_window_drains_cache() {
        let mut ds = push_source();
        ds.on_push(path("/a.rs"), vec![diag("cached")]);

        let docs = mock_docs(&["/a.rs"]);
        let opened = opened(&["/a.rs"]);
        ds.open_window(&docs, &opened);

        let drained = ds.drain_cache_for_window();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].1[0].message, "cached");
    }

    #[test]
    fn push_cache_survives_window_close() {
        let mut ds = push_source();
        ds.on_push(path("/a.rs"), vec![diag("cached")]);

        let docs = mock_docs(&["/a.rs"]);
        let opened = opened(&["/a.rs"]);
        ds.open_window(&docs, &opened);
        ds.close_window();

        // Cache still has the entry
        assert_eq!(
            ds.push_cache.get(&path("/a.rs")).unwrap()[0].message,
            "cached"
        );
    }

    #[test]
    fn push_window_not_frozen() {
        let mut ds = push_source();
        let docs = mock_docs(&["/a.rs"]);
        let opened = opened(&["/a.rs"]);
        ds.open_window(&docs, &opened);
        assert!(!ds.is_frozen());
    }

    #[test]
    fn push_window_closes_on_content_change() {
        let mut ds = push_source();
        let docs = mock_docs(&["/a.rs"]);
        let opened = opened(&["/a.rs"]);
        ds.open_window(&docs, &opened);

        // InertDoc has hash 0. Any real doc would differ.
        let real_doc: Arc<dyn Doc> =
            Arc::new(led_core::TextDoc::from_reader(std::io::Cursor::new(b"changed\n")).unwrap());
        assert!(ds.should_close_window(&path("/a.rs"), &real_doc));
    }

    // ── Pull mode tests ──

    #[test]
    fn pull_window_is_frozen() {
        let mut ds = pull_source();
        let docs = mock_docs(&["/a.rs"]);
        let opened = opened(&["/a.rs"]);
        ds.open_window(&docs, &opened);
        assert!(ds.is_frozen());
    }

    #[test]
    fn pull_window_returns_all_paths() {
        let mut ds = pull_source();
        let docs = mock_docs(&["/a.rs", "/b.rs"]);
        let opened = opened(&["/a.rs", "/b.rs"]);
        let pull_paths = ds.open_window(&docs, &opened);
        assert_eq!(pull_paths.len(), 2);
    }

    #[test]
    fn pull_response_forwarded() {
        let mut ds = pull_source();
        let docs = mock_docs(&["/a.rs"]);
        let opened = opened(&["/a.rs"]);
        ds.open_window(&docs, &opened);

        let (result, all_done) = ds.on_pull_response(path("/a.rs"), vec![diag("pulled")]);
        assert!(result.is_some());
        assert!(all_done);
        assert_eq!(result.unwrap().1[0].message, "pulled");
        // Freeze lifted
        assert!(!ds.is_frozen());
    }

    #[test]
    fn pull_unfreezes_when_all_done() {
        let mut ds = pull_source();
        let docs = mock_docs(&["/a.rs", "/b.rs"]);
        let opened = opened(&["/a.rs", "/b.rs"]);
        ds.open_window(&docs, &opened);
        assert!(ds.is_frozen());

        let (_, done) = ds.on_pull_response(path("/a.rs"), vec![]);
        assert!(!done);
        assert!(ds.is_frozen());

        let (_, done) = ds.on_pull_response(path("/b.rs"), vec![]);
        assert!(done);
        assert!(!ds.is_frozen());
    }

    #[test]
    fn pull_cancel_freeze_unfreezes() {
        let mut ds = pull_source();
        let docs = mock_docs(&["/a.rs"]);
        let opened = opened(&["/a.rs"]);
        ds.open_window(&docs, &opened);
        assert!(ds.is_frozen());

        ds.cancel_freeze();
        assert!(!ds.is_frozen());
        // Window still open for late results
        assert!(ds.has_window());
    }

    #[test]
    fn pull_switches_to_push_on_first_push() {
        let mut ds = pull_source();
        assert_eq!(ds.mode, DiagMode::Pull);
        let result = ds.on_push(path("/a.rs"), vec![diag("pushed")]);
        assert_eq!(ds.mode, DiagMode::Push);
        // No window was open → Ignore (not RestartWindow)
        assert!(matches!(result, DiagPushResult::Ignore));
        // Cache updated
        assert_eq!(
            ds.push_cache.get(&path("/a.rs")).unwrap()[0].message,
            "pushed"
        );
    }

    #[test]
    fn pull_switches_to_push_restarts_window() {
        let mut ds = pull_source();
        let docs = mock_docs(&["/a.rs"]);
        let opened = opened(&["/a.rs"]);
        ds.open_window(&docs, &opened);
        assert!(ds.is_frozen()); // pull window is frozen

        let result = ds.on_push(path("/a.rs"), vec![diag("pushed")]);
        assert_eq!(ds.mode, DiagMode::Push);
        assert!(matches!(result, DiagPushResult::RestartWindow));
        // Window was closed by the switch
        assert!(!ds.has_window());
    }

    // ── Default mode tests ──

    #[test]
    fn default_mode_is_push() {
        let ds = DiagnosticSource::new();
        assert_eq!(ds.mode, DiagMode::Push);
    }
}
