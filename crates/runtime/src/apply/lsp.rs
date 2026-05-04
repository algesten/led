//! LSP-apply helpers extracted from `lib.rs`.
//!
//! Verbatim moves of: `identifier_start_col`, `completion_prefix`,
//! `LspGotoApply` + `current_jump_position`, `LspEditApply` +
//! `apply_file_edits` + `apply_one_text_edit`. Visibility bumped to
//! `pub(crate)` so the main loop can keep calling them.

use led_core::{CanonPath, PathChain};
use led_state_alerts::AlertState;
use led_state_buffer_edits::{BufferEdits, EditedBuffer};
use led_state_jumps::JumpListState;
use led_state_tabs::Tabs;

use crate::dispatch;
use crate::INFO_TTL;

/// Extract the typed prefix the user's cursor is parked at for an
/// incoming `LspEvent::Completion`. Used by ingest to refilter the
/// server response against the current buffer state before
/// installing the session — without this, items appear
/// unfiltered for one frame until the next keystroke.
/// Walk left from `cursor_col` on `prefix_line` while characters
/// are identifier-like (alphanumeric or `_`). The returned col is
/// the first identifier char — `cursor_col` itself when the char
/// to the left isn't identifier-like, `0` when the line begins
/// with an unbroken run. Used as the fallback for completion
/// responses where the server didn't carry a `textEdit.range`
/// (legacy `convert_completion_response`).
/// Walk back through identifier characters from the cursor, in
/// grapheme units, to find the start col of the typed prefix. Used
/// when the LSP server returns a completion item without a
/// `textEdit.range` — we backtrack on the buffer ourselves.
///
/// `cursor_col` and the returned value are both grapheme cols on
/// `prefix_line`. Combining marks attached to a word base inherit
/// the word classification (the base scalar is what gets checked).
pub(crate) fn identifier_start_col(
    edits: &BufferEdits,
    path: &CanonPath,
    prefix_line: usize,
    cursor_col: usize,
) -> u32 {
    let Some(eb) = edits.buffers.get(path) else {
        return cursor_col as u32;
    };
    if prefix_line >= eb.rope.len_lines() {
        return cursor_col as u32;
    }
    let line_slice = eb.rope.line(prefix_line);
    let line_grapheme_count = led_core::line_grapheme_len(line_slice);
    let mut start = cursor_col.min(line_grapheme_count);
    while start > 0 {
        // The cluster immediately before `start` (grapheme units).
        let prev_char_in_line = led_core::grapheme_col_to_char(line_slice, start - 1);
        let line_start_char = eb.rope.line_to_char(prefix_line);
        let ch = eb.rope.char(line_start_char + prev_char_in_line);
        if ch.is_alphanumeric() || ch == '_' {
            start -= 1;
        } else {
            break;
        }
    }
    start as u32
}

pub(crate) fn completion_prefix(
    edits: &BufferEdits,
    path: &CanonPath,
    tab: &led_state_tabs::Tab,
    prefix_line: usize,
    prefix_start_col: usize,
) -> String {
    let Some(eb) = edits.buffers.get(path) else {
        return String::new();
    };
    if prefix_line >= eb.rope.len_lines() {
        return String::new();
    }
    let line_slice = eb.rope.line(prefix_line);
    let line_start = eb.rope.line_to_char(prefix_line);
    // `prefix_start_col` and `tab.cursor.col` are both grapheme cols
    // (M25). Convert each to a char idx via the line's segmentation
    // before slicing the rope; the typed prefix may include emoji or
    // combining marks whose char widths differ from their grapheme
    // count.
    let from = line_start + led_core::grapheme_col_to_char(line_slice, prefix_start_col);
    let to = line_start + led_core::grapheme_col_to_char(line_slice, tab.cursor.col);
    if to < from || to > eb.rope.len_chars() {
        return String::new();
    }
    eb.rope.slice(from..to).to_string()
}

