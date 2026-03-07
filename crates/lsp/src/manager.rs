use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use led_core::lsp_types::{
    DiagnosticSeverity, EditorCodeAction, EditorDiagnostic, EditorInlayHint, EditorPosition,
    EditorRange, EditorTextEdit,
};
use led_core::{Action, Component, Context, DrawContext, Effect, Event, PanelClaim, Waker};

use crate::registry::LspRegistry;
use crate::server::LanguageServer;
use crate::transport::LspNotification;

use ratatui::Frame;
use ratatui::layout::Rect;

fn uri_from_path(path: &Path) -> Option<lsp_types::Uri> {
    let s = format!("file://{}", path.to_str()?);
    s.parse().ok()
}

fn path_from_uri(uri: &lsp_types::Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    if let Some(stripped) = s.strip_prefix("file://") {
        Some(PathBuf::from(stripped))
    } else {
        None
    }
}

/// Convert LSP UTF-16 column offset to char column for a given line.
fn utf16_col_to_char_col(line: &str, utf16_col: u32) -> usize {
    let mut utf16_offset: u32 = 0;
    for (char_idx, ch) in line.chars().enumerate() {
        if utf16_offset >= utf16_col {
            return char_idx;
        }
        utf16_offset += ch.len_utf16() as u32;
    }
    line.chars().count()
}

/// Convert char column to UTF-16 column offset for a given line.
fn char_col_to_utf16_col(line: &str, char_col: usize) -> u32 {
    line.chars()
        .take(char_col)
        .map(|ch| ch.len_utf16() as u32)
        .sum()
}

fn lsp_pos(row: usize, col: usize, lines: &[String]) -> lsp_types::Position {
    let line_text = lines.get(row).map(|s| s.as_str()).unwrap_or("");
    lsp_types::Position {
        line: row as u32,
        character: char_col_to_utf16_col(line_text, col),
    }
}

fn from_lsp_pos(pos: &lsp_types::Position, lines: &[String]) -> EditorPosition {
    let row = pos.line as usize;
    let line_text = lines.get(row).map(|s| s.as_str()).unwrap_or("");
    EditorPosition {
        row,
        col: utf16_col_to_char_col(line_text, pos.character),
    }
}

fn read_file_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.to_string())
        .collect()
}

enum LspManagerEvent {
    ServerStarted {
        language_id: String,
        server: Arc<LanguageServer>,
    },
    ServerError {
        error: String,
    },
    Notification {
        #[allow(dead_code)]
        server_name: String,
        method: String,
        params: Value,
    },
    RequestResult(RequestResult),
}

enum RequestResult {
    GotoDefinition {
        locations: Vec<(PathBuf, usize, usize)>,
    },
    Format {
        path: PathBuf,
        edits: Vec<EditorTextEdit>,
    },
    Rename {
        file_edits: Vec<(PathBuf, Vec<EditorTextEdit>)>,
    },
    CodeActions {
        path: PathBuf,
        actions: Vec<EditorCodeAction>,
        raw_actions: Vec<lsp_types::CodeActionOrCommand>,
    },
    CodeActionResolved {
        file_edits: Vec<(PathBuf, Vec<EditorTextEdit>)>,
    },
    InlayHints {
        path: PathBuf,
        hints: Vec<EditorInlayHint>,
    },
    Error {
        message: String,
    },
}

pub struct LspManager {
    registry: LspRegistry,
    servers: HashMap<String, Arc<LanguageServer>>,
    root: PathBuf,
    event_rx: mpsc::UnboundedReceiver<LspManagerEvent>,
    event_tx: mpsc::UnboundedSender<LspManagerEvent>,
    waker: Option<Waker>,
    pending_starts: HashSet<String>,
    opened_docs: HashSet<PathBuf>,
    pending_code_actions: HashMap<PathBuf, Vec<lsp_types::CodeActionOrCommand>>,
}

