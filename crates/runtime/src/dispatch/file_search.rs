//! Project-wide file-search overlay dispatch (M14).
//!
//! Surface:
//! - Activation: `Ctrl+F` opens the overlay, snapshots the currently
//!   active tab so deactivate can restore it, and pushes focus onto
//!   the side panel.
//! - Typing / toggles: InsertChar / DeleteBack / DeleteForward /
//!   cursor moves in the query + replace inputs, plus `Alt+1/2/3`
//!   for case / regex / replace-mode. Each edit / toggle queues a
//!   fresh `FileSearch` request.
//! - Navigation: `Up` / `Down` cycle through the rows
//!   (`SearchInput` → `ReplaceInput` when active → `Result(0..n)`).
//!   Each move onto a hit row previews the hit's file.
//! - Enter: on a search input row, jump to the first hit; on a
//!   result row, re-preview that hit. The overlay stays open.
//! - Abort / CloseFileSearch: closes any preview tab the overlay
//!   created and restores the previously-active tab.
//!
//! Replace (`Alt+Enter`) lands in stage 7.

use led_state_browser::{BrowserUi, Focus};
use led_state_buffer_edits::BufferEdits;
use led_state_file_search::{FileSearchHit, FileSearchSelection, FileSearchState};
use led_state_tabs::{Cursor, Tabs};

use crate::keymap::Command;

use super::DispatchOutcome;
use super::shared::open_or_focus_tab;

pub(super) fn activate(
    file_search: &mut Option<FileSearchState>,
    browser: &mut BrowserUi,
    tabs: &Tabs,
    edits: &BufferEdits,
) {
    if file_search.is_some() {
        // Already open — Ctrl+F a second time is a no-op.
        return;
    }
    let mut state = FileSearchState::default();
    state.previous_tab = tabs.active;
    // Peek the shared seq counter WITHOUT bumping it. The floor
    // for overlay-scoped undo is "every group with seq > this"
    // — which naturally excludes all pre-overlay edits since
    // those got lower (or equal) seqs.
    state.overlay_open_seq = edits
        .seq_gen
        .0
        .load(std::sync::atomic::Ordering::Relaxed);
    *file_search = Some(state);
    // Overlay lives in the side panel slot; focus moves there so
    // keystrokes route through the overlay.
    browser.visible = true;
    browser.focus = Focus::Side;
}

pub(super) fn deactivate(
    file_search: &mut Option<FileSearchState>,
    browser: &mut BrowserUi,
    tabs: &mut Tabs,
) {
    if let Some(state) = file_search.as_ref() {
        close_preview(tabs, state.previous_tab);
    }
    *file_search = None;
    // Return focus to the main editor pane.
    browser.focus = Focus::Main;
}

/// Remove any preview tab left behind by the overlay. Restores the
/// captured `previous_tab` when it still exists; otherwise falls back
/// to the first remaining tab (or clears when nothing is left).
/// Mirrors find-file's `close_preview` so both overlays behave the
/// same way on Abort.
fn close_preview(tabs: &mut Tabs, restore_to: Option<led_state_tabs::TabId>) {
    let Some(idx) = tabs.open.iter().position(|t| t.preview) else {
        // No preview to clean up — still make sure the saved
        // previous_tab gets refocused (e.g., user previewed by
        // arrowing onto a hit whose file was already open).
        if let Some(prev) = restore_to
            && tabs.open.iter().any(|t| t.id == prev)
        {
            tabs.active = Some(prev);
        }
        return;
    };
    let preview_id = tabs.open[idx].id;
    tabs.open.remove(idx);
    if let Some(prev) = restore_to
        && tabs.open.iter().any(|t| t.id == prev)
    {
        tabs.active = Some(prev);
    } else if tabs.active == Some(preview_id) {
        tabs.active = tabs.open.front().map(|t| t.id);
    }
}

/// Route a `Command` through the overlay when active.
///
/// Returns `Some(Continue)` when fully consumed, `None` to fall
/// through to the normal dispatch path (`Quit` is the only current
/// pass-through).
pub(super) fn run_overlay_command(
    cmd: Command,
    file_search: &mut Option<FileSearchState>,
    browser: &mut BrowserUi,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    terminal: &led_driver_terminal_core::Terminal,
    fs_root: Option<&led_core::CanonPath>,
) -> Option<DispatchOutcome> {
    file_search.as_ref()?;
    match cmd {
        Command::InsertChar(c) => {
            let state = file_search.as_mut()?;
            input_for_selection(state).insert_char(c);
            if targets_query(state) {
                state.queue_search();
            }
        }
        Command::DeleteBack => {
            let state = file_search.as_mut()?;
            input_for_selection(state).delete_back();
            if targets_query(state) {
                state.queue_search();
            }
        }
        Command::DeleteForward => {
            let state = file_search.as_mut()?;
            input_for_selection(state).delete_forward();
            if targets_query(state) {
                state.queue_search();
            }
        }
        Command::CursorLeft => {
            // On a Result row with replace_mode on, Left undoes the
            // most recent per-hit replace (pops the stack). On input
            // rows it's the normal cursor move.
            let state = file_search.as_mut()?;
            if matches!(state.selection, FileSearchSelection::Result(_))
                && state.replace_mode
            {
                unreplace_selected(state, tabs, edits);
            } else {
                input_for_selection(state).move_left();
            }
        }
        Command::CursorRight => {
            // On a Result row with replace_mode on, Right commits
            // the single hit under the cursor and advances. On input
            // rows it's the normal cursor move.
            let state = file_search.as_mut()?;
            if matches!(state.selection, FileSearchSelection::Result(_))
                && state.replace_mode
            {
                replace_selected(state, tabs, edits);
            } else {
                input_for_selection(state).move_right();
            }
        }
        Command::CursorLineStart => {
            input_for_selection(file_search.as_mut()?).to_line_start();
        }
        Command::CursorLineEnd => {
            input_for_selection(file_search.as_mut()?).to_line_end();
        }
        Command::KillLine => {
            let state = file_search.as_mut()?;
            input_for_selection(state).kill_to_end();
            if targets_query(state) {
                state.queue_search();
            }
        }
        Command::ToggleSearchCase => {
            let state = file_search.as_mut()?;
            state.case_sensitive = !state.case_sensitive;
            state.queue_search();
        }
        Command::ToggleSearchRegex => {
            let state = file_search.as_mut()?;
            state.use_regex = !state.use_regex;
            state.queue_search();
        }
        Command::ToggleSearchReplace => {
            let state = file_search.as_mut()?;
            state.replace_mode = !state.replace_mode;
            // No re-search — replace mode only toggles the extra
            // input row; existing results stay.
        }
        Command::CursorDown => {
            move_selection(
                file_search.as_mut()?,
                tabs,
                edits,
                1,
                side_panel_rows(terminal),
            );
        }
        Command::CursorUp => {
            move_selection(
                file_search.as_mut()?,
                tabs,
                edits,
                -1,
                side_panel_rows(terminal),
            );
        }
        Command::InsertNewline => {
            handle_enter(
                file_search,
                browser,
                tabs,
                edits,
                side_panel_rows(terminal),
            );
        }
        Command::Abort | Command::CloseFileSearch => {
            deactivate(file_search, browser, tabs);
        }
        Command::Undo => {
            // Overlay-scoped undo: pop the most-recent group
            // across all loaded buffers, anchored to the floor
            // captured when the overlay opened. Per-hit replace +
            // inverse groups carry FileSearchMark tags so
            // hit_replacements stays consistent; the sidebar
            // selection + scroll follow the affected hit.
            let floor = file_search
                .as_ref()
                .map(|s| s.overlay_open_seq)
                .unwrap_or(0);
            super::undo::undo_global(
                tabs,
                edits,
                file_search.as_mut(),
                floor,
                side_panel_rows(terminal),
            );
        }
        Command::Redo => {
            let floor = file_search
                .as_ref()
                .map(|s| s.overlay_open_seq)
                .unwrap_or(0);
            super::undo::redo_global(
                tabs,
                edits,
                file_search.as_mut(),
                floor,
                side_panel_rows(terminal),
            );
        }
        Command::ReplaceAll => {
            let state = file_search.as_mut()?;
            if state.replace_mode {
                apply_replace_all(state, edits, fs_root);
                deactivate(file_search, browser, tabs);
            }
        }
        // Quit passes through so `Ctrl-X Ctrl-C` still exits.
        Command::Quit => return None,
        // Everything else is absorbed while the overlay owns focus.
        _ => {}
    }
    Some(DispatchOutcome::Continue)
}

