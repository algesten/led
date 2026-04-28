use std::sync::Arc;
use std::time::Instant;

use led_core::ServerId;
use led_driver_lsp_core::LspEvent;
use led_state_syntax::Language;
use serde_json::Value;

use crate::protocol::{
    parse_register_capability_watched_files, parse_unregister_capability_watched_files,
    path_from_uri,
};

use super::parse::parse_diagnostic_entry;
use super::{Manager, ProgressInfo, short_server_id};

impl Manager {
    /// Emit an aggregated `LspEvent::Progress` if the throttle
    /// allows it. Throttle: 200ms minimum between sends, BUT
    /// busy→idle transitions always fire (so the UI never gets
    /// stuck with a stale spinner). Called by both the
    /// `$/progress` and `experimental/serverStatus` handlers at
    /// their tail — the two sources converge here.
    ///
    /// On a busy→idle transition the server has just finished
    /// a round of analysis (cold-index, cargo check, semantic
    /// re-check after save, …). Fire a fresh
    /// `RequestDiagnostics` so the next pull picks up whatever
    /// the server just produced. This is the main mechanism by
    /// which late cargo-check warnings reach the client when the
    /// runtime gates pulls on save (no keystroke-driven pulls).
    pub(super) fn send_progress_throttled(&mut self) {
        let busy = self.is_busy();
        let detail = self.progress_detail();
        let transitioning_to_idle = self.last_progress_busy && !busy;

        let now = Instant::now();
        if !transitioning_to_idle
            && let Some(last) = self.last_progress_sent_at
            && now.duration_since(last) < std::time::Duration::from_millis(200)
        {
            return;
        }

        let Some(server_name) = self.first_server_name() else {
            return;
        };

        let _ = self.lsp_event_tx.send(LspEvent::Progress {
            server: server_name,
            busy,
            detail,
        });
        self.notify.notify();
        self.last_progress_sent_at = Some(now);
        self.last_progress_busy = busy;

        // Side effect: re-pull on every busy→idle edge. Covers
        // both `$/progress end` (cargo check finishing) and
        // `experimental/serverStatus quiescent=true` (ra's
        // overall-done signal). rust-analyzer in pull-only mode
        // wouldn't otherwise emit anything when cargo finishes.
        if transitioning_to_idle {
            self.request_diagnostics();
        }
    }

    pub(super) fn handle_server_request(
        &mut self,
        language: Language,
        method: String,
        params: Value,
        auto_reply: Value,
        forward_as_notification: bool,
    ) {
        let server_name = {
            let Some(entry) = self.servers.get_mut(&language) else {
                return;
            };
            let body = serde_json::to_vec(&auto_reply).expect("auto-reply is valid");
            let _ = entry.server.send_body(&body);
            short_server_id(&entry.server.name)
        };
        if !forward_as_notification {
            return;
        }
        // Forwarded server-initiated requests we actually act on.
        // `client/registerCapability` and its retraction sibling
        // both narrow to `workspace/didChangeWatchedFiles` for now;
        // other dynamic registrations (completion trigger chars,
        // formatting, …) are out of scope.
        match method.as_str() {
            "client/registerCapability" => {
                let regs = parse_register_capability_watched_files(&params);
                for reg in regs {
                    let _ = self.lsp_event_tx.send(LspEvent::WatchedFilesRegistered {
                        server: server_name.clone(),
                        registration_id: reg.registration_id,
                        globs: Arc::new(reg.globs),
                    });
                    self.notify.notify();
                }
            }
            "client/unregisterCapability" => {
                let regs = parse_unregister_capability_watched_files(&params);
                for reg in regs {
                    let _ = self.lsp_event_tx.send(LspEvent::WatchedFilesUnregistered {
                        server: server_name.clone(),
                        registration_id: reg.registration_id,
                    });
                    self.notify.notify();
                }
            }
            _ => {}
        }
    }