impl LspManager {
    pub fn new(root: PathBuf, waker: Option<Waker>) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            registry: LspRegistry::new(),
            servers: HashMap::new(),
            root,
            event_rx,
            event_tx,
            waker,
            pending_starts: HashSet::new(),
            opened_docs: HashSet::new(),
            pending_code_actions: HashMap::new(),
        }
    }

    fn extension_for_path(path: &PathBuf) -> Option<String> {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_string())
    }

    fn server_for_path(&self, path: &Path) -> Option<Arc<LanguageServer>> {
        let ext = path.extension()?.to_str()?;
        let config = self.registry.config_for_extension(ext)?;
        self.servers.get(&config.language_id).cloned()
    }

    fn ensure_server_for_path(&mut self, path: &PathBuf) {
        let Some(ext) = Self::extension_for_path(path) else {
            return;
        };
        let Some(config) = self.registry.config_for_extension(&ext) else {
            return;
        };
        let language_id = config.language_id.clone();

        if self.servers.contains_key(&language_id) || self.pending_starts.contains(&language_id) {
            return;
        }

        self.pending_starts.insert(language_id.clone());

        let Some(root_uri) = uri_from_path(&self.root) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        let config_command = config.command.clone();
        let config_args = config.args.clone();
        let config_language_id = config.language_id.clone();
        let config_extensions = config.extensions.clone();

        let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<LspNotification>();

        let event_tx2 = event_tx.clone();
        let server_name = config_command.clone();
        tokio::spawn(async move {
            while let Some(notif) = notif_rx.recv().await {
                let _ = event_tx2.send(LspManagerEvent::Notification {
                    server_name: server_name.clone(),
                    method: notif.method,
                    params: notif.params,
                });
            }
        });

        let lang_id = language_id.clone();
        let waker2 = self.waker.clone();
        tokio::spawn(async move {
            let config = crate::registry::ServerConfig {
                language_id: config_language_id,
                command: config_command,
                args: config_args,
                extensions: config_extensions,
            };
            match LanguageServer::start(&config, root_uri, notif_tx, waker).await {
                Ok(server) => {
                    let _ = event_tx.send(LspManagerEvent::ServerStarted {
                        language_id: lang_id,
                        server: Arc::new(server),
                    });
                }
                Err(e) => {
                    let _ = event_tx.send(LspManagerEvent::ServerError { error: e });
                }
            }
            if let Some(w) = waker2 {
                w();
            }
        });
    }

    fn send_did_open(&mut self, path: &PathBuf) {
        if self.opened_docs.contains(path) {
            return;
        }
        let Some(ext) = Self::extension_for_path(path) else {
            return;
        };
        let Some(config) = self.registry.config_for_extension(&ext) else {
            return;
        };
        // Track the doc so it gets opened when the server starts
        self.opened_docs.insert(path.clone());

        let Some(server) = self.servers.get(&config.language_id) else {
            return;
        };

        let Some(uri) = uri_from_path(path) else {
            return;
        };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return,
        };

        server.notify::<lsp_types::notification::DidOpenTextDocument>(
            lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri,
                    language_id: config.language_id.clone(),
                    version: 0,
                    text,
                },
            },
        );
    }

    fn send_did_save(&self, path: &PathBuf) {
        let Some(ext) = Self::extension_for_path(path) else {
            return;
        };
        let Some(config) = self.registry.config_for_extension(&ext) else {
            return;
        };
        let Some(server) = self.servers.get(&config.language_id) else {
            return;
        };

        let Some(uri) = uri_from_path(path) else {
            return;
        };
        let text = std::fs::read_to_string(path).ok();

        server.notify::<lsp_types::notification::DidSaveTextDocument>(
            lsp_types::DidSaveTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
                text,
            },
        );
    }

    fn send_did_close(&mut self, path: &PathBuf) {
        if !self.opened_docs.remove(path) {
            return;
        }
        let Some(ext) = Self::extension_for_path(path) else {
            return;
        };
        let Some(config) = self.registry.config_for_extension(&ext) else {
            return;
        };
        let Some(server) = self.servers.get(&config.language_id) else {
            return;
        };

        let Some(uri) = uri_from_path(path) else {
            return;
        };

        server.notify::<lsp_types::notification::DidCloseTextDocument>(
            lsp_types::DidCloseTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
            },
        );
    }

    fn handle_notification(&self, method: &str, params: Value) -> Vec<Effect> {
        if method == "textDocument/publishDiagnostics" {
            if let Ok(diag_params) =
                serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(params)
            {
                let Some(path) = path_from_uri(&diag_params.uri) else {
                    return vec![];
                };
                let lines = read_file_lines(&path);
                let diagnostics: Vec<EditorDiagnostic> = diag_params
                    .diagnostics
                    .iter()
                    .map(|d| {
                        let severity = match d.severity {
                            Some(lsp_types::DiagnosticSeverity::ERROR) => DiagnosticSeverity::Error,
                            Some(lsp_types::DiagnosticSeverity::WARNING) => {
                                DiagnosticSeverity::Warning
                            }
                            Some(lsp_types::DiagnosticSeverity::INFORMATION) => {
                                DiagnosticSeverity::Info
                            }
                            Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
                            _ => DiagnosticSeverity::Error,
                        };
                        EditorDiagnostic {
                            range: EditorRange {
                                start: from_lsp_pos(&d.range.start, &lines),
                                end: from_lsp_pos(&d.range.end, &lines),
                            },
                            severity,
                            message: d.message.clone(),
                        }
                    })
                    .collect();
                return vec![Effect::Emit(Event::SetDiagnostics { path, diagnostics })];
            }
        }
        // Forward other notifications as before
        vec![]
    }

    fn spawn_goto_definition(&self, path: PathBuf, row: usize, col: usize) -> Vec<Effect> {
        let Some(server) = self.server_for_path(&path) else {
            return vec![Effect::SetMessage("No LSP server for this file".into())];
        };
        let Some(uri) = uri_from_path(&path) else {
            return vec![];
        };
        let lines = read_file_lines(&path);
        let pos = lsp_pos(row, col, &lines);
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let params = lsp_types::GotoDefinitionParams {
                text_document_position_params: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri },
                    position: pos,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            match server
                .request::<lsp_types::request::GotoDefinition>(params)
                .await
            {
                Ok(resp) => {
                    let locs = match resp {
                        Some(lsp_types::GotoDefinitionResponse::Scalar(loc)) => vec![loc],
                        Some(lsp_types::GotoDefinitionResponse::Array(locs)) => {
                            locs.into_iter().map(Into::into).collect()
                        }
                        Some(lsp_types::GotoDefinitionResponse::Link(links)) => links
                            .into_iter()
                            .map(|l| lsp_types::Location {
                                uri: l.target_uri,
                                range: l.target_selection_range,
                            })
                            .collect(),
                        None => vec![],
                    };
                    let locations: Vec<(PathBuf, usize, usize)> = locs
                        .into_iter()
                        .filter_map(|loc| {
                            let p = path_from_uri(&loc.uri)?;
                            let file_lines = read_file_lines(&p);
                            let epos = from_lsp_pos(&loc.range.start, &file_lines);
                            Some((p, epos.row, epos.col))
                        })
                        .collect();
                    let _ = event_tx.send(LspManagerEvent::RequestResult(
                        RequestResult::GotoDefinition { locations },
                    ));
                }
                Err(e) => {
                    let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::Error {
                        message: format!("Go to definition: {e}"),
                    }));
                }
            }
            if let Some(w) = waker {
                w();
            }
        });
        vec![]
    }

    fn spawn_format(&self, path: PathBuf) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        // Check if server supports formatting
        if let Some(caps) = server.capabilities() {
            match &caps.document_formatting_provider {
                Some(lsp_types::OneOf::Left(true)) | Some(lsp_types::OneOf::Right(_)) => {}
                _ => return,
            }
        }
        let Some(uri) = uri_from_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();
        let file_path = path.clone();

        tokio::spawn(async move {
            let params = lsp_types::DocumentFormattingParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
                options: lsp_types::FormattingOptions {
                    tab_size: 4,
                    insert_spaces: true,
                    ..Default::default()
                },
                work_done_progress_params: Default::default(),
            };
            match server
                .request::<lsp_types::request::Formatting>(params)
                .await
            {
                Ok(Some(text_edits)) => {
                    let lines = read_file_lines(&file_path);
                    let edits: Vec<EditorTextEdit> = text_edits
                        .into_iter()
                        .map(|te| lsp_edit_to_editor(&te, &lines))
                        .collect();
                    let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::Format {
                        path: file_path,
                        edits,
                    }));
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::Error {
                        message: format!("Format: {e}"),
                    }));
                }
            }
            if let Some(w) = waker {
                w();
            }
        });
    }

    fn spawn_rename(&self, path: PathBuf, row: usize, col: usize, new_name: String) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let Some(uri) = uri_from_path(&path) else {
            return;
        };
        let lines = read_file_lines(&path);
        let pos = lsp_pos(row, col, &lines);
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let params = lsp_types::RenameParams {
                text_document_position: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri },
                    position: pos,
                },
                new_name,
                work_done_progress_params: Default::default(),
            };
            match server.request::<lsp_types::request::Rename>(params).await {
                Ok(Some(workspace_edit)) => {
                    let file_edits = workspace_edit_to_file_edits(&workspace_edit);
                    let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::Rename {
                        file_edits,
                    }));
                }
                Ok(None) => {
                    let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::Error {
                        message: "Rename: no changes".into(),
                    }));
                }
                Err(e) => {
                    let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::Error {
                        message: format!("Rename: {e}"),
                    }));
                }
            }
            if let Some(w) = waker {
                w();
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
        let Some(uri) = uri_from_path(&path) else {
            return;
        };
        let lines = read_file_lines(&path);
        let start = lsp_pos(start_row, start_col, &lines);
        let end = lsp_pos(end_row, end_col, &lines);
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();
        let action_path = path.clone();

        tokio::spawn(async move {
            let params = lsp_types::CodeActionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
                range: lsp_types::Range { start, end },
                context: lsp_types::CodeActionContext {
                    diagnostics: vec![],
                    only: None,
                    trigger_kind: Some(lsp_types::CodeActionTriggerKind::INVOKED),
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            match server
                .request::<lsp_types::request::CodeActionRequest>(params)
                .await
            {
                Ok(Some(actions)) => {
                    let editor_actions: Vec<EditorCodeAction> = actions
                        .iter()
                        .enumerate()
                        .map(|(i, a)| {
                            let title = match a {
                                lsp_types::CodeActionOrCommand::CodeAction(ca) => ca.title.clone(),
                                lsp_types::CodeActionOrCommand::Command(cmd) => cmd.title.clone(),
                            };
                            EditorCodeAction { title, index: i }
                        })
                        .collect();
                    let _ =
                        event_tx.send(LspManagerEvent::RequestResult(RequestResult::CodeActions {
                            path: action_path,
                            actions: editor_actions,
                            raw_actions: actions,
                        }));
                }
                Ok(None) => {
                    let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::Error {
                        message: "No code actions available".into(),
                    }));
                }
                Err(e) => {
                    let _ = event_tx.send(LspManagerEvent::RequestResult(RequestResult::Error {
                        message: format!("Code action: {e}"),
                    }));
                }
            }
            if let Some(w) = waker {
                w();
            }
        });
    }

    fn spawn_code_action_resolve(&self, path: PathBuf, index: usize) {
        let Some(actions) = self.pending_code_actions.get(&path) else {
            return;
        };
        let Some(action) = actions.get(index) else {
            return;
        };

        match action {
            lsp_types::CodeActionOrCommand::CodeAction(ca) => {
                if let Some(ref edit) = ca.edit {
                    // Already has an edit, apply it directly
                    let file_edits = workspace_edit_to_file_edits(edit);
                    let event_tx = self.event_tx.clone();
                    let _ = event_tx.send(LspManagerEvent::RequestResult(
                        RequestResult::CodeActionResolved { file_edits },
                    ));
                    if let Some(w) = &self.waker {
                        w();
                    }
                    return;
                }
                // Need to resolve
                let Some(server) = self.server_for_path(&path) else {
                    return;
                };
                let ca = ca.clone();
                let event_tx = self.event_tx.clone();
                let waker = self.waker.clone();

                tokio::spawn(async move {
                    match server
                        .request::<lsp_types::request::CodeActionResolveRequest>(ca)
                        .await
                    {
                        Ok(resolved) => {
                            if let Some(edit) = resolved.edit {
                                let file_edits = workspace_edit_to_file_edits(&edit);
                                let _ = event_tx.send(LspManagerEvent::RequestResult(
                                    RequestResult::CodeActionResolved { file_edits },
                                ));
                            }
                        }
                        Err(e) => {
                            let _ = event_tx.send(LspManagerEvent::RequestResult(
                                RequestResult::Error {
                                    message: format!("Code action resolve: {e}"),
                                },
                            ));
                        }
                    }
                    if let Some(w) = waker {
                        w();
                    }
                });
            }
            lsp_types::CodeActionOrCommand::Command(_cmd) => {
                // Commands not supported yet
            }
        }
    }

    fn spawn_inlay_hints(&self, path: PathBuf, start_row: usize, end_row: usize) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let Some(uri) = uri_from_path(&path) else {
            return;
        };
        let lines = read_file_lines(&path);
        let start = lsp_pos(start_row, 0, &lines);
        let end = lsp_pos(end_row, 0, &lines);
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();
        let hint_path = path.clone();

        tokio::spawn(async move {
            let params = lsp_types::InlayHintParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
                range: lsp_types::Range { start, end },
                work_done_progress_params: Default::default(),
            };
            match server
                .request::<lsp_types::request::InlayHintRequest>(params)
                .await
            {
                Ok(Some(hints)) => {
                    let file_lines = read_file_lines(&hint_path);
                    let editor_hints: Vec<EditorInlayHint> = hints
                        .into_iter()
                        .map(|h| {
                            let epos = from_lsp_pos(&h.position, &file_lines);
                            let label = match h.label {
                                lsp_types::InlayHintLabel::String(s) => s,
                                lsp_types::InlayHintLabel::LabelParts(parts) => {
                                    parts.iter().map(|p| p.value.as_str()).collect()
                                }
                            };
                            EditorInlayHint {
                                position: epos,
                                label,
                            }
                        })
                        .collect();
                    let _ =
                        event_tx.send(LspManagerEvent::RequestResult(RequestResult::InlayHints {
                            path: hint_path,
                            hints: editor_hints,
                        }));
                }
                Ok(None) => {}
                Err(_) => {}
            }
            if let Some(w) = waker {
                w();
            }
        });
    }

    fn handle_lsp_event(&mut self, event: &Event) -> Vec<Effect> {
        match event {
            Event::LspGotoDefinition { path, row, col } => {
                return self.spawn_goto_definition(path.clone(), *row, *col);
            }
            Event::LspFormat { path } => {
                self.spawn_format(path.clone());
            }
            Event::LspRename {
                path,
                row,
                col,
                new_name,
            } => {
                self.spawn_rename(path.clone(), *row, *col, new_name.clone());
            }
            Event::LspCodeAction {
                path,
                start_row,
                start_col,
                end_row,
                end_col,
            } => {
                self.spawn_code_action(path.clone(), *start_row, *start_col, *end_row, *end_col);
            }
            Event::LspCodeActionResolve { path, index } => {
                self.spawn_code_action_resolve(path.clone(), *index);
            }
            Event::LspInlayHints {
                path,
                start_row,
                end_row,
            } => {
                self.spawn_inlay_hints(path.clone(), *start_row, *end_row);
            }
            _ => {}
        }
        vec![]
    }

    fn handle_request_result(&mut self, result: RequestResult) -> Vec<Effect> {
        match result {
            RequestResult::GotoDefinition { locations } => {
                if let Some((path, row, col)) = locations.into_iter().next() {
                    vec![
                        Effect::Emit(Event::OpenFile(path.clone())),
                        Effect::Emit(Event::GoToPosition {
                            path,
                            row,
                            col,
                            scroll_offset: None,
                        }),
                        Effect::FocusPanel(led_core::PanelSlot::Main),
                    ]
                } else {
                    vec![Effect::SetMessage("No definition found".into())]
                }
            }
            RequestResult::Format { path, edits } => {
                if edits.is_empty() {
                    vec![]
                } else {
                    vec![Effect::Emit(Event::ApplyEdits { path, edits })]
                }
            }
            RequestResult::Rename { file_edits } => {
                let mut effects = Vec::new();
                for (path, edits) in file_edits {
                    if self.opened_docs.contains(&path) {
                        effects.push(Effect::Emit(Event::ApplyEdits {
                            path: path.clone(),
                            edits,
                        }));
                    } else {
                        // Apply edits directly to disk for closed files
                        apply_edits_to_disk(&path, &edits);
                    }
                    effects.push(Effect::Emit(Event::FileSaved(path)));
                }
                effects
            }
            RequestResult::CodeActions {
                path,
                actions,
                raw_actions,
            } => {
                if actions.is_empty() {
                    vec![Effect::SetMessage("No code actions available".into())]
                } else {
                    self.pending_code_actions.insert(path.clone(), raw_actions);
                    vec![Effect::Emit(Event::ShowCodeActions { path, actions })]
                }
            }
            RequestResult::CodeActionResolved { file_edits } => {
                let mut effects = Vec::new();
                for (path, edits) in file_edits {
                    if self.opened_docs.contains(&path) {
                        effects.push(Effect::Emit(Event::ApplyEdits {
                            path: path.clone(),
                            edits,
                        }));
                    } else {
                        apply_edits_to_disk(&path, &edits);
                    }
                }
                effects
            }
            RequestResult::InlayHints { path, hints } => {
                vec![Effect::Emit(Event::SetInlayHints { path, hints })]
            }
            RequestResult::Error { message } => {
                vec![Effect::SetMessage(message)]
            }
        }
    }
}