/// Pick which `TextInput` the current selection points at.
fn input_for_selection(
    state: &mut FileSearchState,
) -> &mut led_core::TextInput {
    match state.selection {
        FileSearchSelection::ReplaceInput => &mut state.replace,
        // Result rows don't have an input — typing there falls
        // back to the search input (user intent: refine query).
        _ => &mut state.query,
    }
}

/// `true` when the active input is the search query (as opposed
/// to the replacement field). Used to gate `queue_search` so
/// typing into the replace input doesn't re-fire ripgrep with the
/// same unchanged query text.
fn targets_query(state: &FileSearchState) -> bool {
    !matches!(state.selection, FileSearchSelection::ReplaceInput)
}

/// `Enter` behaviour. Two paths:
///
/// - **Selection on a Result row** → commit. Promote the hit's
///   tab to non-preview, drop the cursor on the exact match
///   position (line + col), close the overlay. This is the
///   "jump into this hit" flow users expect after arrow-scanning.
/// - **Selection on an input row** → preview-only. Select the
///   first hit, move the preview tab to it, but keep the overlay
///   open so the user can keep refining / scanning.
///
/// No-op when there are no hits.
fn handle_enter(
    file_search: &mut Option<FileSearchState>,
    browser: &mut BrowserUi,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    body_rows: usize,
) {
    let (hit, commit) = {
        let state = match file_search.as_mut() {
            Some(s) if !s.flat_hits.is_empty() => s,
            _ => return,
        };
        let (idx, commit) = match state.selection {
            FileSearchSelection::Result(i) if i < state.flat_hits.len() => (i, true),
            _ => {
                state.selection = FileSearchSelection::Result(0);
                (0, false)
            }
        };
        (state.flat_hits[idx].clone(), commit)
    };
    if commit {
        jump_commit(&hit, file_search, browser, tabs, body_rows);
    } else {
        jump_preview(&hit, tabs, edits, body_rows);
    }
}

/// Commit the hit: promote its tab past preview, move the cursor to
/// the exact match position (1-indexed `hit.line`/`hit.col` become
/// 0-indexed), and close the overlay. `previous_tab` is cleared
/// first so `deactivate`'s `close_preview` doesn't re-focus the tab
/// that was active before the overlay opened.
fn jump_commit(
    hit: &FileSearchHit,
    file_search: &mut Option<FileSearchState>,
    browser: &mut BrowserUi,
    tabs: &mut Tabs,
    body_rows: usize,
) {
    open_or_focus_tab(tabs, &hit.path, /* promote */ true);
    let line = hit.line.saturating_sub(1);
    let col = hit.col.saturating_sub(1);
    if let Some(active_id) = tabs.active
        && let Some(idx) = tabs.open.iter().position(|t| t.id == active_id)
    {
        let tab = &mut tabs.open[idx];
        tab.cursor = Cursor {
            line,
            col,
            preferred_col: col,
        };
        tab.scroll.top = preview_scroll_top(line, body_rows);
    }
    if let Some(state) = file_search.as_mut() {
        state.previous_tab = None;
    }
    deactivate(file_search, browser, tabs);
}

/// Shift the selection by `delta` rows (`+1` = down, `-1` = up).
/// The row order is: `SearchInput`, (`ReplaceInput` when replace_mode
/// is on), then `Result(0..flat_hits.len())`. Saturating at the
/// ends. Landing on a `Result` row triggers a jump-preview so the
/// body mirrors the selection as the user scrolls; `side_rows` is
/// the number of rows available to the side panel and drives the
/// scroll-follow clamp on `scroll_offset`.
fn move_selection(
    state: &mut FileSearchState,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    delta: i32,
    body_rows: usize,
) {
    // Encode the current selection as a flat row index.
    let replace_slot = state.replace_mode as i64;
    let base = 1 + replace_slot; // rows before the first hit
    let current: i64 = match state.selection {
        FileSearchSelection::SearchInput => 0,
        FileSearchSelection::ReplaceInput => 1,
        FileSearchSelection::Result(i) => base + i as i64,
    };
    let total = base + state.flat_hits.len() as i64;
    let next = (current + delta as i64).clamp(0, total.saturating_sub(1));
    state.selection = if next == 0 {
        FileSearchSelection::SearchInput
    } else if state.replace_mode && next == 1 {
        FileSearchSelection::ReplaceInput
    } else {
        FileSearchSelection::Result((next - base) as usize)
    };
    // Side panel and body share the same row budget
    // (`dims.rows - 2` for both); the same value drives the
    // scroll-follow on the sidebar and the preview scroll below.
    clamp_scroll_to_selection(state, body_rows);
    if let FileSearchSelection::Result(i) = state.selection
        && let Some(hit) = state.flat_hits.get(i).cloned()
    {
        jump_preview(&hit, tabs, edits, body_rows);
    }
}

/// Minimum-movement scroll clamp. Persists `scroll_offset` into state
/// so subsequent up/down moves don't re-derive from a stale baseline.
/// Called after `state.selection` has been updated.
fn clamp_scroll_to_selection(
    state: &mut FileSearchState,
    side_rows: usize,
) {
    let input_rows = 1 + 1 + state.replace_mode as usize; // header + query [+ replace]
    let tree_visible = side_rows.saturating_sub(input_rows);
    if tree_visible == 0 {
        return;
    }
    let FileSearchSelection::Result(i) = state.selection else {
        return;
    };
    let stream = tree_row_index_for_hit(&state.results, i);
    if stream < state.scroll_offset {
        state.scroll_offset = stream;
    } else if stream >= state.scroll_offset + tree_visible {
        state.scroll_offset = stream + 1 - tree_visible;
    }
}

/// Same walk as `query::tree_row_index_for_hit`. Duplicated here to
/// keep `dispatch` free of a dependency on the render module's
/// helpers; the two stay in sync by construction.
fn tree_row_index_for_hit(
    groups: &[led_state_file_search::FileSearchGroup],
    flat_idx: usize,
) -> usize {
    let mut stream = 0usize;
    let mut seen = 0usize;
    for group in groups {
        stream += 1; // group header
        if flat_idx < seen + group.hits.len() {
            return stream + (flat_idx - seen);
        }
        stream += group.hits.len();
        seen += group.hits.len();
    }
    stream.saturating_sub(1)
}

/// Visible-row budget for the side panel. Matches
/// `Layout::compute`: side panel always gets `dims.rows - 2` rows
/// when visible; `0` when the terminal is too narrow or dims aren't
/// known yet (the overlay isn't useful in those states).
fn side_panel_rows(
    terminal: &led_driver_terminal_core::Terminal,
) -> usize {
    terminal
        .dims
        .map(|d| d.rows.saturating_sub(2) as usize)
        .unwrap_or(0)
}

