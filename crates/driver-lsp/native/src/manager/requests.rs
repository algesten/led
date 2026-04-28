use std::collections::HashMap;
use std::sync::Arc;

use led_core::{BufferVersion, CanonPath, LspRequestSeq, PersistedContentHash};
use led_driver_lsp_core::{
    LspEvent,
    diag_source::DiagMode,
};
use led_state_syntax::Language;
use serde_json::{Value, json};

use crate::protocol::{parse_completion_response, parse_resolve_additional_edits, uri_from_path};

use super::parse::{
    parse_definition_location, parse_diagnostic_result, parse_inlay_hints,
    parse_text_edit_list, parse_workspace_edit,
};
use super::{Manager, PendingRequest, short_server_id};

impl Manager {
    pub(super) fn request_diagnostics(&mut self) {
        self.trace.lsp_request_diagnostics();
        // We iterate servers; per-server: open a window with the
        // snapshot of every currently-opened buffer. Defer if the
        // server isn't ready yet (quiescence gate).
        let languages: Vec<Language> = self.servers.keys().copied().collect();
        for lang in languages {
            let (snapshot, opened, should_defer) = {
                let entry = self.servers.get_mut(&lang).unwrap();
                if !entry.initialized {
                    // Drop silently — the post-init flush doesn't
                    // auto-request diagnostics; the runtime's next
                    // trigger will.
                    continue;
                }
                if entry.diag.should_defer_request() {
                    entry.deferred_init_request = true;
                    entry.diag.defer_init_request();
                    continue;
                }
                let snap = entry.buffer_hashes.clone();
                let opened = entry.doc_versions.keys().cloned().collect();
                (snap, opened, false)
            };
            let _ = should_defer;
            self.open_diag_window(lang, snapshot, opened);
        }
    }