/// Bundle of references `LspGotoApply::apply` needs. Carved out
/// of the runtime tick / test sites so the apply method can take
/// a small `&mut self` instead of an 8-positional-arg list.
pub(crate) struct LspGotoApply<'a> {
    pub(crate) tabs: &'a mut Tabs,
    pub(crate) edits: &'a BufferEdits,
    pub(crate) jumps: &'a mut JumpListState,
    pub(crate) alerts: &'a mut AlertState,
    pub(crate) lsp_pending: &'a mut led_state_lsp::LspPending,
    pub(crate) terminal: &'a led_driver_terminal_core::Terminal,
    pub(crate) browser: &'a led_state_browser::BrowserUi,
    pub(crate) path_chains: &'a mut std::collections::HashMap<CanonPath, PathChain>,
}

/// Apply a goto-definition response: record a jump, switch to
/// the target tab (when open), move the cursor. Dropped
/// silently if the seq doesn't match the latest outstanding
/// request (user navigated elsewhere). `None` location surfaces
/// a warn alert so the user knows why the keystroke went
/// nowhere.
///
/// Opening a fresh buffer when the target is outside the
/// currently-open tabs is deferred to M21 (session / persistence
/// will stash a pending cursor the same way find-file does);
/// for M18 the jump silent-no-ops when the path isn't open.
impl<'a> LspGotoApply<'a> {
    pub(crate) fn apply(
        &mut self,
        seq: led_core::LspRequestSeq,
        location: Option<led_driver_lsp_core::Location>,
    ) {
        let tabs = &mut *self.tabs;
        let edits = self.edits;
        let jumps = &mut *self.jumps;
        let alerts = &mut *self.alerts;
        let lsp_pending = &mut *self.lsp_pending;
        let terminal = self.terminal;
        let browser = self.browser;
        let path_chains = &mut *self.path_chains;

        if lsp_pending.latest_goto_seq != Some(seq) {
            return;
        }
        lsp_pending.latest_goto_seq = None;
        let Some(loc) = location else {
            alerts.set_warn(
                "lsp.goto".to_string(),
                "No definition found".to_string(),
            );
            return;
        };
        // Capture the pre-jump position before applying the
        // target, so Alt-b returns to where the user called the
        // command from.
        let Some(current) = current_jump_position(tabs) else {
            return;
        };
        jumps.record(current);

        // Two paths now (M21):
        //   * Buffer is already loaded → land cursor + recenter
        //     scroll inline, exactly like before.
        //   * Buffer not yet loaded → open / focus a tab at the
        //     target path and stash the cursor as `pending_cursor`.
        //     The load-completion ingest applies it once the rope
        //     materialises.
        if let Some(idx) = tabs.open.iter().position(|t| t.path == loc.path)
            && let Some(eb) = edits.buffers.get(&loc.path)
        {
            let line_count = eb.rope.len_lines();
            let line = (loc.line as usize).min(line_count.saturating_sub(1));
            // `loc.col` is a UTF-16 code-unit count from the LSP
            // server; convert to grapheme col through the actual
            // line so we land on the same cluster the server picked.
            let line_slice = eb.rope.line(line);
            let col = led_core::utf16_units_to_grapheme_col(line_slice, loc.col);
            let body_rows = terminal
                .dims
                .map(|d| {
                    led_driver_terminal_core::Layout::compute(d, browser.visible)
                        .editor_area
                        .rows as usize
                })
                .unwrap_or(0);
            let content_cols = dispatch::editor_content_cols(terminal, browser);
            let tab = &mut tabs.open[idx];
            tab.cursor.line = line;
            tab.cursor.col = col;
            tab.cursor.preferred_col =
                led_core::prefix_display_width(line_slice, col);
            tab.scroll = dispatch::center_on_cursor(
                tab.scroll,
                tab.cursor,
                body_rows,
                &eb.rope,
                content_cols,
            );
            tabs.active = Some(tab.id);
            alerts.clear_warn("lsp.goto");
            return;
        }

        // Open a fresh tab at the target path with a pending
        // cursor; the load-completion hook applies it. Stash the
        // path-chain so the language detector picks up the
        // user-typed extension on load.
        let chain = led_core::UserPath::new(loc.path.as_path()).resolve_chain();
        path_chains.insert(loc.path.clone(), chain);
        dispatch::open_or_focus_tab(tabs, &loc.path, true);
        if let Some(tab) = tabs
            .open
            .iter_mut()
            .find(|t| t.path == loc.path)
        {
            tab.pending_cursor = Some(led_state_tabs::Cursor {
                line: loc.line as usize,
                col: loc.col as usize,
                preferred_col: loc.col as usize,
            });
            // Don't pre-set a scroll — let the load-completion
            // hook clear pending_scroll = None and the active tab
            // tick recenter via the scroll-adjust pass on the next
            // cursor move (or via a future "if pending_cursor and
            // pending_scroll is None, recenter on apply" path).
        }
        alerts.clear_warn("lsp.goto");
    }
}