/// `Alt+Enter` — project-wide replace-all.
///
/// Two paths, applied together:
///
/// 1. **In-memory.** For every currently-loaded buffer
///    (`edits.buffers`), run `regex.replace_all` against its rope.
///    Changed buffers get a fresh version via `shared::bump` so
///    `dirty()` flips — the session view becomes the source of
///    truth until the user saves. Per-file replacement counts are
///    stashed in `edits.pending_replace_in_memory` for the alert.
///
/// 2. **On-disk.** Dispatch pushes a `PendingReplaceAll` onto
///    `edits.pending_replace_all` with the set of loaded paths as
///    `skip_paths`. The main loop drains that queue and ships a
///    `FileSearchReplaceCmd` to `driver-file-search`, which walks
///    the workspace independently and rewrites the remaining files.
///
/// `fs_root` is the workspace root (dispatch's caller reads it off
/// `FsTree`). Missing root → the driver walk is skipped, in-memory
/// pass still runs.
/// `CursorRight` on a selected hit (replace_mode on) — if the hit
/// is still pending, apply the replacement and mark the row
/// replaced. Rows stay visible in the tree either way, so
/// Left-arrow on a specific replaced row can undo just that one
/// without disturbing others. Advances selection to the next
/// pending hit when one's available (wraps to the first pending).
fn replace_selected(
    state: &mut FileSearchState,
    _tabs: &mut Tabs,
    edits: &mut BufferEdits,
) {
    let FileSearchSelection::Result(idx) = state.selection else {
        return;
    };
    let Some(hit) = state.flat_hits.get(idx).cloned() else {
        return;
    };
    // Ensure the replacements vec is at least as long as flat_hits
    // — defensive in case something (a test, a stray path) bypassed
    // the runtime's post-search resize.
    ensure_replacements_len(state);
    // Already replaced — Right on an already-handled row is a
    // no-op (user can Left to undo).
    if state.hit_replacements[idx].is_some() {
        advance_to_next_pending(state);
        return;
    }
    if state.query.text.is_empty() && state.replace.text.is_empty() {
        return;
    }
    let original_char_len = hit
        .preview
        .get(hit.match_start..hit.match_end)
        .map(|s| s.chars().count())
        .unwrap_or(0);
    if original_char_len == 0 {
        return;
    }
    let original_text = hit
        .preview
        .get(hit.match_start..hit.match_end)
        .unwrap_or("")
        .to_string();
    let replacement = state.replace.text.clone();
    let replacement_char_len = replacement.chars().count();

    let rope_char_start = if let Some(eb) = edits.buffers.get_mut(&hit.path) {
        // Loaded-buffer path: splice the rope in place AND record
        // the edit on the buffer's history as one compound replace
        // group, tagged with a FileSearchMark so undo/redo can
        // resync the overlay's hit_replacements vec.
        let line0 = hit.line.saturating_sub(1);
        if line0 >= eb.rope.len_lines() {
            return;
        }
        let line_start = eb.rope.line_to_char(line0);
        let match_char_start = line_start + hit.col.saturating_sub(1);
        let match_char_end = match_char_start + original_char_len;
        if match_char_end > eb.rope.len_chars() {
            return;
        }
        let mut new_rope = (*eb.rope).clone();
        new_rope.remove(match_char_start..match_char_end);
        if !replacement.is_empty() {
            new_rope.insert(match_char_start, &replacement);
        }
        let cursor_anchor = led_state_tabs::Cursor {
            line: line0,
            col: hit.col.saturating_sub(1),
            preferred_col: hit.col.saturating_sub(1),
        };
        eb.history.record_replace(
            match_char_start,
            std::sync::Arc::<str>::from(original_text.as_str()),
            std::sync::Arc::<str>::from(replacement.as_str()),
            cursor_anchor,
            cursor_anchor,
            Some(led_state_buffer_edits::FileSearchMark {
                hit_idx: idx,
                forward_marks_replaced: true,
            }),
        );
        super::shared::bump(eb, new_rope);
        match_char_start
    } else {
        // On-disk path: queue a driver cmd. `rope_char_start` is
        // meaningless for unloaded buffers — use 0 as a sentinel;
        // undo uses (line, col) via the driver inverse cmd.
        edits.pending_single_replace.push(
            led_state_buffer_edits::PendingSingleReplace {
                path: hit.path.clone(),
                line: hit.line,
                match_start: hit.match_start,
                match_end: hit.match_end,
                original: original_text.clone(),
                replacement: replacement.clone(),
            },
        );
        0
    };

    state.hit_replacements[idx] = Some(led_state_file_search::ReplaceEntry {
        hit: hit.clone(),
        replacement_text: replacement,
        replacement_char_len,
        original_char_len,
        rope_char_start,
        path: hit.path.clone(),
    });

    advance_to_next_pending(state);
}

/// `CursorLeft` on a selected hit — if the hit is marked replaced,
/// revert it. Selection stays on the row so the user can
/// immediately Right-arrow to redo if wanted.
fn unreplace_selected(
    state: &mut FileSearchState,
    _tabs: &mut Tabs,
    edits: &mut BufferEdits,
) {
    let FileSearchSelection::Result(idx) = state.selection else {
        return;
    };
    ensure_replacements_len(state);
    let Some(entry) = state.hit_replacements[idx].take() else {
        return;
    };
    let original = entry
        .hit
        .preview
        .get(entry.hit.match_start..entry.hit.match_end)
        .unwrap_or("")
        .to_string();
    if let Some(eb) = edits.buffers.get_mut(&entry.path) {
        let rope_len = eb.rope.len_chars();
        let replacement_end = entry
            .rope_char_start
            .saturating_add(entry.replacement_char_len);
        if replacement_end > rope_len {
            // Rope shrank past the replacement — abandon the undo.
            // Restore the entry so state stays consistent.
            state.hit_replacements[idx] = Some(entry);
            return;
        }
        let mut new_rope = (*eb.rope).clone();
        if entry.replacement_char_len > 0 {
            new_rope.remove(entry.rope_char_start..replacement_end);
        }
        if !original.is_empty() {
            new_rope.insert(entry.rope_char_start, &original);
        }
        let line0 = entry.hit.line.saturating_sub(1);
        let col0 = entry.hit.col.saturating_sub(1);
        let cursor_anchor = led_state_tabs::Cursor {
            line: line0,
            col: col0,
            preferred_col: col0,
        };
        eb.history.record_replace(
            entry.rope_char_start,
            std::sync::Arc::<str>::from(entry.replacement_text.as_str()),
            std::sync::Arc::<str>::from(original.as_str()),
            cursor_anchor,
            cursor_anchor,
            Some(led_state_buffer_edits::FileSearchMark {
                hit_idx: idx,
                forward_marks_replaced: false,
            }),
        );
        super::shared::bump(eb, new_rope);
    } else {
        let replacement_bytes = entry.replacement_text.len();
        let replacement_end_byte = entry.hit.match_start + replacement_bytes;
        edits.pending_single_replace.push(
            led_state_buffer_edits::PendingSingleReplace {
                path: entry.path.clone(),
                line: entry.hit.line,
                match_start: entry.hit.match_start,
                match_end: replacement_end_byte,
                original: entry.replacement_text.clone(),
                replacement: original,
            },
        );
    }
}