    fn open_diag_window(
        &mut self,
        lang: Language,
        snapshot: HashMap<CanonPath, PersistedContentHash>,
        opened: std::collections::HashSet<CanonPath>,
    ) {
        let pulls_and_cache = {
            let entry = self.servers.get_mut(&lang).unwrap();
            let pulls = entry.diag.open_window(snapshot, &opened);
            let cache = if entry.diag.mode() == DiagMode::Push {
                entry.diag.drain_cache_for_window()
            } else {
                Vec::new()
            };
            (pulls, cache)
        };
        let (pulls, cache) = pulls_and_cache;

        // Forward cached push results immediately.
        for (path, diags, hash) in cache {
            let _ = self.lsp_event_tx.send(LspEvent::Diagnostics {
                path: path.clone(),
                hash,
                diagnostics: diags,
            });
            self.trace.lsp_diagnostics_done(&path, 0, hash);
        }
        self.notify.notify();

        // Issue pulls.
        for path in pulls {
            let id = self.fresh_id();
            let entry = self.servers.get_mut(&lang).unwrap();
            let uri = uri_from_path(&path);
            let server_name = short_server_id(&entry.server.name);
            let body = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "textDocument/diagnostic",
                "params": {
                    "textDocument": { "uri": uri.clone() },
                },
            });
            let _ = entry.server.send_body(&serde_json::to_vec(&body).unwrap());
            self.trace.lsp_send_request(
                &server_name,
                "textDocument/diagnostic",
                id,
                Some(&uri),
            );
            entry.pending_requests.insert(
                id,
                PendingRequest::PullDiagnostic { path: path.clone() },
            );
        }
    }

    /// Send `textDocument/completion` for the cursor at
    /// `(line, col)` on `path`. The runtime's `seq` is carried
    /// into the `PendingRequest` so the eventual
    /// `LspEvent::Completion` can echo it back — stale responses
    /// (seq older than the latest live request) are dropped at
    /// the ingest end. Silently no-ops when no server is attached
    /// to the path's language or the server doesn't advertise
    /// `completionProvider`.
    pub(super) fn request_completion(
        &mut self,
        path: CanonPath,
        seq: LspRequestSeq,
        line: u32,
        col: u32,
        trigger: Option<char>,
    ) {
        let Some(language) = self.language_for_path(&path) else {
            return;
        };
        let id = self.fresh_id();
        let entry = self.servers.get_mut(&language).expect("just resolved");
        if !entry.completion_provider {
            return;
        }
        // triggerCharacter is only set when the char was in the
        // server-advertised list; otherwise report Invoked (2).
        // Matches legacy `spawn_completion` exactly — legacy
        // always sends Invoked with `trigger_character: None`,
        // but we honour the char when we know the server asked
        // for it so smart servers can tune the candidate set.
        let (trigger_kind, trigger_char_json) = match trigger {
            Some(c) if entry.completion_trigger_chars.contains(&c) => {
                (2u8 /* TriggerCharacter */, json!(c.to_string()))
            }
            _ => (1u8 /* Invoked */, Value::Null),
        };
        let uri = uri_from_path(&path);
        let server_name = short_server_id(&entry.server.name);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/completion",
            "params": {
                "textDocument": { "uri": uri.clone() },
                "position": { "line": line, "character": col },
                "context": {
                    "triggerKind": trigger_kind,
                    "triggerCharacter": trigger_char_json,
                },
            },
        });
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize completion"));
        self.trace.lsp_send_request(
            &server_name,
            "textDocument/completion",
            id,
            Some(&uri),
        );
        entry
            .pending_requests
            .insert(id, PendingRequest::Completion { path, seq, line });
    }

    /// Send `completionItem/resolve` for the item the user just
    /// committed. The opaque `data` field on the original
    /// `CompletionItem` (stored as `resolve_data`) is echoed
    /// back so the server can look up whatever index it was
    /// carrying. Returns the server's `additionalTextEdits` via
    /// `LspEvent::CompletionResolved`.
    pub(super) fn resolve_completion(
        &mut self,
        path: CanonPath,
        seq: LspRequestSeq,
        item: led_driver_lsp_core::CompletionItem,
    ) {
        let Some(language) = self.language_for_path(&path) else {
            return;
        };
        let id = self.fresh_id();
        let entry = self.servers.get_mut(&language).expect("just resolved");
        if !entry.completion_resolve_provider {
            return;
        }
        let mut payload = json!({
            "label": item.label.as_ref(),
        });
        if let Some(data) = item.resolve_data.as_ref() {
            // `data` is an opaque blob; we stored it as a JSON
            // string in `resolve_data`. Round-tripping through
            // serde_json::from_str restores the original shape
            // so the server sees what it sent us.
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                payload["data"] = v;
            }
        }
        if let Some(detail) = item.detail.as_ref() {
            payload["detail"] = json!(detail.as_ref());
        }
        let server_name = short_server_id(&entry.server.name);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "completionItem/resolve",
            "params": payload,
        });
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize resolve"));
        self.trace
            .lsp_send_request(&server_name, "completionItem/resolve", id, None);
        entry
            .pending_requests
            .insert(id, PendingRequest::ResolveCompletion { path, seq });
    }

    // ── M18 stubs ─────────────────────────────────────────
    //
    // Each handler below lands as a fully-wired RPC in its own
    // stage (2..=6). For now they're no-ops so the runtime can
    // call the new `LspCmd` variants without the manager
    // panicking or falling through.

    pub(super) fn request_goto_definition(
        &mut self,
        path: CanonPath,
        seq: LspRequestSeq,
        line: u32,
        col: u32,
    ) {
        let Some(language) = self.language_for_path(&path) else {
            self.emit_goto_none(seq);
            return;
        };
        let id = self.fresh_id();
        let entry = self.servers.get_mut(&language).expect("just resolved");
        let uri = uri_from_path(&path);
        let server_name = short_server_id(&entry.server.name);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/definition",
            "params": {
                "textDocument": { "uri": uri.clone() },
                "position": { "line": line, "character": col },
            },
        });
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize goto-def"));
        self.trace.lsp_send_request(
            &server_name,
            "textDocument/definition",
            id,
            Some(&uri),
        );
        entry
            .pending_requests
            .insert(id, PendingRequest::GotoDefinition { seq });
    }

    fn emit_goto_none(&self, seq: LspRequestSeq) {
        let _ = self.lsp_event_tx.send(LspEvent::GotoDefinition {
            seq,
            location: None,
        });
        self.notify.notify();
    }

    pub(super) fn finish_goto_definition(
        &mut self,
        seq: LspRequestSeq,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let location = payload.ok().and_then(parse_definition_location);
        let _ = self
            .lsp_event_tx
            .send(LspEvent::GotoDefinition { seq, location });
        self.notify.notify();
    }

    pub(super) fn request_rename(
        &mut self,
        path: CanonPath,
        seq: LspRequestSeq,
        line: u32,
        col: u32,
        new_name: Arc<str>,
    ) {
        let Some(language) = self.language_for_path(&path) else {
            self.emit_empty_edits(seq, led_driver_lsp_core::EditsOrigin::Rename);
            return;
        };
        let id = self.fresh_id();
        let entry = self.servers.get_mut(&language).expect("just resolved");
        let uri = uri_from_path(&path);
        let server_name = short_server_id(&entry.server.name);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/rename",
            "params": {
                "textDocument": { "uri": uri.clone() },
                "position": { "line": line, "character": col },
                "newName": new_name.as_ref(),
            },
        });
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize rename"));
        self.trace.lsp_send_request(
            &server_name,
            "textDocument/rename",
            id,
            Some(&uri),
        );
        entry
            .pending_requests
            .insert(id, PendingRequest::Rename { seq });
    }

    fn emit_empty_edits(
        &self,
        seq: LspRequestSeq,
        origin: led_driver_lsp_core::EditsOrigin,
    ) {
        let _ = self.lsp_event_tx.send(LspEvent::Edits {
            seq,
            origin,
            edits: Arc::new(Vec::new()),
        });
        self.notify.notify();
    }

    pub(super) fn finish_rename(
        &mut self,
        seq: LspRequestSeq,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let edits = match payload {
            Ok(v) => parse_workspace_edit(&v),
            Err(_) => Vec::new(),
        };
        let _ = self.lsp_event_tx.send(LspEvent::Edits {
            seq,
            origin: led_driver_lsp_core::EditsOrigin::Rename,
            edits: Arc::new(edits),
        });
        self.notify.notify();
    }

    pub(super) fn request_code_action(
        &mut self,
        path: CanonPath,
        seq: LspRequestSeq,
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
    ) {
        let Some(language) = self.language_for_path(&path) else {
            self.emit_empty_code_actions(path, seq);
            return;
        };
        let id = self.fresh_id();
        let entry = self.servers.get_mut(&language).expect("just resolved");
        // Fresh request purges any stale cache from the
        // previous session. A picker always pairs 1:1 with a
        // most-recent request.
        entry.code_action_cache.clear();
        let uri = uri_from_path(&path);
        let server_name = short_server_id(&entry.server.name);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": uri.clone() },
                "range": {
                    "start": { "line": start_line, "character": start_col },
                    "end":   { "line": end_line,   "character": end_col   },
                },
                "context": { "diagnostics": [] },
            },
        });
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize codeAction"));
        self.trace.lsp_send_request(
            &server_name,
            "textDocument/codeAction",
            id,
            Some(&uri),
        );
        entry
            .pending_requests
            .insert(id, PendingRequest::CodeAction { seq, path });
    }

    fn emit_empty_code_actions(&self, path: CanonPath, seq: LspRequestSeq) {
        let _ = self.lsp_event_tx.send(LspEvent::CodeActions {
            path,
            seq,
            actions: Arc::new(Vec::new()),
        });
        self.notify.notify();
    }

    pub(super) fn finish_code_action(
        &mut self,
        language: Language,
        path: CanonPath,
        seq: LspRequestSeq,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let raw_items = match payload {
            Ok(Value::Array(arr)) => arr,
            _ => Vec::new(),
        };
        let entry = self.servers.get_mut(&language).unwrap();
        entry.code_action_cache.clear();
        let mut summaries: Vec<led_driver_lsp_core::CodeActionSummary> =
            Vec::with_capacity(raw_items.len());
        for (idx, raw) in raw_items.into_iter().enumerate() {
            let Some(title) = raw
                .get("title")
                .and_then(|t| t.as_str())
                .map(Arc::<str>::from)
            else {
                continue;
            };
            let kind = raw
                .get("kind")
                .and_then(|k| k.as_str())
                .map(Arc::<str>::from);
            // Pure Command variants have no `edit`; CodeAction
            // objects with an `edit` present skip resolve.
            let has_edit = raw.get("edit").is_some();
            let resolve_needed = !has_edit;
            let action_id: Arc<str> = Arc::<str>::from(format!("ca-{idx}"));
            entry
                .code_action_cache
                .insert(action_id.clone(), raw);
            summaries.push(led_driver_lsp_core::CodeActionSummary {
                title,
                kind,
                resolve_needed,
                action_id,
            });
        }
        let _ = self.lsp_event_tx.send(LspEvent::CodeActions {
            path,
            seq,
            actions: Arc::new(summaries),
        });
        self.notify.notify();
    }

    pub(super) fn select_code_action(
        &mut self,
        path: CanonPath,
        seq: LspRequestSeq,
        action: led_driver_lsp_core::CodeActionSummary,
    ) {
        let Some(language) = self.language_for_path(&path) else {
            self.emit_empty_edits(seq, led_driver_lsp_core::EditsOrigin::CodeAction);
            return;
        };
        let raw = match self
            .servers
            .get(&language)
            .and_then(|e| e.code_action_cache.get(&action.action_id).cloned())
        {
            Some(raw) => raw,
            None => {
                // Cache was purged between request and commit
                // (another Alt-i fired). Legacy parity: drop.
                self.emit_empty_edits(seq, led_driver_lsp_core::EditsOrigin::CodeAction);
                return;
            }
        };
        if !action.resolve_needed && raw.get("edit").is_some() {
            // Edits are already in hand — parse + emit directly.
            let edits = raw
                .get("edit")
                .map(parse_workspace_edit)
                .unwrap_or_default();
            let _ = self.lsp_event_tx.send(LspEvent::Edits {
                seq,
                origin: led_driver_lsp_core::EditsOrigin::CodeAction,
                edits: Arc::new(edits),
            });
            self.notify.notify();
            return;
        }
        // Otherwise issue `codeAction/resolve` for the full
        // item. rust-analyzer and typescript both lazy-resolve
        // so this is the common case.
        let id = self.fresh_id();
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "codeAction/resolve",
            "params": raw.clone(),
        });
        let entry = self.servers.get_mut(&language).expect("server exists");
        let server_name = short_server_id(&entry.server.name);
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize resolve"));
        self.trace
            .lsp_send_request(&server_name, "codeAction/resolve", id, None);
        entry
            .pending_requests
            .insert(id, PendingRequest::ResolveCodeAction { seq, raw });
    }

    pub(super) fn finish_resolve_code_action(
        &mut self,
        seq: LspRequestSeq,
        raw: Value,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let resolved = payload.ok().unwrap_or(raw);
        let edits = resolved
            .get("edit")
            .map(parse_workspace_edit)
            .unwrap_or_default();
        let _ = self.lsp_event_tx.send(LspEvent::Edits {
            seq,
            origin: led_driver_lsp_core::EditsOrigin::CodeAction,
            edits: Arc::new(edits),
        });
        self.notify.notify();
    }

    pub(super) fn request_format(&mut self, path: CanonPath, seq: LspRequestSeq) {
        let Some(language) = self.language_for_path(&path) else {
            // No LSP for this language — emit empty edits so
            // the runtime's post-format save unlocks.
            self.emit_empty_edits(seq, led_driver_lsp_core::EditsOrigin::Format);
            return;
        };
        let id = self.fresh_id();
        let entry = self.servers.get_mut(&language).expect("just resolved");
        let uri = uri_from_path(&path);
        let server_name = short_server_id(&entry.server.name);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/formatting",
            "params": {
                "textDocument": { "uri": uri.clone() },
                "options": {
                    "tabSize": 4,
                    "insertSpaces": true,
                },
            },
        });
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize formatting"));
        self.trace.lsp_send_request(
            &server_name,
            "textDocument/formatting",
            id,
            Some(&uri),
        );
        entry
            .pending_requests
            .insert(id, PendingRequest::Format { seq, path });
    }

    pub(super) fn finish_format(
        &mut self,
        seq: LspRequestSeq,
        path: CanonPath,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let edits_vec = match payload {
            Ok(Value::Array(arr)) => parse_text_edit_list(&arr),
            _ => Vec::new(),
        };
        let file_edits = if edits_vec.is_empty() {
            Vec::new()
        } else {
            vec![led_driver_lsp_core::FileEdit {
                path,
                edits: edits_vec,
            }]
        };
        let _ = self.lsp_event_tx.send(LspEvent::Edits {
            seq,
            origin: led_driver_lsp_core::EditsOrigin::Format,
            edits: Arc::new(file_edits),
        });
        self.notify.notify();
    }

    pub(super) fn request_inlay_hints(
        &mut self,
        path: CanonPath,
        seq: LspRequestSeq,
        version: BufferVersion,
        start_line: u32,
        end_line: u32,
    ) {
        let _ = seq; // seq is internal-only (tracing); manager re-echoes version.
        let Some(language) = self.language_for_path(&path) else {
            self.emit_empty_inlay_hints(path, version);
            return;
        };
        let id = self.fresh_id();
        let entry = self.servers.get_mut(&language).expect("just resolved");
        let uri = uri_from_path(&path);
        let server_name = short_server_id(&entry.server.name);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/inlayHint",
            "params": {
                "textDocument": { "uri": uri.clone() },
                "range": {
                    "start": { "line": start_line, "character": 0 },
                    "end":   { "line": end_line,   "character": 0 },
                },
            },
        });
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize inlayHint"));
        self.trace.lsp_send_request(
            &server_name,
            "textDocument/inlayHint",
            id,
            Some(&uri),
        );
        entry
            .pending_requests
            .insert(id, PendingRequest::InlayHints { path, version });
    }

    fn emit_empty_inlay_hints(&self, path: CanonPath, version: BufferVersion) {
        let _ = self.lsp_event_tx.send(LspEvent::InlayHints {
            path,
            version,
            hints: Arc::new(Vec::new()),
        });
        self.notify.notify();
    }

    pub(super) fn finish_inlay_hints(
        &mut self,
        path: CanonPath,
        version: BufferVersion,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let hints = match payload {
            Ok(Value::Array(arr)) => parse_inlay_hints(&arr),
            _ => Vec::new(),
        };
        let _ = self.lsp_event_tx.send(LspEvent::InlayHints {
            path,
            version,
            hints: Arc::new(hints),
        });
        self.notify.notify();
    }

    pub(super) fn finish_completion(
        &mut self,
        path: CanonPath,
        seq: LspRequestSeq,
        line: u32,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let result = match payload {
            Ok(v) => v,
            Err(_) => return, // server errored; drop silently.
        };
        let parsed = parse_completion_response(&result, line);
        let _ = self.lsp_event_tx.send(LspEvent::Completion {
            path,
            seq,
            items: Arc::new(parsed.items),
            prefix_line: line,
            prefix_start_col: parsed.prefix_start_col,
        });
        self.notify.notify();
    }

    pub(super) fn finish_resolve_completion(
        &mut self,
        path: CanonPath,
        seq: LspRequestSeq,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let result = match payload {
            Ok(v) => v,
            Err(_) => return,
        };
        let edits = parse_resolve_additional_edits(&result);
        let _ = self.lsp_event_tx.send(LspEvent::CompletionResolved {
            path,
            seq,
            additional_edits: edits,
        });
        self.notify.notify();
    }

    pub(super) fn finish_pull_diagnostic(
        &mut self,
        language: Language,
        path: CanonPath,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let diags = match payload {
            Ok(result) => parse_diagnostic_result(&result),
            Err(_) => Vec::new(),
        };
        let entry = self.servers.get_mut(&language).unwrap();
        let (forward, _all_done) = entry.diag.on_pull_response(path, diags);
        if let Some((path, diagnostics, hash)) = forward {
            self.trace
                .lsp_diagnostics_done(&path, diagnostics.len(), hash);
            let _ = self.lsp_event_tx.send(LspEvent::Diagnostics {
                path,
                hash,
                diagnostics,
            });
            self.notify.notify();
        }
    }
}