fn lsp_edit_to_editor(te: &lsp_types::TextEdit, lines: &[String]) -> EditorTextEdit {
    EditorTextEdit {
        range: EditorRange {
            start: from_lsp_pos(&te.range.start, lines),
            end: from_lsp_pos(&te.range.end, lines),
        },
        new_text: te.new_text.clone(),
    }
}

fn workspace_edit_to_file_edits(
    edit: &lsp_types::WorkspaceEdit,
) -> Vec<(PathBuf, Vec<EditorTextEdit>)> {
    let mut result = Vec::new();
    if let Some(ref changes) = edit.changes {
        for (uri, text_edits) in changes {
            if let Some(path) = path_from_uri(uri) {
                let lines = read_file_lines(&path);
                let edits: Vec<EditorTextEdit> = text_edits
                    .iter()
                    .map(|te| lsp_edit_to_editor(te, &lines))
                    .collect();
                result.push((path, edits));
            }
        }
    }
    if let Some(ref doc_changes) = edit.document_changes {
        match doc_changes {
            lsp_types::DocumentChanges::Edits(edits) => {
                for edit in edits {
                    if let Some(path) = path_from_uri(&edit.text_document.uri) {
                        let lines = read_file_lines(&path);
                        let edits: Vec<EditorTextEdit> = edit
                            .edits
                            .iter()
                            .filter_map(|e| match e {
                                lsp_types::OneOf::Left(te) => Some(lsp_edit_to_editor(te, &lines)),
                                lsp_types::OneOf::Right(annotated) => {
                                    Some(lsp_edit_to_editor(&annotated.text_edit, &lines))
                                }
                            })
                            .collect();
                        result.push((path, edits));
                    }
                }
            }
            lsp_types::DocumentChanges::Operations(ops) => {
                for op in ops {
                    if let lsp_types::DocumentChangeOperation::Edit(edit) = op {
                        if let Some(path) = path_from_uri(&edit.text_document.uri) {
                            let lines = read_file_lines(&path);
                            let edits: Vec<EditorTextEdit> = edit
                                .edits
                                .iter()
                                .filter_map(|e| match e {
                                    lsp_types::OneOf::Left(te) => {
                                        Some(lsp_edit_to_editor(te, &lines))
                                    }
                                    lsp_types::OneOf::Right(annotated) => {
                                        Some(lsp_edit_to_editor(&annotated.text_edit, &lines))
                                    }
                                })
                                .collect();
                            result.push((path, edits));
                        }
                    }
                }
            }
        }
    }
    result
}