/// Advance selection to the next pending hit after the current
/// index, wrapping to the start. No-op (selection stays) if every
/// hit has already been replaced — user can Left to undo where
/// they are, or Down to move within the fully-replaced set.
fn advance_to_next_pending(state: &mut FileSearchState) {
    let FileSearchSelection::Result(idx) = state.selection else {
        return;
    };
    let n = state.flat_hits.len();
    if n == 0 {
        return;
    }
    // Look forward from idx+1, wrap to 0, back to idx.
    for step in 1..=n {
        let candidate = (idx + step) % n;
        if state
            .hit_replacements
            .get(candidate)
            .and_then(|e| e.as_ref())
            .is_none()
        {
            state.selection = FileSearchSelection::Result(candidate);
            return;
        }
    }
    // All replaced — stay put.
}

fn ensure_replacements_len(state: &mut FileSearchState) {
    if state.hit_replacements.len() != state.flat_hits.len() {
        state.hit_replacements = vec![None; state.flat_hits.len()];
    }
}

fn apply_replace_all(
    state: &led_state_file_search::FileSearchState,
    edits: &mut led_state_buffer_edits::BufferEdits,
    fs_root: Option<&led_core::CanonPath>,
) {
    if state.query.text.is_empty() {
        return;
    }
    let pattern = if state.use_regex {
        state.query.text.clone()
    } else {
        regex_syntax::escape(&state.query.text)
    };
    let re = match regex::RegexBuilder::new(&pattern)
        .case_insensitive(!state.case_sensitive)
        .build()
    {
        Ok(r) => r,
        Err(_) => return,
    };
    let replacement = state.replace.text.as_str();

    // In-memory pass: every loaded buffer that has at least one
    // match gets rewritten. Not limited to `state.results` —
    // changes the user has typed since the last search still count.
    let mut skip_paths: Vec<led_core::CanonPath> =
        Vec::with_capacity(edits.buffers.len());
    let mut loaded_paths: Vec<led_core::CanonPath> =
        edits.buffers.keys().cloned().collect();
    // Deterministic order makes tests + trace diffs stable.
    loaded_paths.sort_by(|a, b| a.as_path().cmp(b.as_path()));
    for path in loaded_paths {
        let Some(eb) = edits.buffers.get_mut(&path) else {
            continue;
        };
        let existing = eb.rope.to_string();
        let count = re.find_iter(&existing).count();
        if count == 0 {
            continue;
        }
        let replaced = re.replace_all(&existing, replacement);
        if replaced.as_ref() != existing {
            super::shared::bump(eb, ropey::Rope::from_str(replaced.as_ref()));
            edits
                .pending_replace_in_memory
                .push(led_state_buffer_edits::InMemoryReplace {
                    path: path.clone(),
                    count,
                });
        }
        skip_paths.push(path);
    }

    // Skip_paths also needs the full loaded set (even buffers
    // with zero matches) so the driver can't race and clobber a
    // loaded-but-unmatched file with an on-disk rewrite that
    // happens to succeed via regex differences we missed.
    for path in edits.buffers.keys() {
        if !skip_paths.contains(path) {
            skip_paths.push(path.clone());
        }
    }

    if let Some(root) = fs_root {
        edits.pending_replace_all.push(
            led_state_buffer_edits::PendingReplaceAll {
                root: root.clone(),
                query: state.query.text.clone(),
                replacement: replacement.to_string(),
                case_sensitive: state.case_sensitive,
                use_regex: state.use_regex,
                skip_paths,
            },
        );
    }
}

/// Open (or focus) the hit's file as a preview tab and position the
/// cursor on the match. `open_or_focus_tab(promote=false)` re-uses an
/// existing tab for the same path, otherwise creates a preview. The
/// cursor goes to the start of the match line (col 0 keeps the
/// preview unobtrusive; commit-Enter is what jumps onto the match
/// column). The viewport scrolls so the hit sits ~1/3 down from
/// the top when there's room — giving the user some context above.
fn jump_preview(
    hit: &FileSearchHit,
    tabs: &mut Tabs,
    edits: &BufferEdits,
    body_rows: usize,
) {
    open_or_focus_tab(tabs, &hit.path, false);
    let Some(active_id) = tabs.active else { return };
    let Some(idx) = tabs.open.iter().position(|t| t.id == active_id) else {
        return;
    };
    let line = hit.line.saturating_sub(1);
    let tab = &mut tabs.open[idx];
    tab.cursor = Cursor {
        line,
        col: 0,
        preferred_col: 0,
    };
    tab.scroll.top = preview_scroll_top(line, body_rows);
    let _ = edits;
}

