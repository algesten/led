use std::path::Path;
use std::sync::Arc;

use led_core::{Action, Component, Context, DrawContext, Effect, Event, LspStatus, PanelClaim};
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::LspManager;
use crate::server::LanguageServer;
use crate::types::LspManagerEvent;

/// Fetch a single line from DocStore, falling back to disk.
fn doc_line(path: &Path, row: usize, ctx: &Context) -> Option<String> {
    if let Some(line) = ctx.docs.line(path, row) {
        return Some(line);
    }
    let content = std::fs::read_to_string(path).ok()?;
    content.lines().nth(row).map(|l| l.to_string())
}

impl Component for LspManager {
    fn panel_claims(&self) -> &[PanelClaim] {
        &[]
    }

    fn handle_action(&mut self, action: Action, ctx: &mut Context) -> Vec<Effect> {
        match action {
            Action::Tick => {
                let mut effects = Vec::new();
                while let Ok(event) = self.event_rx.try_recv() {
                    match event {
                        LspManagerEvent::ServerStarted {
                            language_id,
                            server,
                        } => {
                            log::info!("LSP server started: {} ({})", server.name, language_id);
                            self.pending_starts.remove(&language_id);
                            self.servers.insert(language_id.clone(), server.clone());
                            // Assume busy until server reports quiescent
                            self.quiescent = false;
                            effects.push(Effect::SetLspStatus(LspStatus {
                                server_name: server.name.clone(),
                                busy: true,
                                detail: None,
                            }));
                            // Propagate completion trigger characters
                            if let Some(caps) = server.capabilities.lock().unwrap().as_ref() {
                                if let Some(ref cp) = caps.completion_provider {
                                    if let Some(ref triggers) = cp.trigger_characters {
                                        let extensions = self
                                            .registry
                                            .config_for_language(&language_id)
                                            .map(|c| {
                                                c.extensions.iter().map(|s| s.to_string()).collect()
                                            })
                                            .unwrap_or_default();
                                        effects.push(Effect::Emit(Event::SetCompletionTriggers {
                                            extensions,
                                            triggers: triggers.clone(),
                                        }));
                                    }
                                }
                            }
                            // Send didOpen for any docs that were waiting for this server
                            let pending: Vec<std::path::PathBuf> =
                                self.pending_opens.iter().cloned().collect();
                            for path in pending {
                                if self.server_for_path(&path).is_some() {
                                    self.pending_opens.remove(&path);
                                    self.send_did_open(&path, &*ctx.docs);
                                }
                            }
                        }
                        LspManagerEvent::ServerError { error } => {
                            log::error!("LSP server error: {}", error);
                            effects.push(Effect::SetMessage(format!("LSP: {}", error)));
                        }
                        LspManagerEvent::Notification(notif) => {
                            effects.extend(self.handle_notification(notif, &*ctx.docs));
                        }
                        LspManagerEvent::RequestResult(result) => {
                            effects.extend(self.handle_request_result(result, &*ctx.docs));
                        }
                        LspManagerEvent::FileChanged(path) => {
                            self.send_file_changed(&path);
                        }
                    }
                }
                effects
            }
            _ => vec![],
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> Vec<Effect> {
        match event {
            Event::TabActivated { path: Some(path) } => {
                self.ensure_server_for_path(path);
                self.send_did_open(path, &*ctx.docs);
            }
            Event::BufferChanged { path } => {
                let changes = ctx.docs.drain_changes(path);
                if !changes.is_empty() {
                    let version = ctx.docs.version(path).unwrap_or(0);
                    self.send_did_change(path, &changes, version, &*ctx.docs);
                }
            }
            Event::FileSaved(path) => {
                self.send_did_save(path);
            }
            Event::BufferClosed(path) => {
                self.send_did_close(path);
            }
            Event::LspGotoDefinition { path, row, col } => {
                let line = doc_line(path, *row, ctx);
                self.spawn_goto_definition(path.clone(), *row, *col, line);
            }
            Event::LspInlayHints {
                path,
                start_row,
                end_row,
            } => {
                self.spawn_inlay_hints(path.clone(), *start_row, *end_row);
            }
            Event::LspRename {
                path,
                row,
                col,
                new_name,
            } => {
                let line = doc_line(path, *row, ctx);
                self.spawn_rename(path.clone(), *row, *col, new_name.clone(), line);
            }
            Event::LspCodeAction {
                path,
                start_row,
                start_col,
                end_row,
                end_col,
            } => {
                let start_line = doc_line(path, *start_row, ctx);
                let end_line = if *end_row == *start_row {
                    start_line.clone()
                } else {
                    doc_line(path, *end_row, ctx)
                };
                self.spawn_code_action(
                    path.clone(),
                    *start_row,
                    *start_col,
                    *end_row,
                    *end_col,
                    start_line,
                    end_line,
                );
            }
            Event::LspCodeActionResolve { path, index } => {
                self.spawn_code_action_resolve(path.clone(), *index);
            }
            Event::LspFormat { path, generation } => {
                if self.server_for_path(path).is_some() {
                    self.spawn_format(path.clone(), *generation);
                } else {
                    return vec![Effect::Emit(Event::FormatDone {
                        path: path.clone(),
                        generation: *generation,
                    })];
                }
            }
            Event::LspCompletion { path, row, col } => {
                let line = doc_line(path, *row, ctx);
                self.spawn_completion(path.clone(), *row, *col, line);
            }
            Event::LspResolveCompletion {
                path,
                lsp_item_json,
            } => {
                self.spawn_completion_resolve(path.clone(), lsp_item_json.clone());
            }
            _ => {}
        }
        vec![]
    }

    fn draw(&mut self, _f: &mut Frame, _a: Rect, _ctx: &mut DrawContext) {}
}

impl Drop for LspManager {
    fn drop(&mut self) {
        let servers: Vec<Arc<LanguageServer>> = self.servers.values().cloned().collect();
        if !servers.is_empty() {
            tokio::spawn(async move {
                for server in servers {
                    server.shutdown().await;
                }
            });
        }
    }
}
