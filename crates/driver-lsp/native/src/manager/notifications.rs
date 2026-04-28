use std::sync::Arc;

use led_core::{CanonPath, PersistedContentHash};
use led_driver_lsp_core::Trace;
use led_state_syntax::Language;
use ropey::Rope;
use serde_json::{Value, json};

use crate::protocol::{language_id, uri_from_path};

use super::{Manager, PendingOpen, ServerEntry, short_server_id};

/// Threshold: if the incremental `didChange` replacement would be
/// larger than this many chars, fall back to full-text. Protects
/// against pathological deltas (rebase, format-all) where the
/// incremental form is actually larger / slower than a fresh
/// sync.
pub(super) const DIDCHANGE_INCREMENTAL_MAX_CHARS: usize = 4096;

impl Manager {
    pub(super) fn open_buffer(
        &mut self,
        language: Language,
        path: CanonPath,
        rope: Arc<Rope>,
        hash: PersistedContentHash,
    ) {
        let Some(entry) = self.servers.get_mut(&language) else {
            return;
        };
        entry.buffer_hashes.insert(path.clone(), hash);
        if entry.initialized {
            send_did_open(entry, &path, &rope, self.trace.as_ref());
        } else {
            entry.queued_opens.push(PendingOpen {
                path,
                rope,
                hash,
            });
        }
    }

    pub(super) fn buffer_changed(
        &mut self,
        path: &CanonPath,
        rope: &Arc<Rope>,
        hash: PersistedContentHash,
        is_save: bool,
    ) {
        // Find the server that has this path open.
        let language = self.servers.iter().find_map(|(l, e)| {
            e.doc_versions.contains_key(path).then_some(*l)
        });
        let Some(language) = language else { return };
        let entry = self.servers.get_mut(&language).expect("just found");

        // Freeze discipline: the rope moved to new content, so
        // any open window that snapshotted this path's hash is
        // now stale. Close it so a later RequestDiagnostics opens
        // a fresh window at the current hash.
        if entry.diag.should_close_window(path, hash) {
            entry.diag.close_window();
        }

        entry.buffer_hashes.insert(path.clone(), hash);
        // Legacy push_cache invalidation: the cached push is
        // pinned to an earlier content; next push (if any) will
        // supersede, but in the meantime we don't want to forward
        // stale data on the next window.
        entry.diag.invalidate_cache(path);

        let lsp_version = {
            let v = entry.doc_versions.entry(path.clone()).or_insert(0);
            *v += 1;
            *v
        };
        // Incremental if we have a previous rope for this path
        // AND the delta is small enough to justify it. Otherwise
        // full-text. Matches legacy's "single-edit Range-based,
        // else full-text" rule by outcome (ops.len() == 1 implies
        // a single contiguous LCP/LCS-trim delta).
        let incremental_change = entry
            .last_rope_sent
            .get(path)
            .and_then(|old| incremental_content_change(old, rope));
        let content_changes = match incremental_change {
            Some(change) => json!([change]),
            None => json!([{ "text": rope.to_string() }]),
        };
        let uri = uri_from_path(path);
        let server_name = short_server_id(&entry.server.name);
        let body = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": {
                    "uri": uri.clone(),
                    "version": lsp_version,
                },
                "contentChanges": content_changes,
            },
        });
        let _ = entry.server.send_body(&serde_json::to_vec(&body).unwrap());
        self.trace.lsp_send_notification(
            &server_name,
            "textDocument/didChange",
            Some(&uri),
            Some(lsp_version),
        );
        entry.last_rope_sent.insert(path.clone(), rope.clone());

        if is_save {
            let save_body = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didSave",
                "params": {
                    "textDocument": { "uri": uri.clone() },
                },
            });
            let _ = entry
                .server
                .send_body(&serde_json::to_vec(&save_body).unwrap());
            self.trace.lsp_send_notification(
                &server_name,
                "textDocument/didSave",
                Some(&uri),
                None,
            );
        }
    }

    pub(super) fn buffer_closed(&mut self, path: &CanonPath) {
        let language = self.servers.iter().find_map(|(l, e)| {
            e.doc_versions.contains_key(path).then_some(*l)
        });
        let Some(language) = language else { return };
        let entry = self.servers.get_mut(&language).expect("just found");

        let uri = uri_from_path(path);
        let server_name = short_server_id(&entry.server.name);
        let body = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": {
                "textDocument": { "uri": uri.clone() },
            },
        });
        let _ = entry.server.send_body(&serde_json::to_vec(&body).unwrap());
        self.trace.lsp_send_notification(
            &server_name,
            "textDocument/didClose",
            Some(&uri),
            None,
        );
        entry.doc_versions.remove(path);
        entry.buffer_hashes.remove(path);
        entry.last_rope_sent.remove(path);
        entry.diag.invalidate_cache(path);
    }
}