/// Scroll the viewport so `line` lands roughly 1/3 down from the
/// top, leaving context above. Returns 0 when the hit is too close
/// to the start of the buffer for 1/3 to fit, or when rows is 0.
fn preview_scroll_top(line: usize, body_rows: usize) -> usize {
    if body_rows == 0 {
        return line;
    }
    let offset = body_rows / 3;
    line.saturating_sub(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activate_opens_overlay_and_moves_focus_to_side() {
        let mut fs = None;
        let mut browser = BrowserUi::default();
        let tabs = Tabs::default();
        assert_eq!(browser.focus, Focus::Main);
        activate(&mut fs, &mut browser, &tabs, &BufferEdits::default());
        assert!(fs.is_some());
        assert!(browser.visible);
        assert_eq!(browser.focus, Focus::Side);
    }

    #[test]
    fn deactivate_clears_and_restores_focus() {
        let mut fs = Some(FileSearchState::default());
        let mut browser = BrowserUi {
            visible: true,
            focus: Focus::Side,
            ..Default::default()
        };
        let mut tabs = Tabs::default();
        deactivate(&mut fs, &mut browser, &mut tabs);
        assert!(fs.is_none());
        assert_eq!(browser.focus, Focus::Main);
    }

    #[test]
    fn activate_twice_is_noop_on_the_second_call() {
        let mut fs = None;
        let mut browser = BrowserUi::default();
        let tabs = Tabs::default();
        activate(&mut fs, &mut browser, &tabs, &BufferEdits::default());
        let first = fs.clone();
        activate(&mut fs, &mut browser, &tabs, &BufferEdits::default());
        assert_eq!(fs, first);
    }

    #[test]
    fn activate_captures_previous_tab() {
        use led_state_tabs::{Tab, TabId};
        let mut fs = None;
        let mut browser = BrowserUi::default();
        let tabs = Tabs {
            open: imbl::vector![Tab {
                id: TabId(7),
                ..Default::default()
            }],
            active: Some(TabId(7)),
        };
        activate(&mut fs, &mut browser, &tabs, &BufferEdits::default());
        assert_eq!(fs.unwrap().previous_tab, Some(TabId(7)));
    }

    fn fs_state_with_hits(n_hits: usize) -> FileSearchState {
        use led_state_file_search::{FileSearchGroup, FileSearchHit};
        let path = led_core::UserPath::new("a.rs").canonicalize();
        let hits: Vec<FileSearchHit> = (1..=n_hits)
            .map(|i| FileSearchHit {
                path: path.clone(),
                line: i,
                col: 1,
                preview: format!("hit {i}"),
                match_start: 0,
                match_end: 0,
            })
            .collect();
        FileSearchState {
            results: vec![FileSearchGroup {
                path,
                relative: "a.rs".into(),
                hits: hits.clone(),
            }],
            flat_hits: hits,
            ..Default::default()
        }
    }

    #[test]
    fn move_selection_does_not_scroll_up_until_selection_leaves_the_top() {
        // 4 side rows = 2 pinned (header + query) + 2 tree rows.
        // Stream layout: 0=a.rs header, 1=hit1, 2=hit2, …, 6=hit6.
        //
        // Scroll down until the viewport shows hit5+hit6 (scroll=5),
        // then arrow up. The viewport must hold steady until the
        // selection exits the top of it — then scroll one row per
        // further up-arrow. No scrolling while the selection is
        // still inside the visible window.
        let mut state = fs_state_with_hits(6);
        let mut tabs = Tabs::default();
        let mut edits = led_state_buffer_edits::BufferEdits::default();
        let side_rows = 4;

        // Down 6 times: SearchInput → Result(0..5).
        for _ in 0..6 {
            move_selection(&mut state, &mut tabs, &mut edits, 1, side_rows);
        }
        assert_eq!(state.selection, FileSearchSelection::Result(5));
        // Selected stream row = 6; tree_visible = 2; scroll clamped
        // to 6 + 1 - 2 = 5. Viewport = stream 5+6 (hit5+hit6).
        assert_eq!(state.scroll_offset, 5);

        // Arrow up once: selection = Result(4) → stream 5. That's
        // still the top of the visible window, so no scroll.
        move_selection(&mut state, &mut tabs, &mut edits, -1, side_rows);
        assert_eq!(state.selection, FileSearchSelection::Result(4));
        assert_eq!(state.scroll_offset, 5);

        // Arrow up again: selection = Result(3) → stream 4. Now
        // 4 < 5 → scroll follows selection up to 4. Viewport =
        // stream 4+5 (hit4+hit5).
        move_selection(&mut state, &mut tabs, &mut edits, -1, side_rows);
        assert_eq!(state.selection, FileSearchSelection::Result(3));
        assert_eq!(state.scroll_offset, 4);
    }

    #[test]
    fn move_selection_scrolls_down_when_selection_leaves_the_bottom() {
        let mut state = fs_state_with_hits(6);
        let mut tabs = Tabs::default();
        let mut edits = led_state_buffer_edits::BufferEdits::default();
        let side_rows = 4; // 2 tree rows visible

        // Down three times: SearchInput → Result(0) → Result(1) →
        // Result(2). Stream rows 1, 2, 3. Initial scroll = 0 so
        // tree shows stream 0+1 (header + hit1). After first
        // down-arrow (stream 1) still fits. After third (stream 3)
        // scroll clamps up to 2.
        move_selection(&mut state, &mut tabs, &mut edits, 1, side_rows);
        assert_eq!(state.scroll_offset, 0);
        move_selection(&mut state, &mut tabs, &mut edits, 1, side_rows);
        assert_eq!(state.scroll_offset, 1);
        move_selection(&mut state, &mut tabs, &mut edits, 1, side_rows);
        assert_eq!(state.scroll_offset, 2);
    }

    #[test]
    fn enter_on_result_row_commits_promotes_tab_and_closes_overlay() {
        // Build a state with one hit at line 3, col 7 (1-indexed),
        // selection already on Result(0). Tab for the hit's file
        // is open as a preview. Enter should:
        //  - promote the tab (preview → real)
        //  - position cursor at (line=2, col=6) in 0-indexed form
        //  - deactivate the overlay (file_search becomes None)
        //  - leave focus on Main (editor), not Side.
        use led_state_browser::{BrowserUi, Focus};
        use led_state_file_search::{FileSearchGroup, FileSearchHit};
        use led_state_tabs::{Tab, TabId};

        let path = led_core::UserPath::new("a.rs").canonicalize();
        let hit = FileSearchHit {
            path: path.clone(),
            line: 3,
            col: 7,
            preview: "    let foo = bar;".into(),
            match_start: 8,
            match_end: 11,
        };
        let state = FileSearchState {
            results: vec![FileSearchGroup {
                path: path.clone(),
                relative: "a.rs".into(),
                hits: vec![hit.clone()],
            }],
            flat_hits: vec![hit],
            selection: FileSearchSelection::Result(0),
            previous_tab: Some(TabId(99)), // pretend some other tab was active
            ..Default::default()
        };
        let mut file_search = Some(state);
        let mut tabs = Tabs {
            open: imbl::vector![Tab {
                id: TabId(1),
                path: path.clone(),
                preview: true,
                ..Default::default()
            }],
            active: Some(TabId(1)),
        };
        let mut browser = BrowserUi {
            focus: Focus::Side,
            visible: true,
            ..Default::default()
        };
        let mut edits = led_state_buffer_edits::BufferEdits::default();
        // Small viewport so body_rows/3 = 0 and the match stays at
        // the very top — makes the scroll.top assertion concrete.
        let body_rows = 3;

        handle_enter(&mut file_search, &mut browser, &mut tabs, &mut edits, body_rows);

        // Overlay closed.
        assert!(file_search.is_none());
        assert_eq!(browser.focus, Focus::Main);
        // Tab survived (wasn't closed by a stale previous_tab restore)
        // and is now a real (non-preview) tab.
        assert_eq!(tabs.open.len(), 1);
        assert_eq!(tabs.open[0].id, TabId(1));
        assert!(!tabs.open[0].preview);
        assert_eq!(tabs.active, Some(TabId(1)));
        // Cursor at the match position, not line start.
        assert_eq!(tabs.open[0].cursor.line, 2);
        assert_eq!(tabs.open[0].cursor.col, 6);
        assert_eq!(tabs.open[0].cursor.preferred_col, 6);
        // body_rows/3 = 1 → scroll = line - 1 = 1.
        assert_eq!(tabs.open[0].scroll.top, 1);
    }

    #[test]
    fn enter_on_search_input_previews_first_hit_and_keeps_overlay_open() {
        // Contrast with the commit test: selection on SearchInput
        // should keep the overlay open and leave the tab as a
        // preview. Cursor lands at col 0 of the hit line (preview
        // behaviour from stage 6), not the match col.
        use led_state_browser::{BrowserUi, Focus};
        use led_state_file_search::{FileSearchGroup, FileSearchHit};

        let path = led_core::UserPath::new("a.rs").canonicalize();
        let hit = FileSearchHit {
            path: path.clone(),
            line: 3,
            col: 7,
            preview: "    let foo = bar;".into(),
            match_start: 8,
            match_end: 11,
        };
        let state = FileSearchState {
            results: vec![FileSearchGroup {
                path: path.clone(),
                relative: "a.rs".into(),
                hits: vec![hit.clone()],
            }],
            flat_hits: vec![hit],
            selection: FileSearchSelection::SearchInput,
            ..Default::default()
        };
        let mut file_search = Some(state);
        let mut tabs = Tabs::default();
        let mut browser = BrowserUi {
            focus: Focus::Side,
            visible: true,
            ..Default::default()
        };
        let mut edits = led_state_buffer_edits::BufferEdits::default();
        let body_rows = 3;

        handle_enter(&mut file_search, &mut browser, &mut tabs, &mut edits, body_rows);

        assert!(file_search.is_some());
        let state = file_search.as_ref().unwrap();
        assert_eq!(state.selection, FileSearchSelection::Result(0));
        // A preview tab was created (file wasn't previously open).
        assert_eq!(tabs.open.len(), 1);
        assert!(tabs.open[0].preview);
        assert_eq!(tabs.open[0].cursor.line, 2);
        // Preview parks the cursor at col 0, not the match col.
        assert_eq!(tabs.open[0].cursor.col, 0);
    }

    #[test]
    fn apply_replace_all_splits_loaded_and_ondisk_paths() {
        // Two loaded buffers — one matching the query, one not —
        // plus a workspace root present. After apply_replace_all:
        //  - matched loaded buffer's rope is rewritten + version
        //    bumped + InMemoryReplace staged with count=2.
        //  - unmatched loaded buffer has no replacement row.
        //  - BOTH loaded paths land in pending_replace_all.skip_paths
        //    so the driver walk won't overwrite them.
        //  - pending_replace_all has one entry with the query +
        //    replacement + the workspace root.
        use led_core::UserPath;
        use led_state_buffer_edits::{BufferEdits, EditedBuffer};
        use led_state_file_search::{FileSearchGroup, FileSearchHit};

        let root = UserPath::new(".").canonicalize();
        let path_match = UserPath::new("./a.rs").canonicalize();
        let path_other = UserPath::new("./b.rs").canonicalize();

        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            path_match.clone(),
            EditedBuffer::fresh(std::sync::Arc::new(ropey::Rope::from_str(
                "foo here\nand foo\n",
            ))),
        );
        edits.buffers.insert(
            path_other.clone(),
            EditedBuffer::fresh(std::sync::Arc::new(ropey::Rope::from_str(
                "no match\n",
            ))),
        );

        // Query/toggles that will match "foo" literally. results
        // list is deliberately incomplete — apply_replace_all must
        // not rely on it.
        let hit = FileSearchHit {
            path: path_match.clone(),
            line: 1,
            col: 1,
            preview: "foo here".into(),
            match_start: 0,
            match_end: 3,
        };
        let mut state = FileSearchState::default();
        state.query.set("foo");
        state.replace.set("BAR");
        state.replace_mode = true;
        state.results = vec![FileSearchGroup {
            path: path_match.clone(),
            relative: "a.rs".into(),
            hits: vec![hit.clone()],
        }];
        state.flat_hits = vec![hit];

        apply_replace_all(&state, &mut edits, Some(&root));

        // In-memory: matched buffer got rewritten, stage counted 2.
        assert_eq!(
            edits.buffers[&path_match].rope.to_string(),
            "BAR here\nand BAR\n",
        );
        assert_eq!(edits.buffers[&path_other].rope.to_string(), "no match\n");
        assert_eq!(edits.pending_replace_in_memory.len(), 1);
        assert_eq!(edits.pending_replace_in_memory[0].path, path_match);
        assert_eq!(edits.pending_replace_in_memory[0].count, 2);

        // On-disk: one queued cmd with both loaded paths in skip.
        assert_eq!(edits.pending_replace_all.len(), 1);
        let cmd = &edits.pending_replace_all[0];
        assert_eq!(cmd.root, root);
        assert_eq!(cmd.query, "foo");
        assert_eq!(cmd.replacement, "BAR");
        assert!(!cmd.case_sensitive);
        assert!(!cmd.use_regex);
        assert!(cmd.skip_paths.contains(&path_match));
        assert!(cmd.skip_paths.contains(&path_other));
    }

    #[test]
    fn apply_replace_all_with_no_workspace_still_rewrites_loaded_buffers() {
        // No fs.root → no on-disk cmd queued, but the in-memory
        // pass should still run so the user's loaded buffers pick
        // up the change.
        use led_state_buffer_edits::{BufferEdits, EditedBuffer};

        let path = led_core::UserPath::new("/tmp/a.rs").canonicalize();
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            path.clone(),
            EditedBuffer::fresh(std::sync::Arc::new(ropey::Rope::from_str(
                "alpha foo\n",
            ))),
        );

        let mut state = FileSearchState::default();
        state.query.set("foo");
        state.replace.set("BAR");
        state.replace_mode = true;

        apply_replace_all(&state, &mut edits, None);

        assert_eq!(edits.buffers[&path].rope.to_string(), "alpha BAR\n");
        assert_eq!(edits.pending_replace_in_memory.len(), 1);
        assert!(edits.pending_replace_all.is_empty());
    }

    // ── Per-hit replace (Right/Left on a Result row) ─────────────

    fn fs_state_with_hit_in(
        path: &led_core::CanonPath,
        line_text: &str,
        line: usize,
        col: usize,
        match_bytes: (usize, usize),
    ) -> FileSearchState {
        use led_state_file_search::{FileSearchGroup, FileSearchHit};
        let hit = FileSearchHit {
            path: path.clone(),
            line,
            col,
            preview: line_text.to_string(),
            match_start: match_bytes.0,
            match_end: match_bytes.1,
        };
        let mut state = FileSearchState::default();
        state.query.set("foo");
        state.replace.set("BAR");
        state.replace_mode = true;
        state.results = vec![FileSearchGroup {
            path: path.clone(),
            relative: "a.rs".into(),
            hits: vec![hit.clone()],
        }];
        state.flat_hits = vec![hit];
        state.selection = FileSearchSelection::Result(0);
        state
    }

    #[test]
    fn right_arrow_on_result_marks_hit_replaced_and_advances() {
        use led_state_buffer_edits::{BufferEdits, EditedBuffer};

        let path = led_core::UserPath::new("/tmp/a.rs").canonicalize();
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            path.clone(),
            EditedBuffer::fresh(std::sync::Arc::new(ropey::Rope::from_str(
                "alpha foo beta\n",
            ))),
        );

        let mut state = fs_state_with_hit_in(
            &path,
            "alpha foo beta",
            1,
            7,
            (6, 9),
        );
        // Second (unrelated) hit so we can verify advance-to-next.
        let mut tabs = Tabs::default();
        let second = led_state_file_search::FileSearchHit {
            path: path.clone(),
            line: 2,
            col: 1,
            preview: "foo at start".into(),
            match_start: 0,
            match_end: 3,
        };
        state.results[0].hits.push(second.clone());
        state.flat_hits.push(second);
        state.hit_replacements = vec![None; state.flat_hits.len()];

        replace_selected(&mut state, &mut tabs, &mut edits);

        // Rope mutated.
        assert_eq!(edits.buffers[&path].rope.to_string(), "alpha BAR beta\n");
        // Hits list UNCHANGED in length — the replaced row stays
        // visible.
        assert_eq!(state.flat_hits.len(), 2);
        // First row marked replaced, second is still pending.
        assert!(state.hit_replacements[0].is_some());
        assert!(state.hit_replacements[1].is_none());
        // Selection advanced to the next pending hit.
        assert_eq!(state.selection, FileSearchSelection::Result(1));
    }

    #[test]
    fn left_arrow_on_replaced_row_undoes_that_specific_hit() {
        use led_state_buffer_edits::{BufferEdits, EditedBuffer};

        let path = led_core::UserPath::new("/tmp/a.rs").canonicalize();
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            path.clone(),
            EditedBuffer::fresh(std::sync::Arc::new(ropey::Rope::from_str(
                "alpha foo beta\n",
            ))),
        );
        let mut state = fs_state_with_hit_in(
            &path,
            "alpha foo beta",
            1,
            7,
            (6, 9),
        );
        state.hit_replacements = vec![None; state.flat_hits.len()];
        let mut tabs = Tabs::default();

        // Forward: replace.
        replace_selected(&mut state, &mut tabs, &mut edits);
        assert_eq!(edits.buffers[&path].rope.to_string(), "alpha BAR beta\n");
        assert!(state.hit_replacements[0].is_some());

        // Selection wrapped back to 0 (the only hit, already
        // replaced) — selection stays put. Left-arrow there reverts
        // that specific row.
        assert_eq!(state.selection, FileSearchSelection::Result(0));
        unreplace_selected(&mut state, &mut tabs, &mut edits);

        assert_eq!(edits.buffers[&path].rope.to_string(), "alpha foo beta\n");
        assert!(state.hit_replacements[0].is_none());
        // flat_hits still contains the row — undo doesn't add or
        // remove rows.
        assert_eq!(state.flat_hits.len(), 1);
    }

    #[test]
    fn left_arrow_on_pending_row_is_noop() {
        use led_state_buffer_edits::BufferEdits;

        let path = led_core::UserPath::new("/tmp/a.rs").canonicalize();
        let mut edits = BufferEdits::default();
        let mut state = fs_state_with_hit_in(
            &path,
            "alpha foo beta",
            1,
            7,
            (6, 9),
        );
        state.hit_replacements = vec![None; state.flat_hits.len()];
        let mut tabs = Tabs::default();

        unreplace_selected(&mut state, &mut tabs, &mut edits);

        assert!(state.hit_replacements[0].is_none());
    }

    #[test]
    fn right_arrow_on_on_disk_hit_queues_driver_cmd() {
        use led_state_buffer_edits::BufferEdits;

        let path = led_core::UserPath::new("/tmp/unloaded.rs").canonicalize();
        let mut edits = BufferEdits::default();
        let mut state = fs_state_with_hit_in(
            &path,
            "alpha foo beta",
            1,
            7,
            (6, 9),
        );
        state.hit_replacements = vec![None; state.flat_hits.len()];
        let mut tabs = Tabs::default();

        replace_selected(&mut state, &mut tabs, &mut edits);

        // Row still visible, marked replaced.
        assert_eq!(state.flat_hits.len(), 1);
        assert!(state.hit_replacements[0].is_some());
        // Driver cmd queued with the right coords.
        assert_eq!(edits.pending_single_replace.len(), 1);
        let cmd = &edits.pending_single_replace[0];
        assert_eq!(cmd.path, path);
        assert_eq!(cmd.line, 1);
        assert_eq!(cmd.match_start, 6);
        assert_eq!(cmd.match_end, 9);
        assert_eq!(cmd.original, "foo");
        assert_eq!(cmd.replacement, "BAR");
    }

    #[test]
    fn left_arrow_undoes_on_disk_replace_by_queueing_inverse_cmd() {
        use led_state_buffer_edits::BufferEdits;

        let path = led_core::UserPath::new("/tmp/unloaded.rs").canonicalize();
        let mut edits = BufferEdits::default();
        let mut state = fs_state_with_hit_in(
            &path,
            "alpha foo beta",
            1,
            7,
            (6, 9),
        );
        state.hit_replacements = vec![None; state.flat_hits.len()];
        let mut tabs = Tabs::default();
        replace_selected(&mut state, &mut tabs, &mut edits);
        assert_eq!(edits.pending_single_replace.len(), 1);
        assert!(state.hit_replacements[0].is_some());

        // Undo on the same row queues the inverse cmd and clears
        // hit_replacements[0].
        unreplace_selected(&mut state, &mut tabs, &mut edits);
        assert_eq!(edits.pending_single_replace.len(), 2);
        let inverse = &edits.pending_single_replace[1];
        assert_eq!(inverse.path, path);
        assert_eq!(inverse.line, 1);
        assert_eq!(inverse.match_start, 6);
        assert_eq!(inverse.match_end, 9); // 6 + "BAR".len() = 9
        assert_eq!(inverse.original, "BAR");
        assert_eq!(inverse.replacement, "foo");
        assert!(state.hit_replacements[0].is_none());
    }

    #[test]
    fn queue_search_clears_hit_replacements() {
        use led_state_buffer_edits::{BufferEdits, EditedBuffer};

        let path = led_core::UserPath::new("/tmp/a.rs").canonicalize();
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            path.clone(),
            EditedBuffer::fresh(std::sync::Arc::new(ropey::Rope::from_str(
                "alpha foo beta\n",
            ))),
        );
        let mut state = fs_state_with_hit_in(
            &path,
            "alpha foo beta",
            1,
            7,
            (6, 9),
        );
        state.hit_replacements = vec![None; state.flat_hits.len()];
        let mut tabs = Tabs::default();
        replace_selected(&mut state, &mut tabs, &mut edits);
        assert!(state.hit_replacements[0].is_some());

        state.query.insert_char('x');
        state.queue_search();

        // Replacements vec goes to length 0 (cleared) — the runtime
        // resizes it back when a fresh driver response lands.
        assert!(state.hit_replacements.is_empty());
    }

    // ── Global undo / redo ────────────────────────────────────

    /// Helper: set up two loaded buffers with the shared seq_gen
    /// from a single BufferEdits so groups land in global order.
    fn two_buffer_edits(
        a_path: &led_core::CanonPath,
        a_content: &str,
        b_path: &led_core::CanonPath,
        b_content: &str,
    ) -> led_state_buffer_edits::BufferEdits {
        use led_state_buffer_edits::{BufferEdits, EditedBuffer};
        let mut edits = BufferEdits::default();
        let sg = edits.seq_gen.clone();
        edits.buffers.insert(
            a_path.clone(),
            EditedBuffer::fresh_with_seq_gen(
                std::sync::Arc::new(ropey::Rope::from_str(a_content)),
                sg.clone(),
            ),
        );
        edits.buffers.insert(
            b_path.clone(),
            EditedBuffer::fresh_with_seq_gen(
                std::sync::Arc::new(ropey::Rope::from_str(b_content)),
                sg,
            ),
        );
        edits
    }

    fn fs_state_with_hit_for(
        state: &mut FileSearchState,
        path: &led_core::CanonPath,
        line_text: &str,
        line: usize,
        col: usize,
        match_bytes: (usize, usize),
    ) -> usize {
        use led_state_file_search::{FileSearchGroup, FileSearchHit};
        let hit = FileSearchHit {
            path: path.clone(),
            line,
            col,
            preview: line_text.to_string(),
            match_start: match_bytes.0,
            match_end: match_bytes.1,
        };
        let idx = state.flat_hits.len();
        state.flat_hits.push(hit.clone());
        state.hit_replacements.push(None);
        let gi = state
            .results
            .iter()
            .position(|g| &g.path == path)
            .unwrap_or_else(|| {
                state.results.push(FileSearchGroup {
                    path: path.clone(),
                    relative: path.as_path().display().to_string(),
                    hits: Vec::new(),
                });
                state.results.len() - 1
            });
        state.results[gi].hits.push(hit);
        idx
    }

    #[test]
    fn undo_global_picks_max_seq_buffer_and_syncs_marks() {
        // Replace one hit in A, then one in B, then Ctrl+_ in
        // overlay twice. First undo reverts B (higher seq); second
        // reverts A. Both hit_replacements flip back to None.
        use super::super::undo::{redo_global, undo_global};

        let a = led_core::UserPath::new("/tmp/a.rs").canonicalize();
        let b = led_core::UserPath::new("/tmp/b.rs").canonicalize();
        let mut edits = two_buffer_edits(&a, "foo in a\n", &b, "foo in b\n");

        let mut state = FileSearchState::default();
        state.query.set("foo");
        state.replace.set("BAR");
        state.replace_mode = true;
        let idx_a = fs_state_with_hit_for(&mut state, &a, "foo in a", 1, 1, (0, 3));
        let idx_b = fs_state_with_hit_for(&mut state, &b, "foo in b", 1, 1, (0, 3));

        let mut tabs = Tabs::default();

        // Replace A.
        state.selection = FileSearchSelection::Result(idx_a);
        replace_selected(&mut state, &mut tabs, &mut edits);
        assert_eq!(edits.buffers[&a].rope.to_string(), "BAR in a\n");

        // Replace B.
        state.selection = FileSearchSelection::Result(idx_b);
        replace_selected(&mut state, &mut tabs, &mut edits);
        assert_eq!(edits.buffers[&b].rope.to_string(), "BAR in b\n");
        assert!(state.hit_replacements[idx_a].is_some());
        assert!(state.hit_replacements[idx_b].is_some());

        // Global undo → pops B (higher seq).
        undo_global(&mut tabs, &mut edits, Some(&mut state), 0, 40);
        assert_eq!(edits.buffers[&b].rope.to_string(), "foo in b\n");
        assert_eq!(edits.buffers[&a].rope.to_string(), "BAR in a\n");
        assert!(state.hit_replacements[idx_a].is_some());
        assert!(state.hit_replacements[idx_b].is_none());

        // Again → pops A.
        undo_global(&mut tabs, &mut edits, Some(&mut state), 0, 40);
        assert_eq!(edits.buffers[&a].rope.to_string(), "foo in a\n");
        assert!(state.hit_replacements[idx_a].is_none());

        // Redo once → restores A (smaller future seq is the only
        // one > floor=0 initially... actually A's future now has
        // the A-replace with seq < B's. redo picks max, so B's
        // replace comes back first).
        redo_global(&mut tabs, &mut edits, Some(&mut state), 0, 40);
        // Future's max seq is B's; but wait A was popped second
        // so A went to future LATER — higher seq on future is A.
        // Actually take_undo/push_future preserves group.seq. A
        // got seq=1 when first recorded, B got seq=2. After
        // undo-B then undo-A, future has [B(2), A(1)] where A is
        // on top (pushed last). top-seq of future = A's seq = 1.
        //
        // max future_top_seq across buffers: A's top future seq
        // is 1, B's top future seq is 2. redo picks B (max), so
        // we'd redo B first. But we JUST undid A most recently!
        //
        // This is the one non-intuitive bit of global-seq redo:
        // it recovers the order of original FORWARD edits, not
        // the reverse of the undo sequence. That matches "redo
        // reapplies the last-applied forward edit that's now in
        // future" across buffers — the one with the largest seq.
        assert_eq!(edits.buffers[&b].rope.to_string(), "BAR in b\n");
    }

    #[test]
    fn undo_global_scrolls_offscreen_affected_hit_into_view() {
        // Set up a file with many hits, replace a mid-tree row,
        // scroll far past it so the row is off-screen, then undo.
        // Selection should move back to the hit and scroll_offset
        // should drop to ~stream_row - tree_visible/3.
        use super::super::undo::undo_global;
        use led_state_buffer_edits::{BufferEdits, EditedBuffer};
        use led_state_file_search::{FileSearchGroup, FileSearchHit};

        let path = led_core::UserPath::new("/tmp/a.rs").canonicalize();
        let mut edits = BufferEdits::default();
        let sg = edits.seq_gen.clone();
        // 10-line buffer with "foo" on each line.
        let mut body = String::new();
        for _ in 0..10 {
            body.push_str("foo\n");
        }
        edits.buffers.insert(
            path.clone(),
            EditedBuffer::fresh_with_seq_gen(
                std::sync::Arc::new(ropey::Rope::from_str(&body)),
                sg,
            ),
        );

        let mut state = FileSearchState::default();
        state.query.set("foo");
        state.replace.set("BAR");
        state.replace_mode = true;
        let mut hits = Vec::new();
        for i in 0..10 {
            hits.push(FileSearchHit {
                path: path.clone(),
                line: i + 1,
                col: 1,
                preview: "foo".into(),
                match_start: 0,
                match_end: 3,
            });
        }
        state.results = vec![FileSearchGroup {
            path: path.clone(),
            relative: "a.rs".into(),
            hits: hits.clone(),
        }];
        state.flat_hits = hits;
        state.hit_replacements = vec![None; state.flat_hits.len()];
        state.selection = FileSearchSelection::Result(2);

        let mut tabs = Tabs::default();
        // Right on hit 2 (line 3). Stream index for hit 2 = 3
        // (1 group header + 2 hits + this one).
        replace_selected(&mut state, &mut tabs, &mut edits);

        // Scroll far past it so the row is off-screen.
        state.scroll_offset = 8;

        // body_rows = 6 → 3 input rows (header + query +
        // replace) → 3 tree rows visible. Stream 3 < scroll=8
        // → off-screen. Undo triggers scroll-follow.
        undo_global(&mut tabs, &mut edits, Some(&mut state), 0, 6);

        assert_eq!(state.selection, FileSearchSelection::Result(2));
        // tree_visible = 3, third = 1 → stream 3 - 1 = 2.
        assert_eq!(state.scroll_offset, 2);
    }

    #[test]
    fn undo_global_leaves_scroll_alone_when_affected_hit_is_visible() {
        // Same setup, but the hit is already in view when undo
        // fires — scroll_offset shouldn't jump.
        use super::super::undo::undo_global;
        use led_state_buffer_edits::{BufferEdits, EditedBuffer};
        use led_state_file_search::{FileSearchGroup, FileSearchHit};

        let path = led_core::UserPath::new("/tmp/a.rs").canonicalize();
        let mut edits = BufferEdits::default();
        let sg = edits.seq_gen.clone();
        let mut body = String::new();
        for _ in 0..5 {
            body.push_str("foo\n");
        }
        edits.buffers.insert(
            path.clone(),
            EditedBuffer::fresh_with_seq_gen(
                std::sync::Arc::new(ropey::Rope::from_str(&body)),
                sg,
            ),
        );

        let mut state = FileSearchState::default();
        state.query.set("foo");
        state.replace.set("BAR");
        state.replace_mode = true;
        let mut hits = Vec::new();
        for i in 0..5 {
            hits.push(FileSearchHit {
                path: path.clone(),
                line: i + 1,
                col: 1,
                preview: "foo".into(),
                match_start: 0,
                match_end: 3,
            });
        }
        state.results = vec![FileSearchGroup {
            path: path.clone(),
            relative: "a.rs".into(),
            hits: hits.clone(),
        }];
        state.flat_hits = hits;
        state.hit_replacements = vec![None; state.flat_hits.len()];
        state.selection = FileSearchSelection::Result(1);

        let mut tabs = Tabs::default();
        replace_selected(&mut state, &mut tabs, &mut edits);
        // Scroll_offset stays at 0 (nothing scrolled).
        state.scroll_offset = 0;

        // body_rows = 10 → plenty visible; stream idx for hit 1
        // = 2, well within viewport.
        undo_global(&mut tabs, &mut edits, Some(&mut state), 0, 10);

        assert_eq!(state.selection, FileSearchSelection::Result(1));
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn undo_global_respects_floor_and_refuses_pre_overlay_edits() {
        use super::super::undo::undo_global;
        use led_state_buffer_edits::{BufferEdits, EditedBuffer};

        let a = led_core::UserPath::new("/tmp/a.rs").canonicalize();
        let mut edits = BufferEdits::default();
        let sg = edits.seq_gen.clone();
        edits.buffers.insert(
            a.clone(),
            EditedBuffer::fresh_with_seq_gen(
                std::sync::Arc::new(ropey::Rope::from_str("hello\n")),
                sg,
            ),
        );

        // Simulate a pre-overlay edit: type "X" then finalise.
        {
            let eb = edits.buffers.get_mut(&a).unwrap();
            eb.history.record_insert_char(
                0,
                'X',
                led_state_tabs::Cursor::default(),
                led_state_tabs::Cursor::default(),
            );
            eb.history.finalise();
        }

        // Floor = current seq (overlay just opened). Any undo
        // with seq <= floor is refused. Nothing happens.
        let floor = edits
            .seq_gen
            .0
            .load(std::sync::atomic::Ordering::Relaxed);

        let mut tabs = Tabs::default();
        let past_len_before = edits.buffers[&a].history.past_len();
        undo_global(&mut tabs, &mut edits, None, floor, 40);
        assert_eq!(
            edits.buffers[&a].history.past_len(),
            past_len_before,
            "pre-overlay edits must not be undoable from the overlay"
        );
    }
}