pub(crate) fn current_jump_position(tabs: &Tabs) -> Option<led_state_jumps::JumpPosition> {
    let id = tabs.active?;
    let tab = tabs.open.iter().find(|t| t.id == id)?;
    Some(led_state_jumps::JumpPosition {
        path: tab.path.clone(),
        line: tab.cursor.line,
        col: tab.cursor.col,
        top: tab.scroll.top,
        top_sub_line: tab.scroll.top_sub_line,
    })
}

/// Bundle of references `LspEditApply::apply` needs. Carved out
/// of the runtime tick / test sites so the apply method can take
/// a small `&mut self` instead of a 7-positional-arg list.
pub(crate) struct LspEditApply<'a> {
    pub(crate) edits: &'a mut BufferEdits,
    pub(crate) tabs: &'a led_state_tabs::Tabs,
    pub(crate) alerts: &'a mut AlertState,
    pub(crate) lsp_pending: &'a mut led_state_lsp::LspPending,
}

/// Apply an `LspEvent::Edits` delivery: walk `file_edits`, apply
/// each `TextEditOp` to its target buffer (when currently open),
/// and record history entries so Undo can revert. Edits for
/// paths we don't have open are dropped silently — M18 parity
/// with legacy, which writes disk-only edits from the manager
/// side rather than through the buffer layer.
///
/// Stale seq (rename only, for now) drops the whole delivery.
/// Edits arrive ordered by the server; we reapply per-file from
/// latest range to earliest so later applies don't shift
/// earlier ones. Alerts surface "Renamed N occurrence(s) in M
/// file(s)" on success.
impl<'a> LspEditApply<'a> {
    pub(crate) fn apply(
        &mut self,
        seq: led_core::LspRequestSeq,
        origin: led_driver_lsp_core::EditsOrigin,
        file_edits: &std::sync::Arc<Vec<led_driver_lsp_core::FileEdit>>,
    ) {
        let edits = &mut *self.edits;
        let tabs = self.tabs;
        let alerts = &mut *self.alerts;
        let lsp_pending = &mut *self.lsp_pending;
    // Stale-seq gate per origin.
    match origin {
        led_driver_lsp_core::EditsOrigin::Rename => {
            if lsp_pending.latest_rename_seq != Some(seq) {
                return;
            }
            lsp_pending.latest_rename_seq = None;
        }
        led_driver_lsp_core::EditsOrigin::CodeAction => {
            if lsp_pending.latest_code_action_select_seq != Some(seq) {
                return;
            }
            lsp_pending.latest_code_action_select_seq = None;
        }
        led_driver_lsp_core::EditsOrigin::Format => {
            // Per-path stale gate: the most-recently-queued
            // format for each path is the only reply whose
            // edits the runtime accepts. Older replies (e.g.
            // from a pre-reformat keystroke's follow-up)
            // drop silently.
            let mut keep = false;
            for fe in file_edits.iter() {
                if lsp_pending.latest_format_seq.get(&fe.path) == Some(&seq) {
                    lsp_pending.latest_format_seq.remove(&fe.path);
                    keep = true;
                }
            }
            if !keep && file_edits.is_empty() {
                // Empty-edit formats still need to release the
                // save gate. Walk every `pending_save_after_format`
                // path and if ANY has its latest_format_seq
                // matching, accept this delivery as that path's
                // completion.
                let matching: Vec<CanonPath> = lsp_pending
                    .pending_save_after_format
                    .iter()
                    .filter(|p| lsp_pending.latest_format_seq.get(*p) == Some(&seq))
                    .cloned()
                    .collect();
                for p in &matching {
                    lsp_pending.latest_format_seq.remove(p);
                }
                if matching.is_empty() {
                    return;
                }
                // Post-format save trigger below still handles
                // matching.
            } else if !keep {
                return;
            }
        }
    }

    let mut total_ops = 0usize;
    let mut files_touched = 0usize;
    for fe in file_edits.iter() {
        // Capture the tab's cursor for this file (if any) before
        // the edit runs, so the group's undo/redo bookends point
        // at a meaningful location rather than (0, 0). When no
        // tab is open for the path (shouldn't happen in
        // practice — we only get edits for paths we asked about)
        // we fall back to Default.
        let cursor = tabs
            .open
            .iter()
            .find(|t| t.path == fe.path)
            .map(|t| t.cursor)
            .unwrap_or_default();
        let Some(eb) = edits.buffers.get_mut(&fe.path) else {
            continue;
        };
        if fe.edits.is_empty() {
            continue;
        }
        let applied = apply_file_edits(eb, &fe.edits, cursor);
        if applied > 0 {
            total_ops += applied;
            files_touched += 1;
        }
    }

    if total_ops > 0
        && !matches!(origin, led_driver_lsp_core::EditsOrigin::Format)
    {
        let msg = match origin {
            led_driver_lsp_core::EditsOrigin::Rename => {
                if files_touched == 1 {
                    format!(
                        "Renamed {total_ops} occurrence{} in 1 file",
                        if total_ops == 1 { "" } else { "s" },
                    )
                } else {
                    format!(
                        "Renamed {total_ops} occurrences in {files_touched} files"
                    )
                }
            }
            led_driver_lsp_core::EditsOrigin::CodeAction => {
                format!("Applied code action ({total_ops} edit{})",
                    if total_ops == 1 { "" } else { "s" })
            }
            led_driver_lsp_core::EditsOrigin::Format => unreachable!(),
        };
        alerts.set_info(msg, std::time::Instant::now(), INFO_TTL);
    }

    // Post-format save trigger: paths awaiting save after
    // format now slot into `pending_saves`. Covers the
    // format-arrived-empty case (no file_edits, nothing
    // touched) as well as the format-with-edits case (edits
    // applied above, now save).
    if matches!(origin, led_driver_lsp_core::EditsOrigin::Format) {
        // Collect paths associated with this format delivery:
        // either referenced in `file_edits`, or in
        // `pending_save_after_format` (fallback for empty
        // deliveries where `file_edits` is empty).
        let mut to_save: Vec<CanonPath> = file_edits
            .iter()
            .map(|fe| fe.path.clone())
            .collect();
        if to_save.is_empty() {
            to_save = lsp_pending
                .pending_save_after_format
                .iter()
                .cloned()
                .collect();
        }
        for path in to_save {
            if lsp_pending.pending_save_after_format.remove(&path).is_none() {
                continue;
            }
            // Always save, even if the buffer looks clean: the
            // user asked for Save, the format round-trip is
            // complete, and writing a byte-identical file is
            // cheap. Gating on `eb.dirty()` here would drop the
            // save whenever format returned no edits on a clean
            // buffer, contradicting "save should always save".
            //
            // Pre-save cleanup runs after the format edits land
            // so trailing whitespace the formatter didn't touch
            // (and the missing final newline) get fixed up in
            // the same save. Recorded as one undo group so a
            // post-save Ctrl-/ reverses both format and cleanup
            // together.
            if let Some(eb) = edits.buffers.get_mut(&path) {
                let cursor = tabs
                    .open
                    .iter()
                    .find(|t| t.path == path)
                    .map(|t| t.cursor)
                    .unwrap_or_default();
                crate::dispatch::save::apply_save_cleanup(eb, cursor);
                edits.pending_saves.insert(path);
            }
        }
    }
    }
}