pub(super) fn send_did_open(
    entry: &mut ServerEntry,
    path: &CanonPath,
    rope: &Arc<Rope>,
    trace: &dyn Trace,
) {
    let uri = uri_from_path(path);
    let body = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": uri.clone(),
                "languageId": language_id(entry.language),
                "version": 1,
                "text": rope.to_string(),
            },
        },
    });
    entry.doc_versions.insert(path.clone(), 1);
    // Seed the incremental-didChange cache so the FIRST
    // didChange can go incremental instead of full-text.
    entry.last_rope_sent.insert(path.clone(), rope.clone());
    let _ = entry.server.send_body(&serde_json::to_vec(&body).unwrap());
    trace.lsp_send_notification(
        &short_server_id(&entry.server.name),
        "textDocument/didOpen",
        Some(&uri),
        Some(1),
    );
}

/// Compute an LSP Range-based `contentChange` entry from the
/// char-delta between `old` and `new`. Returns `None` if the
/// delta is too large (>`DIDCHANGE_INCREMENTAL_MAX_CHARS` chars
/// of replacement text) — caller falls back to full-text. Also
/// returns `None` when the ropes are identical; callers treat
/// that as "nothing to send."
///
/// Positions are emitted as UTF-16 code units per LSP's default
/// encoding (`general.positionEncodings` unspecified by us →
/// server assumes UTF-16).
pub(super) fn incremental_content_change(old: &Rope, new: &Rope) -> Option<Value> {
    let (prefix, old_end, new_end) = char_delta_bounds(old, new)?;
    let replacement_len = new_end.saturating_sub(prefix);
    if replacement_len > DIDCHANGE_INCREMENTAL_MAX_CHARS {
        return None;
    }
    let (start_line, start_utf16) = char_idx_to_line_utf16(old, prefix);
    let (end_line, end_utf16) = char_idx_to_line_utf16(old, old_end);
    let new_text: String = new.slice(prefix..new_end).to_string();
    Some(json!({
        "range": {
            "start": { "line": start_line, "character": start_utf16 },
            "end":   { "line": end_line,   "character": end_utf16   },
        },
        "text": new_text,
    }))
}

/// Longest-common-prefix / longest-common-suffix trim.
/// Returns `(prefix_char_idx, old_end_char_idx, new_end_char_idx)`
/// — the inclusive-start / exclusive-end range of chars that
/// actually differ between the two ropes. `None` when the ropes
/// are byte-for-byte identical.
pub(super) fn char_delta_bounds(old: &Rope, new: &Rope) -> Option<(usize, usize, usize)> {
    let old_len = old.len_chars();
    let new_len = new.len_chars();
    if old_len == new_len {
        // Cheap escape: identical ropes → no delta.
        let old_cmp = old.slice(..).bytes().eq(new.slice(..).bytes());
        if old_cmp {
            return None;
        }
    }
    let min_len = old_len.min(new_len);

    // Common prefix via paired char iteration.
    let mut prefix = 0usize;
    let mut o_it = old.chars();
    let mut n_it = new.chars();
    while prefix < min_len {
        match (o_it.next(), n_it.next()) {
            (Some(o), Some(n)) if o == n => prefix += 1,
            _ => break,
        }
    }

    // Common suffix (indexed from the end, stopping at `prefix`).
    let max_suffix = min_len - prefix;
    let mut suffix = 0usize;
    while suffix < max_suffix {
        let o_idx = old_len - 1 - suffix;
        let n_idx = new_len - 1 - suffix;
        if old.char(o_idx) != new.char(n_idx) {
            break;
        }
        suffix += 1;
    }
    Some((prefix, old_len - suffix, new_len - suffix))
}

/// Convert a char index into `(line, utf16_col)` — UTF-16 code
/// units relative to the start of the containing line. LSP
/// `Position` uses this encoding by default.
pub(super) fn char_idx_to_line_utf16(rope: &Rope, char_idx: usize) -> (usize, usize) {
    let line = rope.char_to_line(char_idx);
    let line_start = rope.line_to_char(line);
    let utf16_at_line_start = rope.char_to_utf16_cu(line_start);
    let utf16_at_char = rope.char_to_utf16_cu(char_idx);
    (line, utf16_at_char - utf16_at_line_start)
}
