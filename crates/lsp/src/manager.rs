use std::path::{Path, PathBuf};
use std::sync::Arc;

use led_core::lsp_types::{
    DiagnosticSeverity as EditorSeverity, EditorCompletionItem, EditorDiagnostic, EditorRange,
    EditorTextEdit,
};
use led_core::{DocStore, Effect, Event};
use lsp_types::{
    CodeActionOrCommand, CodeActionParams, CodeActionResponse, CompletionParams,
    CompletionResponse, DocumentFormattingParams, FormattingOptions, GotoDefinitionParams,
    GotoDefinitionResponse, InlayHintParams, NumberOrString, Position, Range, RenameParams,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, TextEdit, WorkspaceEdit,
};
use serde_json::Value;

use crate::LspManager;
use crate::convert::{
    apply_edits_to_disk, definition_response_to_locations, language_id_for_extension,
    lsp_edit_to_editor, workspace_edit_to_file_edits,
};
use crate::server::LanguageServer;
use crate::transport::LspNotification;
use crate::types::{LspManagerEvent, ProgressState, RequestResult};
use crate::util::{from_lsp_pos, lsp_pos, uri_from_path};

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

fn classify_progress(value: &lsp_types::ProgressParamsValue) -> ProgressUpdate {
    match value {
        lsp_types::ProgressParamsValue::WorkDone(wd) => match wd {
            lsp_types::WorkDoneProgress::Begin(begin) => ProgressUpdate::Begin {
                title: begin.title.clone(),
                message: begin.message.clone(),
                percentage: begin.percentage,
            },
            lsp_types::WorkDoneProgress::Report(report) => {
                // Treat 100% as implicit End — rust-analyzer
                // delays the real End notification.
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

/// Fetch a single line from DocStore, falling back to disk.
fn doc_line(docs: &DocStore, path: &Path, row: usize) -> Option<String> {
    if let Some(line) = docs.line(path, row) {
        return Some(line);
    }
    // Fallback: read from disk
    let content = std::fs::read_to_string(path).ok()?;
    content.lines().nth(row).map(|l| l.to_string())
}

/// Extract raw TextEdits for a specific path from a WorkspaceEdit.
fn extract_raw_edits_for_path(edit: &WorkspaceEdit, target: &Path) -> Vec<TextEdit> {
    let mut result = Vec::new();

    if let Some(changes) = &edit.changes {
        for (uri, edits) in changes {
            if let Some(path) = crate::util::path_from_uri(uri) {
                if path == target {
                    result.extend(edits.iter().cloned());
                }
            }
        }
    }

    if let Some(document_changes) = &edit.document_changes {
        use lsp_types::DocumentChanges;
        match document_changes {
            DocumentChanges::Edits(edits) => {
                for tde in edits {
                    if let Some(path) = crate::util::path_from_uri(&tde.text_document.uri) {
                        if path == target {
                            for e in &tde.edits {
                                match e {
                                    lsp_types::OneOf::Left(te) => result.push(te.clone()),
                                    lsp_types::OneOf::Right(ate) => {
                                        result.push(ate.text_edit.clone())
                                    }
                                }
                            }
                        }
                    }
                }
            }
            DocumentChanges::Operations(ops) => {
                for op in ops {
                    if let lsp_types::DocumentChangeOperation::Edit(tde) = op {
                        if let Some(path) = crate::util::path_from_uri(&tde.text_document.uri) {
                            if path == target {
                                for e in &tde.edits {
                                    match e {
                                        lsp_types::OneOf::Left(te) => result.push(te.clone()),
                                        lsp_types::OneOf::Right(ate) => {
                                            result.push(ate.text_edit.clone())
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    result
}

// -- File watching -----------------------------------------------------------

impl LspManager {
    pub(crate) fn handle_register_capability(&mut self, params: &Value) {
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
        let waker = self.waker.clone();
        let globs = match self.file_watcher_globs.as_ref() {
            Some(g) => g.clone(),
            None => return,
        };
        let root = self.root.clone();

        let watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
            let Ok(ev) = res else { return };
            let kind = match ev.kind {
                notify::EventKind::Create(_) => crate::types::FileChangeKind::Created,
                notify::EventKind::Modify(_) => crate::types::FileChangeKind::Changed,
                notify::EventKind::Remove(_) => crate::types::FileChangeKind::Deleted,
                _ => return,
            };
            for path in ev.paths {
                if globs.is_match(&path) {
                    let _ = event_tx.send(LspManagerEvent::FileChanged(path, kind));
                    if let Some(ref w) = waker {
                        w();
                    }
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

    pub(crate) fn send_file_changed(&self, path: &Path, kind: crate::types::FileChangeKind) {
        // Send to all servers — the server will filter by relevance
        let Some(uri) = uri_from_path(path) else {
            return;
        };
        let change_type = match kind {
            crate::types::FileChangeKind::Created => lsp_types::FileChangeType::CREATED,
            crate::types::FileChangeKind::Changed => lsp_types::FileChangeType::CHANGED,
            crate::types::FileChangeKind::Deleted => lsp_types::FileChangeType::DELETED,
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

    // -- Server lifecycle ----------------------------------------------------

    pub(crate) fn ensure_server_for_path(&mut self, path: &Path) {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

        let Some(config) = self.registry.config_for_extension(ext).cloned() else {
            return;
        };

        let lang_id = config.language_id.to_string();
        if self.servers.contains_key(&lang_id) || self.pending_starts.contains(&lang_id) {
            return;
        }

        log::info!("LSP starting server for language: {}", lang_id);
        self.pending_starts.insert(lang_id.clone());

        let root = self.root.clone();
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        // Create a notification channel that feeds into our event channel
        let (notif_tx, mut notif_rx) = tokio::sync::mpsc::unbounded_channel::<LspNotification>();
        let event_tx2 = event_tx.clone();
        let waker2 = waker.clone();
        tokio::spawn(async move {
            while let Some(notif) = notif_rx.recv().await {
                let _ = event_tx2.send(LspManagerEvent::Notification(notif));
                if let Some(ref w) = waker2 {
                    w();
                }
            }
        });

        tokio::spawn(async move {
            match LanguageServer::start(&config, &root, notif_tx, waker.clone()).await {
                Ok(server) => {
                    let _ = event_tx.send(LspManagerEvent::ServerStarted {
                        language_id: lang_id,
                        server,
                    });
                    if let Some(ref w) = waker {
                        w();
                    }
                }
                Err(e) => {
                    let _ = event_tx.send(LspManagerEvent::ServerError { error: e.message });
                    if let Some(ref w) = waker {
                        w();
                    }
                }
            }
        });
    }

    pub(crate) fn server_for_path(&self, path: &Path) -> Option<Arc<LanguageServer>> {
        let ext = path.extension().and_then(|e| e.to_str())?;
        let config = self.registry.config_for_extension(ext)?;
        self.servers.get(config.language_id).cloned()
    }

    // -- Document sync -------------------------------------------------------

    pub(crate) fn send_did_open(&mut self, path: &Path, docs: &DocStore) {
        if self.opened_docs.contains(path) {
            return;
        }
        let Some(server) = self.server_for_path(path) else {
            // Server not ready yet — remember so we can open when it starts
            log::debug!(
                "LSP didOpen deferred (server not ready): {}",
                path.display()
            );
            self.pending_opens.insert(path.to_path_buf());
            return;
        };
        let Some(uri) = uri_from_path(path) else {
            return;
        };
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let lang_id = language_id_for_extension(ext);
        // Read content from DocStore (includes unsaved changes), fall back to disk
        let text = docs
            .content(path)
            .unwrap_or_else(|| std::fs::read_to_string(path).unwrap_or_default());
        let version = docs.version(path).unwrap_or(0);

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
        self.need_diagnostics = true;
    }

    pub(crate) fn send_did_change(
        &mut self,
        path: &Path,
        changes: &[EditorTextEdit],
        version: i32,
        _docs: &DocStore,
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
        let content_changes: Vec<lsp_types::TextDocumentContentChangeEvent> = changes
            .iter()
            .map(|edit| lsp_types::TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: lsp_pos(
                        edit.range.start.row,
                        edit.range.start.col,
                        edit.start_line.as_deref(),
                    ),
                    end: lsp_pos(
                        edit.range.end.row,
                        edit.range.end.col,
                        edit.end_line.as_deref(),
                    ),
                }),
                range_length: None,
                text: edit.new_text.clone(),
            })
            .collect();

        server.notify(
            "textDocument/didChange",
            &lsp_types::DidChangeTextDocumentParams {
                text_document: lsp_types::VersionedTextDocumentIdentifier { uri, version },
                content_changes,
            },
        );
        self.need_diagnostics = true;
    }

    pub(crate) fn send_did_save(&mut self, path: &Path) {
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

    pub(crate) fn send_did_close(&mut self, path: &Path) {
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

    // -- Feature methods (spawn tokio tasks) ---------------------------------

    pub(crate) fn spawn_goto_definition(
        &self,
        path: PathBuf,
        row: usize,
        col: usize,
        cursor_line: Option<String>,
    ) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
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

            let _ = event_tx.send(LspManagerEvent::RequestResult(
                RequestResult::GotoDefinition {
                    locations,
                    origin_path: path,
                    origin_row: row,
                    origin_col: col,
                },
            ));
            if let Some(ref w) = waker {
                w();
            }
        });
    }

    pub(crate) fn spawn_inlay_hints(&self, path: PathBuf, start_row: usize, end_row: usize) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
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

            let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::InlayHints {
                path,
                hints,
            }));
            if let Some(ref w) = waker {
                w();
            }
        });
    }

    pub(crate) fn spawn_rename(
        &self,
        path: PathBuf,
        row: usize,
        col: usize,
        new_name: String,
        cursor_line: Option<String>,
    ) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
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
                    let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::Rename {
                        primary_path: path,
                        file_edits,
                    }));
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::Error {
                        message: e.message,
                    }));
                }
            }
            if let Some(ref w) = waker {
                w();
            }
        });
    }

    pub(crate) fn spawn_code_action(
        &self,
        path: PathBuf,
        start_row: usize,
        start_col: usize,
        end_row: usize,
        end_col: usize,
        start_line: Option<String>,
        end_line: Option<String>,
    ) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
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
                        event_tx.send(LspManagerEvent::RequestResult(RequestResult::CodeActions {
                            path,
                            raw: actions,
                        }));
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::Error {
                        message: e.message,
                    }));
                }
            }
            if let Some(ref w) = waker {
                w();
            }
        });
    }

    pub(crate) fn spawn_code_action_resolve(&self, path: PathBuf, index: usize) {
        let raw_actions = match self.pending_code_actions.get(&path) {
            Some(actions) => actions.clone(),
            None => return,
        };

        let Some(action) = raw_actions.get(index).cloned() else {
            return;
        };

        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            match action {
                CodeActionOrCommand::CodeAction(mut ca) => {
                    // If the action already has an edit, use it directly
                    if ca.edit.is_none() {
                        // Try to resolve
                        match server
                            .request::<_, lsp_types::CodeAction>("codeAction/resolve", &ca)
                            .await
                        {
                            Ok(resolved) => ca = resolved,
                            Err(e) => {
                                let _ = event_tx.send(LspManagerEvent::RequestResult(
                                    RequestResult::Error { message: e.message },
                                ));
                                if let Some(ref w) = waker {
                                    w();
                                }
                                return;
                            }
                        }
                    }

                    if let Some(edit) = ca.edit {
                        let file_edits = workspace_edit_to_file_edits(&edit);
                        let _ = event_tx.send(LspManagerEvent::RequestResult(
                            RequestResult::CodeActionResolved {
                                primary_path: path,
                                file_edits,
                            },
                        ));
                    }
                }
                CodeActionOrCommand::Command(_cmd) => {
                    // Command execution not supported yet
                }
            }
            if let Some(ref w) = waker {
                w();
            }
        });
    }

    pub(crate) fn spawn_format(&self, path: PathBuf, generation: u64) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let uri =
                match uri_from_path(&path) {
                    Some(u) => u,
                    None => {
                        let _ = event_tx.send(LspManagerEvent::RequestResult(
                            RequestResult::FormatDone { path, generation },
                        ));
                        if let Some(ref w) = waker {
                            w();
                        }
                        return;
                    }
                };

            // Step 1: Organize imports via codeAction
            let oi_params = CodeActionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 0,
                    },
                },
                context: lsp_types::CodeActionContext {
                    diagnostics: vec![],
                    only: Some(vec![lsp_types::CodeActionKind::new(
                        "source.organizeImports",
                    )]),
                    trigger_kind: Some(lsp_types::CodeActionTriggerKind::AUTOMATIC),
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };

            let oi_result: Result<Option<CodeActionResponse>, _> =
                server.request("textDocument/codeAction", &oi_params).await;

            if let Ok(Some(actions)) = oi_result {
                for action in actions {
                    if let CodeActionOrCommand::CodeAction(ca) = action {
                        if let Some(ref edit) = ca.edit {
                            // Extract raw TextEdits for the primary file
                            let raw_edits = extract_raw_edits_for_path(edit, &path);
                            if !raw_edits.is_empty() {
                                let _ = event_tx.send(LspManagerEvent::RequestResult(
                                    RequestResult::Format {
                                        path: path.clone(),
                                        edits: raw_edits,
                                    },
                                ));
                                if let Some(ref w) = waker {
                                    w();
                                }
                            }
                        }
                    }
                }
            }

            // Step 2: Format document
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
                    let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::Format {
                        path: path.clone(),
                        edits,
                    }));
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::Error {
                        message: e.message,
                    }));
                }
            }

            // Step 3: Always signal format done
            let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::FormatDone {
                path,
                generation,
            }));
            if let Some(ref w) = waker {
                w();
            }
        });
    }

    pub(crate) fn spawn_completion(
        &self,
        path: PathBuf,
        row: usize,
        col: usize,
        cursor_line: Option<String>,
    ) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
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
                    let _ =
                        event_tx.send(LspManagerEvent::RequestResult(RequestResult::Completion {
                            path,
                            response,
                            row,
                            col,
                        }));
                }
                Ok(None) => {}
                Err(e) => {
                    log::error!("LSP completion failed: {}", e.message);
                }
            }
            if let Some(ref w) = waker {
                w();
            }
        });
    }

    pub(crate) fn spawn_completion_resolve(&self, path: PathBuf, lsp_item_json: String) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let item: lsp_types::CompletionItem = match serde_json::from_str(&lsp_item_json) {
                Ok(v) => v,
                Err(_) => return,
            };
            let result: Result<lsp_types::CompletionItem, _> =
                server.request("completionItem/resolve", &item).await;

            if let Ok(resolved) = result {
                if let Some(edits) = resolved.additional_text_edits {
                    if !edits.is_empty() {
                        let editor_edits: Vec<_> = edits
                            .iter()
                            .map(|e| lsp_edit_to_editor(e, &|_| None))
                            .collect();
                        let _ = event_tx.send(LspManagerEvent::RequestResult(
                            RequestResult::CompletionResolved {
                                path,
                                additional_edits: editor_edits,
                            },
                        ));
                    }
                }
            }
            if let Some(ref w) = waker {
                w();
            }
        });
    }

    // -- Notification handling -----------------------------------------------

    pub(crate) fn handle_notification(
        &mut self,
        notif: LspNotification,
        docs: &DocStore,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();

        match notif.method.as_str() {
            "textDocument/publishDiagnostics" => {
                if let Ok(params) =
                    serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(notif.params)
                {
                    if let Some(path) = crate::util::path_from_uri(&params.uri) {
                        log::debug!(
                            "LSP diagnostics: {} count={}",
                            path.display(),
                            params.diagnostics.len()
                        );
                        let diagnostics = convert_diagnostics(&params.diagnostics, |row| {
                            doc_line(docs, &path, row)
                        });
                        effects.push(Effect::Emit(Event::SetDiagnostics { path, diagnostics }));
                    }
                }
            }
            "$/progress" => {
                match serde_json::from_value::<lsp_types::ProgressParams>(notif.params) {
                    Ok(params) => {
                        let token = match &params.token {
                            NumberOrString::Number(n) => n.to_string(),
                            NumberOrString::String(s) => s.clone(),
                        };

                        let update = classify_progress(&params.value);
                        self.apply_progress_update(&token, update);
                        effects.push(self.lsp_status_effect());
                    }
                    Err(e) => {
                        log::debug!("LSP $/progress deserialize error: {}", e);
                    }
                }
            }
            "experimental/serverStatus" => {
                let quiescent = notif.params.get("quiescent").and_then(|v| v.as_bool());
                let message = notif.params.get("message").and_then(|v| v.as_str());
                log::debug!(
                    "LSP serverStatus: quiescent={:?} message={:?}",
                    quiescent,
                    message
                );
                if let Some(q) = quiescent {
                    let was_busy = !self.quiescent;
                    self.quiescent = q;
                    effects.push(self.lsp_status_effect());
                    // When server becomes quiescent, pull fresh diagnostics
                    // to replace any stale push diagnostics from early analysis.
                    if was_busy && q {
                        self.need_diagnostics = true;
                    }
                }
            }
            "client/registerCapability" => {
                self.handle_register_capability(&notif.params);
            }
            _ => {
                log::debug!("LSP unhandled notification: {}", notif.method);
            }
        }

        effects
    }

    fn apply_progress_update(&mut self, token: &str, update: ProgressUpdate) {
        match update {
            ProgressUpdate::Begin {
                title,
                message,
                percentage,
            } => {
                log::debug!(
                    "LSP progress begin: token={} title={:?} message={:?}",
                    token,
                    title,
                    message
                );
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
                log::debug!(
                    "LSP progress end: token={} (remaining={})",
                    token,
                    self.progress_tokens.len().saturating_sub(1)
                );
                self.progress_tokens.remove(token);
            }
        }
    }

    pub(crate) fn handle_request_result(
        &mut self,
        result: RequestResult,
        docs: &DocStore,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();

        match result {
            RequestResult::GotoDefinition {
                locations,
                origin_path,
                origin_row,
                origin_col,
            } => {
                if let Some((path, row, col)) = locations.into_iter().next() {
                    // Record jump at origin before navigating
                    effects.push(Effect::Emit(Event::RecordJump {
                        path: origin_path,
                        row: origin_row,
                        col: origin_col,
                        scroll_offset: 0,
                    }));
                    effects.push(Effect::Emit(Event::OpenDefinition(path.clone())));
                    effects.push(Effect::Emit(Event::GoToPosition {
                        path,
                        row,
                        col,
                        scroll_offset: None,
                    }));
                }
            }
            RequestResult::Format { path, edits } => {
                if !edits.is_empty() {
                    let line_at = |row: usize| doc_line(docs, &path, row);
                    let editor_edits: Vec<EditorTextEdit> = edits
                        .iter()
                        .map(|e| lsp_edit_to_editor(e, &line_at))
                        .collect();
                    effects.push(Effect::Emit(Event::ApplyEdits {
                        path,
                        edits: editor_edits,
                    }));
                }
            }
            RequestResult::Rename {
                primary_path,
                file_edits,
            } => {
                for (path, edits) in &file_edits {
                    if *path == primary_path {
                        effects.push(Effect::Emit(Event::ApplyEdits {
                            path: path.clone(),
                            edits: edits.clone(),
                        }));
                    } else {
                        // Apply to disk for non-open files
                        apply_edits_to_disk(path, edits);
                    }
                }
            }
            RequestResult::CodeActions { path, raw } => {
                if !raw.is_empty() {
                    let items: Vec<String> = raw
                        .iter()
                        .map(|a| match a {
                            CodeActionOrCommand::CodeAction(ca) => ca.title.clone(),
                            CodeActionOrCommand::Command(cmd) => cmd.title.clone(),
                        })
                        .collect();
                    self.pending_code_actions.insert(path.clone(), raw);
                    effects.push(Effect::Emit(Event::ShowPicker {
                        title: "Code Actions".into(),
                        items,
                        source_path: path,
                        kind: led_core::PickerKind::CodeAction,
                    }));
                }
            }
            RequestResult::CodeActionResolved {
                primary_path,
                file_edits,
            } => {
                for (path, edits) in &file_edits {
                    if *path == primary_path {
                        effects.push(Effect::Emit(Event::ApplyEdits {
                            path: path.clone(),
                            edits: edits.clone(),
                        }));
                    } else {
                        apply_edits_to_disk(path, edits);
                    }
                }
            }
            RequestResult::InlayHints { path, hints } => {
                let editor_hints = convert_inlay_hints(hints, |row| doc_line(docs, &path, row));
                effects.push(Effect::Emit(Event::SetInlayHints {
                    path,
                    hints: editor_hints,
                }));
            }
            RequestResult::Diagnostics { path, raw } => {
                let diagnostics = convert_diagnostics(&raw, |row| doc_line(docs, &path, row));
                effects.push(Effect::Emit(Event::SetDiagnostics { path, diagnostics }));
            }
            RequestResult::Completion {
                path,
                response,
                row,
                col,
            } => {
                let (items, prefix_start_col) =
                    convert_completion_response(response, row, col, |r| doc_line(docs, &path, r));
                effects.push(Effect::Emit(Event::SetCompletions {
                    path,
                    items,
                    prefix_start_col,
                }));
            }
            RequestResult::CompletionResolved {
                path,
                additional_edits,
            } => {
                effects.push(Effect::Emit(Event::CompletionResolved {
                    path,
                    additional_edits,
                }));
            }
            RequestResult::Error { message } => {
                effects.push(Effect::SetMessage(format!("LSP: {}", message)));
            }
            RequestResult::FormatDone { path, generation } => {
                effects.push(Effect::Emit(Event::FormatDone { path, generation }));
            }
        }

        effects
    }

    /// Pull diagnostics for all open documents.
    /// Called when the server transitions to quiescent.
    pub(crate) fn pull_all_diagnostics(&self) {
        for (path, server) in self.open_docs_with_servers() {
            self.spawn_pull_diagnostics(path, server);
        }
    }

    fn open_docs_with_servers(&self) -> Vec<(PathBuf, Arc<LanguageServer>)> {
        self.opened_docs
            .iter()
            .filter_map(|path| {
                let server = self.server_for_path(path)?;
                Some((path.clone(), server))
            })
            .collect()
    }

    fn spawn_pull_diagnostics(&self, path: PathBuf, server: Arc<LanguageServer>) {
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
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
                )) => {
                    log::debug!(
                        "LSP pull diagnostics: {} count={}",
                        path.display(),
                        report.full_document_diagnostic_report.items.len()
                    );
                    report.full_document_diagnostic_report.items
                }
                Ok(_) => {
                    // Unchanged or partial — keep existing diagnostics
                    return;
                }
                Err(e) => {
                    log::debug!(
                        "LSP pull diagnostics failed for {}: {}",
                        path.display(),
                        e.message
                    );
                    return;
                }
            };

            let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::Diagnostics {
                path,
                raw,
            }));
            if let Some(ref w) = waker {
                w();
            }
        });
    }
}