    pub(super) fn handle_notification(
        &mut self,
        language: Language,
        method: String,
        params: Value,
    ) {
        if let Some(entry) = self.servers.get(&language) {
            let server_name = short_server_id(&entry.server.name);
            self.trace.lsp_recv_notification(&server_name, &method);
        }
        match method.as_str() {
            "textDocument/publishDiagnostics" => {
                let Some(path) = params
                    .get("uri")
                    .and_then(|u| u.as_str())
                    .and_then(path_from_uri)
                    .map(|pb| led_core::UserPath::new(pb).canonicalize())
                else {
                    return;
                };
                let diags = params
                    .get("diagnostics")
                    .and_then(|d| d.as_array())
                    .map(|arr| arr.iter().filter_map(parse_diagnostic_entry).collect())
                    .unwrap_or_default();
                let result = {
                    let entry = self.servers.get_mut(&language).unwrap();
                    // Stamp the push with the buffer's CURRENT
                    // content hash — the hash we believe matches
                    // the bytes rust-analyzer just analysed. That
                    // lets the runtime's replay pipeline map the
                    // diagnostic through any edits the user has
                    // since landed instead of pinning it to
                    // whichever hash a future drain happens to see.
                    let current_hash = entry
                        .buffer_hashes
                        .get(&path)
                        .copied()
                        .unwrap_or_default();
                    entry.diag.on_push(path.clone(), diags, current_hash)
                };
                self.dispatch_push_result(language, path, result);
            }
            "experimental/serverStatus" => {
                // rust-analyzer's custom status extension.
                // `quiescent=false` = server is working (indexing,
                // cachePriming, type-checking, …). `quiescent=true`
                // = idle. `message` carries a human-readable tail
                // that we deliberately discard — detail is owned
                // by `$/progress` exclusively, matching legacy
                // `progress_lsp_in` (manager.rs:1689-1709).
                //
                // Quiescence detection is runtime-first: the very
                // arrival of a `serverStatus` notification proves
                // the server supports the extension, regardless of
                // what its initialize capabilities advertised. On
                // first arrival we latch `has_quiescence = true`
                // (which also flips `lsp_ready = false` — the
                // server is NOT ready until it emits
                // `quiescent=true`).
                let quiescent = params
                    .get("quiescent")
                    .and_then(|q| q.as_bool())
                    .unwrap_or(false);
                let server_name = ServerId::new(self.servers[&language].server.name.clone());
                // `was_busy` reads the PREVIOUS quiescent value —
                // absent entry means default-idle (matches
                // legacy's `unwrap_or(&true)` → `!true = false`).
                let was_busy = !*self.quiescent.get(&language).unwrap_or(&true);
                {
                    let entry = self.servers.get_mut(&language).unwrap();
                    if !entry.diag.has_quiescence() {
                        entry.diag.set_has_quiescence(true);
                    }
                }
                self.quiescent.insert(language, quiescent);
                if quiescent {
                    let _ = self
                        .lsp_event_tx
                        .send(LspEvent::Ready { server: server_name });
                    self.notify.notify();
                    // Consume the deferred-init flag so
                    // `should_defer_request` stops blocking
                    // future requests. The re-pull trigger on
                    // busy→idle lives in `send_progress_throttled`
                    // below, covering both this path and
                    // `$/progress end` with one source of truth.
                    if was_busy {
                        let entry = self.servers.get_mut(&language).unwrap();
                        entry.diag.on_quiescence();
                        entry.deferred_init_request = false;
                    }
                }
                // Unified progress emission — both sources
                // converge through `send_progress_throttled`.
                self.send_progress_throttled();
            }
            "$/progress" => {
                // Progress token lifecycle: `begin` inserts a new
                // token with title+message; `report` updates; `end`
                // removes. `report` with `percentage=100` is
                // promoted to `end` (matches legacy's
                // `classify_progress` at manager.rs:2042-2052).
                let _ = language;
                let token = params
                    .get("token")
                    .map(|t| match t {
                        Value::String(s) => s.clone(),
                        Value::Number(n) => n.to_string(),
                        _ => String::new(),
                    })
                    .unwrap_or_default();
                if token.is_empty() {
                    return;
                }
                let kind = params
                    .pointer("/value/kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("");
                let title = params
                    .pointer("/value/title")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string());
                let message = params
                    .pointer("/value/message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string());
                let percentage = params
                    .pointer("/value/percentage")
                    .and_then(|p| p.as_u64());
                let effective_kind = if kind == "report" && percentage == Some(100) {
                    "end"
                } else {
                    kind
                };
                match effective_kind {
                    "begin" => {
                        self.progress_tokens
                            .insert(token, ProgressInfo { title, message });
                    }
                    "report" => {
                        let entry = self.progress_tokens.entry(token).or_default();
                        if title.is_some() {
                            entry.title = title;
                        }
                        if message.is_some() {
                            entry.message = message;
                        }
                    }
                    "end" => {
                        self.progress_tokens.remove(&token);
                    }
                    _ => {}
                }
                self.send_progress_throttled();
            }
            "window/logMessage" | "window/showMessage" => {
                // Ignored for now. `client/registerCapability` is
                // a request (id+method), not a notification, so it
                // routes through `handle_server_request` —
                // intentionally absent from this match.
            }
            _ => {}
        }
    }
}
