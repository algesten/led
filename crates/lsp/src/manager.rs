use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use led_core::{CharOffset, ContentHash, Doc, EditOp, LanguageId, Row};
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
    FileChanged(PathBuf, FileChangeKind),
}

enum RequestResult {
    GotoDefinition {
        locations: Vec<(PathBuf, usize, usize)>,
    },
    Format {
        path: PathBuf,
        edits: Vec<TextEdit>,
    },
    Rename {
        file_edits: Vec<FileEdit>,
    },
    CodeActions {
        path: PathBuf,
        raw: Vec<CodeActionOrCommand>,
    },
    CodeActionResolved {
        file_edits: Vec<FileEdit>,
    },
    InlayHints {
        path: PathBuf,
        hints: Vec<lsp_types::InlayHint>,
    },
    Diagnostics {
        path: PathBuf,
        raw: Vec<lsp_types::Diagnostic>,
    },
    Completion {
        path: PathBuf,
        response: CompletionResponse,
        row: usize,
        col: usize,
        seq: u64,
    },
    CompletionResolved {
        additional_edits: Vec<crate::TextEdit>,
    },
    FormatRaw {
        path: PathBuf,
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
    root: PathBuf,
    event_tx: tokio::sync::mpsc::UnboundedSender<ManagerEvent>,
    pending_starts: HashSet<LanguageId>,
    opened_docs: HashSet<PathBuf>,
    /// Reverse map: canonical path → original path (for macOS /var → /private/var).
    canonical_to_original: HashMap<PathBuf, PathBuf>,
    pending_opens: HashSet<PathBuf>,
    docs: HashMap<PathBuf, Arc<dyn Doc>>,
    doc_versions: HashMap<PathBuf, i32>,
    pending_code_actions: HashMap<PathBuf, Vec<CodeActionOrCommand>>,
    completion_items: Vec<lsp_types::CompletionItem>,
    /// Active completion session for re-filtering on BufferChanged
    completion_path: Option<PathBuf>,
    completion_row: usize,
    completion_prefix_start_col: usize,
    /// Domain items from last server response (unfiltered)
    completion_domain_items: Vec<crate::CompletionItem>,
    progress_tokens: HashMap<String, ProgressState>,
    quiescent: HashMap<LanguageId, bool>,
    need_diagnostics: bool,
    buffered_diagnostics: HashMap<PathBuf, Vec<crate::Diagnostic>>,
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
        root: PathBuf::new(),
        event_tx,
        pending_starts: HashSet::new(),
        opened_docs: HashSet::new(),
        canonical_to_original: HashMap::new(),
        pending_opens: HashSet::new(),
        docs: HashMap::new(),
        doc_versions: HashMap::new(),
        pending_code_actions: HashMap::new(),
        completion_items: Vec::new(),
        completion_path: None,
        completion_row: 0,
        completion_prefix_start_col: 0,
        completion_domain_items: Vec::new(),
        progress_tokens: HashMap::new(),
        quiescent: HashMap::new(),
        need_diagnostics: false,
        buffered_diagnostics: HashMap::new(),
        _file_watcher: None,
        file_watcher_globs: None,
        last_progress_sent: std::time::Instant::now(),
        trigger_characters: Vec::new(),
        completion_seq: 0,
    };