fn apply_edits_to_disk(path: &Path, edits: &[EditorTextEdit]) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut rope = ropey::Rope::from_str(&content);

    let mut sorted_edits: Vec<&EditorTextEdit> = edits.iter().collect();
    sorted_edits.sort_by(|a, b| {
        b.range
            .start
            .row
            .cmp(&a.range.start.row)
            .then(b.range.start.col.cmp(&a.range.start.col))
    });

    for edit in sorted_edits {
        let start_row = edit.range.start.row.min(lines.len().saturating_sub(1));
        let end_row = edit.range.end.row.min(lines.len().saturating_sub(1));
        let start_col = edit.range.start.col;
        let end_col = edit.range.end.col;

        let start_idx = rope.line_to_char(start_row) + start_col;
        let end_idx = rope.line_to_char(end_row) + end_col;

        if start_idx < end_idx {
            rope.remove(start_idx..end_idx);
        }
        if !edit.new_text.is_empty() {
            rope.insert(start_idx, &edit.new_text);
        }
    }

    let _ = std::fs::write(path, rope.to_string());
}

impl Component for LspManager {
    fn panel_claims(&self) -> &[PanelClaim] {
        &[]
    }

    fn handle_action(&mut self, action: Action, _ctx: &mut Context) -> Vec<Effect> {
        match action {
            Action::Tick => {
                let mut effects = Vec::new();
                while let Ok(event) = self.event_rx.try_recv() {
                    match event {
                        LspManagerEvent::ServerStarted {
                            language_id,
                            server,
                        } => {
                            self.pending_starts.remove(&language_id);
                            let name = server.name.clone();
                            self.servers.insert(language_id, server);
                            let paths: Vec<_> = self.opened_docs.iter().cloned().collect();
                            self.opened_docs.clear();
                            for path in paths {
                                self.send_did_open(&path);
                            }
                            effects.push(Effect::SetMessage(format!("LSP: {name} started")));
                        }
                        LspManagerEvent::ServerError { error, .. } => {
                            effects.push(Effect::SetMessage(format!("LSP error: {error}")));
                        }
                        LspManagerEvent::Notification { method, params, .. } => {
                            effects.extend(self.handle_notification(&method, params));
                        }
                        LspManagerEvent::RequestResult(result) => {
                            effects.extend(self.handle_request_result(result));
                        }
                    }
                }
                effects
            }
            _ => vec![],
        }
    }

