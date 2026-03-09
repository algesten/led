use std::path::{Path, PathBuf};
use std::sync::Arc;

use led_core::lsp_types::{
    DiagnosticSeverity as EditorSeverity, EditorCodeAction, EditorDiagnostic, EditorInlayHint,
    EditorRange, EditorTextEdit,
};
use led_core::{Effect, Event};
use lsp_types::{
    CodeActionOrCommand, CodeActionParams, CodeActionResponse, DocumentFormattingParams,
    FormattingOptions, GotoDefinitionParams, GotoDefinitionResponse, InlayHint, InlayHintLabel,
    InlayHintParams, NumberOrString, Position, PublishDiagnosticsParams, Range, RenameParams,
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
use crate::util::{from_lsp_pos, lsp_pos, read_file_lines, uri_from_path};

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
            match ev.kind {
                notify::EventKind::Create(_)
                | notify::EventKind::Modify(_)
                | notify::EventKind::Remove(_) => {}
                _ => return,
            }
            for path in ev.paths {
                if globs.is_match(&path) {
                    let _ = event_tx.send(LspManagerEvent::FileChanged(path));
                    if let Some(ref w) = waker {
                        w();
                    }
                }
            }
        });

        match watcher {
            Ok(mut w) => {
                if let Err(e) = w.watch(&root, RecursiveMode::Recursive) {
                    log::info!(
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
                log::info!("LSP file watcher: failed to create: {}", e);
            }
        }
    }

    pub(crate) fn send_file_changed(&self, path: &Path) {
        // Send to all servers — the server will filter by relevance
        let Some(uri) = uri_from_path(path) else {
            return;
        };
        let change_type = if path.exists() {
            lsp_types::FileChangeType::CHANGED
        } else {
            lsp_types::FileChangeType::DELETED
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

    pub(crate) fn send_did_open(&mut self, path: &Path, docs: &led_core::DocStore) {
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
        self.spawn_pull_diagnostics(path.to_path_buf(), server);
    }

    pub(crate) fn send_did_change(&self, path: &Path, changes: &[EditorTextEdit], version: i32) {
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
                    start: Position {
                        line: edit.range.start.row as u32,
                        character: edit.range.start.col as u32,
                    },
                    end: Position {
                        line: edit.range.end.row as u32,
                        character: edit.range.end.col as u32,
                    },
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
        self.spawn_pull_diagnostics(path.to_path_buf(), server.clone());
    }

    pub(crate) fn send_did_save(&self, path: &Path) {
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
        self.spawn_pull_diagnostics(path.to_path_buf(), server.clone());
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

    pub(crate) fn spawn_goto_definition(&self, path: PathBuf, row: usize, col: usize) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let lines = read_file_lines(&path);
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
            };
            let params = GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: lsp_pos(row, col, &lines),
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
            let lines = read_file_lines(&path);
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
            };
            let params = InlayHintParams {
                text_document: TextDocumentIdentifier { uri },
                range: Range {
                    start: lsp_pos(start_row, 0, &lines),
                    end: lsp_pos(end_row, 0, &lines),
                },
                work_done_progress_params: Default::default(),
            };

            let result: Result<Option<Vec<InlayHint>>, _> =
                server.request("textDocument/inlayHint", &params).await;

            let hints = match result {
                Ok(Some(hints)) => hints
                    .iter()
                    .map(|h| {
                        let pos = from_lsp_pos(&h.position, &lines);
                        let label = match &h.label {
                            InlayHintLabel::String(s) => s.clone(),
                            InlayHintLabel::LabelParts(parts) => {
                                parts.iter().map(|p| p.value.as_str()).collect()
                            }
                        };
                        EditorInlayHint {
                            position: pos,
                            label,
                        }
                    })
                    .collect(),
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

    pub(crate) fn spawn_rename(&self, path: PathBuf, row: usize, col: usize, new_name: String) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let lines = read_file_lines(&path);
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
            };
            let params = RenameParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: lsp_pos(row, col, &lines),
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
    ) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let lines = read_file_lines(&path);
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
            };
            let params = CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range: Range {
                    start: lsp_pos(start_row, start_col, &lines),
                    end: lsp_pos(end_row, end_col, &lines),
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
                    let editor_actions: Vec<EditorCodeAction> = actions
                        .iter()
                        .enumerate()
                        .map(|(i, a)| {
                            let title = match a {
                                CodeActionOrCommand::CodeAction(ca) => ca.title.clone(),
                                CodeActionOrCommand::Command(cmd) => cmd.title.clone(),
                            };
                            EditorCodeAction { title, index: i }
                        })
                        .collect();

                    let _ =
                        event_tx.send(LspManagerEvent::RequestResult(RequestResult::CodeActions {
                            path,
                            actions: editor_actions,
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

    pub(crate) fn spawn_format(&self, path: PathBuf) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let lines = read_file_lines(&path);
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
            };
            let params = DocumentFormattingParams {
                text_document: TextDocumentIdentifier { uri },
                options: FormattingOptions {
                    tab_size: 4,
                    insert_spaces: true,
                    ..Default::default()
                },
                work_done_progress_params: Default::default(),
            };

            let result: Result<Option<Vec<TextEdit>>, _> =
                server.request("textDocument/formatting", &params).await;

            match result {
                Ok(Some(edits)) => {
                    let editor_edits: Vec<EditorTextEdit> = edits
                        .iter()
                        .map(|e| lsp_edit_to_editor(e, &lines))
                        .collect();
                    let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::Format {
                        path,
                        edits: editor_edits,
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

    // -- Notification handling -----------------------------------------------

    pub(crate) fn handle_notification(&mut self, notif: LspNotification) -> Vec<Effect> {
        let mut effects = Vec::new();

        match notif.method.as_str() {
            "textDocument/publishDiagnostics" => {
                if let Ok(params) = serde_json::from_value::<PublishDiagnosticsParams>(notif.params)
                {
                    if let Some(path) = crate::util::path_from_uri(&params.uri) {
                        log::info!(
                            "LSP diagnostics: {} count={}",
                            path.display(),
                            params.diagnostics.len()
                        );
                        let diagnostics = convert_diagnostics(&path, &params.diagnostics);
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

                        match params.value {
                            lsp_types::ProgressParamsValue::WorkDone(wd) => match wd {
                                lsp_types::WorkDoneProgress::Begin(ref begin) => {
                                    log::info!(
                                        "LSP progress begin: token={} title={:?} message={:?}",
                                        token,
                                        begin.title,
                                        begin.message
                                    );
                                    self.progress_tokens.insert(
                                        token,
                                        ProgressState {
                                            title: begin.title.clone(),
                                            message: begin.message.clone(),
                                            percentage: begin.percentage,
                                        },
                                    );
                                    effects.push(self.lsp_status_effect());
                                }
                                lsp_types::WorkDoneProgress::Report(ref report) => {
                                    // Treat 100% as implicit End — rust-analyzer
                                    // delays the real End notification.
                                    if report.percentage == Some(100) {
                                        self.progress_tokens.remove(&token);
                                    } else if let Some(state) = self.progress_tokens.get_mut(&token)
                                    {
                                        if report.message.is_some() {
                                            state.message = report.message.clone();
                                        }
                                        if report.percentage.is_some() {
                                            state.percentage = report.percentage;
                                        }
                                    }
                                    effects.push(self.lsp_status_effect());
                                }
                                lsp_types::WorkDoneProgress::End(_) => {
                                    log::info!(
                                        "LSP progress end: token={} (remaining={})",
                                        token,
                                        self.progress_tokens.len().saturating_sub(1)
                                    );
                                    self.progress_tokens.remove(&token);
                                    effects.push(self.lsp_status_effect());
                                }
                            },
                        }
                    }
                    Err(e) => {
                        log::info!("LSP $/progress deserialize error: {}", e);
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
                        self.pull_all_diagnostics();
                    }
                }
            }
            "client/registerCapability" => {
                self.handle_register_capability(&notif.params);
            }
            _ => {
                log::info!("LSP unhandled notification: {}", notif.method);
            }
        }

        effects
    }

    pub(crate) fn handle_request_result(&mut self, result: RequestResult) -> Vec<Effect> {
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
                    effects.push(Effect::Emit(Event::ApplyEdits { path, edits }));
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
            RequestResult::CodeActions { path, actions, raw } => {
                if !actions.is_empty() {
                    self.pending_code_actions.insert(path.clone(), raw);
                    effects.push(Effect::Emit(Event::ShowCodeActions { path, actions }));
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
                effects.push(Effect::Emit(Event::SetInlayHints { path, hints }));
            }
            RequestResult::Diagnostics { path, diagnostics } => {
                effects.push(Effect::Emit(Event::SetDiagnostics { path, diagnostics }));
            }
            RequestResult::Error { message } => {
                effects.push(Effect::SetMessage(format!("LSP: {}", message)));
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

            let diagnostics = match result {
                Ok(lsp_types::DocumentDiagnosticReportResult::Report(
                    lsp_types::DocumentDiagnosticReport::Full(report),
                )) => {
                    log::info!(
                        "LSP pull diagnostics: {} count={}",
                        path.display(),
                        report.full_document_diagnostic_report.items.len()
                    );
                    convert_diagnostics(&path, &report.full_document_diagnostic_report.items)
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
                diagnostics,
            }));
            if let Some(ref w) = waker {
                w();
            }
        });
    }
}

fn convert_diagnostics(path: &Path, lsp_diags: &[lsp_types::Diagnostic]) -> Vec<EditorDiagnostic> {
    let lines = read_file_lines(path);
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
            let start = from_lsp_pos(&d.range.start, &lines);
            let end = from_lsp_pos(&d.range.end, &lines);
            EditorDiagnostic {
                range: EditorRange { start, end },
                severity,
                message: d.message.clone(),
            }
        })
        .collect()
}