    loop {
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
            LspOut::BufferOpened { path, doc } => {
                self.docs.insert(path.clone(), doc);
                self.ensure_server_for_path(&path);
                self.send_did_open(&path);
            }
            LspOut::BufferChanged {
                path,
                doc,
                edit_ops,
                external,
            } => {
                let old_doc = self.docs.insert(path.clone(), doc);
                self.send_did_change(&path, &edit_ops, old_doc.as_deref());
                if external {
                    // The file is already saved on disk — send didSave so
                    // the server re-diagnoses, and flush any buffered diagnostics.
                    self.send_did_save(&path);
                    for (diag_path, diagnostics) in self.buffered_diagnostics.drain() {
                        let h = self
                            .docs
                            .get(&diag_path)
                            .map(|d| d.content_hash())
                            .unwrap_or(ContentHash(0));
                        let _ = result_tx
                            .send(LspIn::Diagnostics {
                                path: diag_path,
                                diagnostics,
                                content_hash: h,
                            })
                            .await;
                    }
                } else {
                    // Check if last edit was a trigger character → fresh completion
                    let triggered = self.check_trigger_char(&path, &edit_ops);
                    if triggered {
                        self.completion_path = None;
                        self.completion_domain_items.clear();
                        // Compute cursor position from edit_ops
                        if let Some(op) = edit_ops.last() {
                            let new_doc = self.docs.get(&path);
                            if let Some(d) = new_doc {
                                let cursor_offset =
                                    CharOffset(op.offset.0 + op.new_text.chars().count());
                                let row = d.char_to_line(cursor_offset);
                                let col = cursor_offset.0 - d.line_to_char(row).0;
                                self.spawn_completion(path.clone(), row.0, col);
                            }
                        }
                    } else {
                        // Re-filter active completion
                        self.refilter_completion(&path, &edit_ops, result_tx).await;
                    }
                }
            }
            LspOut::BufferSaved {
                path,
                content_hash: _,
            } => {
                self.send_did_save(&path);
                // Flush diagnostics that arrived while need_diagnostics was false
                // (e.g. from format-on-save didChange before didSave).
                for (diag_path, diagnostics) in self.buffered_diagnostics.drain() {
                    let h = self
                        .docs
                        .get(&diag_path)
                        .map(|d| d.content_hash())
                        .unwrap_or(ContentHash(0));
                    let _ = result_tx
                        .send(LspIn::Diagnostics {
                            path: diag_path,
                            diagnostics,
                            content_hash: h,
                        })
                        .await;
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
                let pending: Vec<PathBuf> = self.pending_opens.drain().collect();
                for path in pending {
                    self.send_did_open(&path);
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

    fn ensure_server_for_path(&mut self, path: &Path) {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let Some(config) = self.registry.config_for_extension(ext).cloned() else {
            return;
        };

        let language = config.language;
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

    fn server_for_path(&self, path: &Path) -> Option<Arc<LanguageServer>> {
        let ext = path.extension().and_then(|e| e.to_str())?;
        let config = self.registry.config_for_extension(ext)?;
        self.servers.get(&config.language).cloned()
    }

    async fn shutdown_all(&mut self) {
        for (_, server) in self.servers.drain() {
            server.shutdown().await;
        }
    }

    // ── Document sync (full-text) ──

    fn send_did_open(&mut self, path: &Path) {
        if self.opened_docs.contains(path) {
            return;
        }
        let Some(server) = self.server_for_path(path) else {
            self.pending_opens.insert(path.to_path_buf());
            return;
        };
        let Some(uri) = uri_from_path(path) else {
            return;
        };
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let lang_id = LanguageId::from_extension(ext)
            .map(|l| l.as_lsp_str())
            .unwrap_or("plaintext");

        let text = self
            .docs
            .get(path)
            .map(|d| doc_full_text(&**d))
            .unwrap_or_else(|| std::fs::read_to_string(path).unwrap_or_default());

        let version = self.next_version(path);

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
        self.opened_docs.insert(path.to_path_buf());
        if let Ok(canonical) = std::fs::canonicalize(path) {
            if canonical != path {
                self.canonical_to_original
                    .insert(canonical, path.to_path_buf());
            }
        }
        self.need_diagnostics = true;
    }

    fn send_did_change(&mut self, path: &Path, edit_ops: &[EditOp], old_doc: Option<&dyn Doc>) {
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
                let start_line_text = doc_line(old, start_line.0);
                let end_line_text = if end_line == start_line {
                    start_line_text.clone()
                } else {
                    doc_line(old, end_line.0)
                };
                vec![lsp_types::TextDocumentContentChangeEvent {
                    range: Some(lsp_types::Range {
                        start: lsp_pos(start_line.0, start_col, start_line_text.as_deref()),
                        end: lsp_pos(end_line.0, end_col, end_line_text.as_deref()),
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

        server.notify(
            "textDocument/didChange",
            &lsp_types::DidChangeTextDocumentParams {
                text_document: lsp_types::VersionedTextDocumentIdentifier { uri, version },
                content_changes,
            },
        );
        self.need_diagnostics = false;
    }

    fn send_did_save(&mut self, path: &Path) {
        let Some(server) = self.server_for_path(path) else {
            return;
        };
        let Some(uri) = uri_from_path(path) else {
            return;
        };
        let text = std::fs::read_to_string(path).unwrap_or_default();
        server.notify(
            "textDocument/didSave",
            &lsp_types::DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier { uri },
                text: Some(text),
            },
        );
        self.need_diagnostics = true;
    }

    fn send_did_close(&mut self, path: &Path) {
        if !self.opened_docs.remove(path) {
            return;
        }
        if let Ok(canonical) = std::fs::canonicalize(path) {
            self.canonical_to_original.remove(&canonical);
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

    fn next_version(&mut self, path: &Path) -> i32 {
        let v = self.doc_versions.entry(path.to_path_buf()).or_insert(0);
        *v += 1;
        *v
    }

    // ── Feature requests ──

    fn spawn_goto_definition(&self, path: PathBuf, row: usize, col: usize) {
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

    fn spawn_completion(&mut self, path: PathBuf, row: usize, col: usize) {
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

    fn spawn_rename(&self, path: PathBuf, row: usize, col: usize, new_name: String) {
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
        path: PathBuf,
        start_row: usize,
        start_col: usize,
        end_row: usize,
        end_col: usize,
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

    fn spawn_format(&self, path: PathBuf, doc_content: Option<Vec<u8>>) {
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

            log::info!("format: formatting took {:?}, total {:?}", t1.elapsed(), t0.elapsed());
            // Always signal format done so save-after-format can proceed
            let _ = event_tx.send(ManagerEvent::RequestResult(RequestResult::FormatDone));
        });
    }

    fn spawn_inlay_hints(&self, path: PathBuf, start_row: usize, end_row: usize) {
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
                    start: lsp_pos(start_row, 0, None),
                    end: lsp_pos(end_row, 0, None),
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
                    if let Some(raw_path) = crate::convert::path_from_uri(&params.uri) {
                        let path = self.resolve_path(raw_path);
                        let line_at = |row: usize| {
                            self.docs
                                .get(&path)
                                .and_then(|d| doc_line(&**d, row))
                                .or_else(|| {
                                    std::fs::read_to_string(&path)
                                        .ok()
                                        .and_then(|c| c.lines().nth(row).map(|l| l.to_string()))
                                })
                        };
                        let diagnostics = convert_diagnostics(&params.diagnostics, &line_at);
                        if self.need_diagnostics {
                            let h = self
                                .docs
                                .get(&path)
                                .map(|d| d.content_hash())
                                .unwrap_or(ContentHash(0));
                            let _ = result_tx
                                .send(LspIn::Diagnostics {
                                    path,
                                    diagnostics,
                                    content_hash: h,
                                })
                                .await;
                        } else {
                            self.buffered_diagnostics.insert(path, diagnostics);
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
                if let Some(q) = quiescent {
                    let was_busy = !*self.quiescent.get(&language).unwrap_or(&true);
                    self.quiescent.insert(language, q);
                    if was_busy && q && self.need_diagnostics {
                        self.pull_all_diagnostics();
                    }
                    self.send_progress_throttled(result_tx).await;
                }
            }
            "client/registerCapability" => {
                self.handle_register_capability(&notif.params);
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
                    let path = self.resolve_path(path);
                    let _ = result_tx.send(LspIn::Navigate { path, row, col }).await;
                }
            }
            RequestResult::Format { path, edits } => {
                let line_at = |row: usize| self.docs.get(&path).and_then(|d| doc_line(&**d, row));
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
                for mut fe in file_edits {
                    fe.path = self.resolve_path(fe.path);
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
                for mut fe in file_edits {
                    fe.path = self.resolve_path(fe.path);
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
                let line_at = |row: usize| self.docs.get(&path).and_then(|d| doc_line(&**d, row));
                let domain_hints = convert_inlay_hints(hints, &line_at);
                let _ = result_tx
                    .send(LspIn::InlayHints {
                        path,
                        hints: domain_hints,
                    })
                    .await;
            }
            RequestResult::Diagnostics { path, raw } => {
                let line_at = |row: usize| self.docs.get(&path).and_then(|d| doc_line(&**d, row));
                let diagnostics = convert_diagnostics(&raw, &line_at);
                let h = self
                    .docs
                    .get(&path)
                    .map(|d| d.content_hash())
                    .unwrap_or(ContentHash(0));
                let _ = result_tx
                    .send(LspIn::Diagnostics {
                        path,
                        diagnostics,
                        content_hash: h,
                    })
                    .await;
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
                let line_at = |r: usize| self.docs.get(&path).and_then(|d| doc_line(&**d, r));

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

    fn check_trigger_char(&self, path: &Path, edit_ops: &[EditOp]) -> bool {
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
        changed_path: &Path,
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
        if row >= doc.line_count() {
            self.clear_completion(result_tx).await;
            return;
        }

        let line = doc.line(Row(row));
        let line_chars: Vec<char> = line.chars().collect();
        let psc = self.completion_prefix_start_col;

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
            if edit_row.0 != row || edit_col < psc || edit_col > id_end {
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
                    prefix_start_col: psc,
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
                    prefix_start_col: psc,
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
                prefix_start_col: 0,
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
                    let _ = event_tx.send(ManagerEvent::FileChanged(path, kind));
                }
            }
        });

        match watcher {
            Ok(mut w) => {
                if let Err(e) = w.watch(&root, RecursiveMode::Recursive) {
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

    fn send_file_changed(&self, path: &Path, kind: FileChangeKind) {
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

    // ── Pull diagnostics ──

    fn pull_all_diagnostics(&self) {
        for path in &self.opened_docs {
            if let Some(server) = self.server_for_path(path) {
                self.spawn_pull_diagnostics(path.clone(), server);
            }
        }
    }

    fn spawn_pull_diagnostics(&self, path: PathBuf, server: Arc<LanguageServer>) {
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
                Ok(_) => return,
                Err(e) => {
                    log::debug!(
                        "LSP pull diagnostics failed for {}: {}",
                        path.display(),
                        e.message
                    );
                    return;
                }
            };

            let _ = event_tx.send(ManagerEvent::RequestResult(RequestResult::Diagnostics {
                path,
                raw,
            }));
        });
    }

    // ── Helpers ──

    /// Resolve a canonical path (from server responses) back to the original
    /// path used by the rest of the system. On macOS, /private/var/… → /var/….
    fn resolve_path(&self, path: PathBuf) -> PathBuf {
        self.canonical_to_original
            .get(&path)
            .cloned()
            .unwrap_or(path)
    }

    fn line_at(&self, path: &Path, row: usize) -> Option<String> {
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
fn find_prettier(file_path: &Path) -> Option<PathBuf> {
    let mut dir = file_path.parent()?;
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
    file_path: &Path,
    content: &[u8],
) -> Option<Vec<crate::TextEdit>> {
    use tokio::process::Command;

    let mut child = Command::new(prettier_bin)
        .arg("--stdin-filepath")
        .arg(file_path)
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
        start_row: 0,
        start_col: 0,
        end_row: last_line,
        end_col: last_col,
        new_text: formatted,
    }])
}