    fn handle_event(&mut self, event: &Event, _ctx: &mut Context) -> Vec<Effect> {
        match event {
            Event::OpenFile(path) => {
                self.ensure_server_for_path(path);
                self.send_did_open(path);
            }
            Event::TabActivated { path: Some(path) } => {
                self.ensure_server_for_path(path);
                self.send_did_open(path);
            }
            Event::FileSaved(path) => {
                self.send_did_save(path);
                // Trigger formatting on save
                self.spawn_format(path.clone());
            }
            Event::BufferClosed(path) => {
                self.send_did_close(path);
            }
            Event::LspGotoDefinition { .. }
            | Event::LspFormat { .. }
            | Event::LspRename { .. }
            | Event::LspCodeAction { .. }
            | Event::LspCodeActionResolve { .. }
            | Event::LspInlayHints { .. } => {
                return self.handle_lsp_event(event);
            }
            _ => {}
        }
        vec![]
    }

    fn draw(&mut self, _frame: &mut Frame, _area: Rect, _ctx: &mut DrawContext) {}
}

impl Drop for LspManager {
    fn drop(&mut self) {
        let servers: Vec<Arc<LanguageServer>> = self.servers.drain().map(|(_, s)| s).collect();
        for server in servers {
            tokio::spawn(async move {
                if let Ok(server) = Arc::try_unwrap(server) {
                    server.shutdown().await;
                }
            });
        }
    }
}