fn convert_inlay_hints(
    hints: Vec<lsp_types::InlayHint>,
    line_at: impl Fn(usize) -> Option<String>,
) -> Vec<led_core::lsp_types::EditorInlayHint> {
    use led_core::lsp_types::EditorInlayHint;
    hints
        .into_iter()
        .filter_map(|h| {
            let line = line_at(h.position.line as usize);
            let pos = from_lsp_pos(&h.position, line.as_deref());
            let label = match h.label {
                lsp_types::InlayHintLabel::String(s) => s,
                lsp_types::InlayHintLabel::LabelParts(parts) => {
                    parts.into_iter().map(|p| p.value).collect::<String>()
                }
            };
            Some(EditorInlayHint {
                position: pos,
                label,
            })
        })
        .collect()
}

fn convert_completion_response(
    resp: CompletionResponse,
    row: usize,
    col: usize,
    line_at: impl Fn(usize) -> Option<String>,
) -> (Vec<EditorCompletionItem>, usize) {
    let lsp_items = match resp {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    };

    // Compute prefix_start_col from first text_edit, or scan backward for word chars
    let prefix_start_col = lsp_items
        .iter()
        .find_map(|item| {
            if let Some(lsp_types::CompletionTextEdit::Edit(ref te)) = item.text_edit {
                let line = line_at(te.range.start.line as usize);
                let pos = from_lsp_pos(&te.range.start, line.as_deref());
                Some(pos.col)
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            // Scan backward from col for word characters
            if let Some(line_str) = line_at(row) {
                let line: Vec<char> = line_str.chars().collect();
                let mut start = col;
                while start > 0 && start <= line.len() {
                    let ch = line[start - 1];
                    if ch.is_alphanumeric() || ch == '_' {
                        start -= 1;
                    } else {
                        break;
                    }
                }
                start
            } else {
                col
            }
        });

    let items: Vec<EditorCompletionItem> = lsp_items
        .into_iter()
        .map(|item| {
            let label = item.label.clone();
            let detail = item.detail.clone();

            let (insert_text, text_edit) = match item.text_edit {
                Some(lsp_types::CompletionTextEdit::Edit(ref te)) => {
                    (te.new_text.clone(), Some(lsp_edit_to_editor(te, &line_at)))
                }
                Some(lsp_types::CompletionTextEdit::InsertAndReplace(ref te)) => {
                    let start_line = line_at(te.insert.start.line as usize);
                    let end_line = line_at(te.insert.end.line as usize);
                    let start = from_lsp_pos(&te.insert.start, start_line.as_deref());
                    let end = from_lsp_pos(&te.insert.end, end_line.as_deref());
                    (
                        te.new_text.clone(),
                        Some(EditorTextEdit {
                            range: EditorRange { start, end },
                            new_text: te.new_text.clone(),
                            start_line: None,
                            end_line: None,
                        }),
                    )
                }
                None => {
                    let text = item
                        .insert_text
                        .as_deref()
                        .unwrap_or(&item.label)
                        .to_string();
                    (text, None)
                }
            };

            let additional_edits = item
                .additional_text_edits
                .as_ref()
                .map(|edits| {
                    edits
                        .iter()
                        .map(|e| lsp_edit_to_editor(e, &line_at))
                        .collect()
                })
                .unwrap_or_default();

            let sort_text = item.sort_text.clone();
            let filter_text = item.filter_text.clone();

            // Store raw LSP item as JSON for resolve requests
            let lsp_completion = serde_json::to_string(&item).ok();

            EditorCompletionItem {
                label,
                detail,
                insert_text,
                text_edit,
                additional_edits,
                sort_text,
                filter_text,
                lsp_completion,
            }
        })
        .collect();

    (items, prefix_start_col)
}

fn convert_diagnostics(
    lsp_diags: &[lsp_types::Diagnostic],
    line_at: impl Fn(usize) -> Option<String>,
) -> Vec<EditorDiagnostic> {
    lsp_diags
        .iter()
        .map(|d| {
            let severity = match d.severity {
                Some(lsp_types::DiagnosticSeverity::ERROR) => EditorSeverity::Error,
                Some(lsp_types::DiagnosticSeverity::WARNING) => EditorSeverity::Warning,
                Some(lsp_types::DiagnosticSeverity::INFORMATION) => EditorSeverity::Info,
                Some(lsp_types::DiagnosticSeverity::HINT) => EditorSeverity::Hint,
                _ => EditorSeverity::Error,
            };
            let start_line = line_at(d.range.start.line as usize);
            let end_line = if d.range.end.line == d.range.start.line {
                start_line.clone()
            } else {
                line_at(d.range.end.line as usize)
            };
            let start = from_lsp_pos(&d.range.start, start_line.as_deref());
            let end = from_lsp_pos(&d.range.end, end_line.as_deref());
            EditorDiagnostic {
                range: EditorRange { start, end },
                severity,
                message: d.message.clone(),
            }
        })
        .collect()
}