/// Apply a batch of per-file `TextEditOp`s to a single buffer
/// and record them as a **single** undo group so one Ctrl-/
/// reverses the whole batch atomically.
///
/// Per-op groups (the previous approach) break whenever the
/// server returns overlapping-by-effect edits — e.g. sort-imports
/// is `(delete "foo, " at X, insert "foo, " at Y)`. Undoing them
/// one at a time leaves a duplicate-text intermediate state, and
/// the second undo then uses stale positions. Coalescing into
/// one group keeps the intermediate state unobservable and keeps
/// every op's recorded `at` valid relative to the rope at the
/// moment of inversion.
///
/// Edits apply bottom-first (descending start position) so each
/// apply's char indices stay valid for the next one. `cursor`
/// is the active-tab cursor captured pre-apply; it doubles as
/// `cursor_before` and `cursor_after` so undo/redo don't
/// teleport the user to (0, 0). Returns the number of ops
/// actually applied (skips any whose range is out of bounds).
pub(crate) fn apply_file_edits(
    eb: &mut EditedBuffer,
    ops: &[led_driver_lsp_core::TextEditOp],
    cursor: led_state_tabs::Cursor,
) -> usize {
    // Sort descending by (start_line, start_col) so later edits
    // don't invalidate earlier ones' indices.
    let mut sorted: Vec<&led_driver_lsp_core::TextEditOp> = ops.iter().collect();
    sorted.sort_by(|a, b| {
        (b.start_line, b.start_col)
            .cmp(&(a.start_line, a.start_col))
    });
    let mut replaces: Vec<(
        usize,
        std::sync::Arc<str>,
        std::sync::Arc<str>,
    )> = Vec::with_capacity(sorted.len());
    for op in sorted {
        if let Some((at, removed, inserted)) = apply_one_text_edit(eb, op) {
            replaces.push((at, removed, inserted));
        }
    }
    let applied = replaces.len();
    if applied > 0 {
        eb.history
            .record_replace_batch(replaces, cursor, cursor);
    }
    applied
}

/// Apply a single `TextEditOp` to the rope + bump version, and
/// return the `(at, removed, inserted)` triple the caller needs
/// to record in history. Returns `None` when the op's range is
/// out of bounds; the caller skips those.
pub(crate) fn apply_one_text_edit(
    eb: &mut EditedBuffer,
    op: &led_driver_lsp_core::TextEditOp,
) -> Option<(usize, std::sync::Arc<str>, std::sync::Arc<str>)> {
    let rope = &eb.rope;
    let line_count = rope.len_lines();
    if (op.start_line as usize) >= line_count {
        return None;
    }
    let start_line = op.start_line as usize;
    let end_line = (op.end_line as usize).min(line_count.saturating_sub(1));
    let start_line_char = rope.line_to_char(start_line);
    let end_line_char = rope.line_to_char(end_line);
    let start_line_len = if start_line + 1 < line_count {
        rope.line_to_char(start_line + 1) - start_line_char
    } else {
        rope.len_chars() - start_line_char
    };
    let end_line_len = if end_line + 1 < line_count {
        rope.line_to_char(end_line + 1) - end_line_char
    } else {
        rope.len_chars() - end_line_char
    };
    let start_char = start_line_char + (op.start_col as usize).min(start_line_len);
    let end_char = end_line_char + (op.end_col as usize).min(end_line_len);
    if end_char < start_char {
        return None;
    }

    let mut new_rope = (*eb.rope).clone();
    let removed: String = new_rope.slice(start_char..end_char).to_string();
    new_rope.remove(start_char..end_char);
    new_rope.insert(start_char, &op.new_text);

    eb.rope = std::sync::Arc::new(new_rope);
    eb.version.0 = eb.version.0.saturating_add(1);
    Some((
        start_char,
        std::sync::Arc::<str>::from(removed),
        std::sync::Arc::<str>::from(op.new_text.as_ref()),
    ))
}
