//! Low-level helpers shared across the dispatch submodules.
//!
//! Kept `pub(super)` so the sibling modules (`cursor`, `edit`, `kill`,
//! `undo`, …) can call them without re-exporting. Nothing here is part
//! of the dispatch public API.

use led_core::{CanonPath, char_to_grapheme_col, col_to_sub_line, grapheme_col_to_char};
use led_driver_terminal_core::{Layout, Terminal};
use led_state_browser::BrowserUi;
use led_state_buffer_edits::{BufferEdits, EditedBuffer};
use led_state_tabs::{Cursor, Tab, TabId, Tabs};
use ropey::Rope;
use std::sync::Arc;

/// Gutter column reserved by the painter before the first char of
/// buffer content. Matches [`crate::query::GUTTER_WIDTH`].
pub(super) const GUTTER_WIDTH: usize = 2;

/// Trailing column the painter never writes to. Matches
/// [`crate::query::TRAILING_RESERVED_COLS`]. Held at `0` now
/// that the terminal-driver paint path uses a cell-grid diff
/// (see `driver-terminal/native/src/{buffer,render}.rs`) — the
/// renderer never emits `Clear(UntilNewLine)`, so the last col
/// is safe to write and dispatch's `content_cols` derivation
/// should match.
pub(super) const TRAILING_RESERVED_COLS: usize = 0;

/// Editor body content width in chars — the same `content_cols`
/// the painter uses for sub-line geometry. Terminal dims absent
/// → 0 (callers treat as "no wrap / no-op").
pub fn editor_content_cols(terminal: &Terminal, browser: &BrowserUi) -> usize {
    terminal
        .dims
        .map(|d| {
            let layout = Layout::compute(d, browser.visible);
            (layout.editor_area.cols as usize)
                .saturating_sub(GUTTER_WIDTH)
                .saturating_sub(TRAILING_RESERVED_COLS)
        })
        .unwrap_or(0)
}

/// Allocate the next unused `TabId` by scanning `tabs.open`. Dispatch
/// doesn't hold the runtime's `TabIdGen` (that lives on the main
/// stack frame), so each submodule that needs a new tab derives one
/// locally. Ids are monotonic per-session, never reused.
pub(super) fn next_tab_id(tabs: &Tabs) -> TabId {
    let max = tabs.open.iter().map(|t| t.id.0).max().unwrap_or(0);
    TabId(max + 1)
}

/// Open (or focus) a file tab at `path`.
///
/// - **Existing tab at this path**: activate it; if `promote` is true,
///   clear its preview flag so it becomes a pinned tab.
/// - **Preview tab exists**: replace its path. Promote vs keep-preview
///   per the flag; reset cursor/scroll/mark (new buffer, fresh state).
///   `previous_tab` is preserved across replacements — matches legacy's
///   `set_preview`: the FIRST preview captures the restore target,
///   subsequent previews inherit it.
/// - **No preview**: create a fresh tab — preview if `!promote`, real
///   otherwise. On a new preview, `previous_tab` is seeded from the
///   current `tabs.active` so `close_preview` can restore on Abort or
///   directory-nav.
///
/// Shared between M11 browser (`open_selected` / `open_selected_bg`)
/// and M12 find-file commit.
pub fn open_or_focus_tab(tabs: &mut Tabs, path: &CanonPath, promote: bool) {
    if let Some(idx) = tabs.open.iter().position(|t| &t.path == path) {
        let id = tabs.open[idx].id;
        tabs.active = Some(id);
        if promote {
            tabs.open[idx].preview = false;
            tabs.open[idx].previous_tab = None;
        }
        return;
    }
    if let Some(idx) = tabs.open.iter().position(|t| t.preview) {
        let previous = tabs.open[idx].previous_tab;
        let id = tabs.open[idx].id;
        tabs.open[idx].path = path.clone();
        tabs.open[idx].preview = !promote;
        tabs.open[idx].cursor = Default::default();
        tabs.open[idx].scroll = Default::default();
        tabs.open[idx].mark = None;
        tabs.open[idx].previous_tab = if promote { None } else { previous };
        tabs.active = Some(id);
        return;
    }
    let id = next_tab_id(tabs);
    // Capture the active tab as the restore target for the new
    // preview. Real tabs don't need a restore target (they don't
    // get implicitly closed).
    let previous_tab = if promote { None } else { tabs.active };
    tabs.open.push_back(Tab {
        id,
        path: path.clone(),
        preview: !promote,
        previous_tab,
        ..Default::default()
    });
    tabs.active = Some(id);
}

/// Remove the (single) preview tab if one exists. Restores
/// `previous_tab` as the active tab when that tab still exists;
/// otherwise falls back to the last remaining tab (or `None` if
/// none remain). Matches legacy `close_preview`
/// (`/led/led/src/model/action/preview.rs:100`).
pub fn close_preview(tabs: &mut Tabs) {
    let Some(idx) = tabs.open.iter().position(|t| t.preview) else {
        return;
    };
    let restore = tabs.open[idx].previous_tab;
    tabs.open.remove(idx);
    if let Some(target) = restore
        && tabs.open.iter().any(|t| t.id == target)
    {
        tabs.active = Some(target);
        return;
    }
    tabs.active = tabs.open.back().map(|t| t.id);
}

/// Access the active tab and its edited buffer together. Bails if
/// either is missing — buffer not yet loaded means edit keys no-op.
pub(super) fn with_active<F>(tabs: &mut Tabs, edits: &mut BufferEdits, f: F)
where
    F: FnOnce(&mut Tab, &mut EditedBuffer),
{
    let Some(id) = tabs.active else {
        return;
    };
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    let tab = &mut tabs.open[idx];
    let Some(eb) = edits.buffers.get_mut(&tab.path) else {
        return;
    };
    f(tab, eb);
}

/// Replace the buffer's rope with a new one and bump `version`.
/// `saved_version` is untouched — `dirty()` derives as `version >
/// saved_version`.
pub(super) fn bump(eb: &mut EditedBuffer, new_rope: Rope) {
    eb.rope = Arc::new(new_rope);
    eb.version.0 = eb.version.0.saturating_add(1);
}

/// `Cursor { line, col }` → absolute char index in the rope, with
/// bounds clamping (out-of-range lines / grapheme cols are pulled
/// in). `c.col` is a grapheme-cluster index per M25; this helper
/// is the single boundary where dispatch consumers translate to
/// rope-friendly char indices.
pub(super) fn cursor_to_char(c: &Cursor, rope: &Rope) -> usize {
    let line_count = rope.len_lines().max(1);
    let line = c.line.min(line_count - 1);
    let line_grapheme_count = line_grapheme_len(rope, line);
    let col = c.col.min(line_grapheme_count);
    let slice = rope.line(line);
    rope.line_to_char(line) + grapheme_col_to_char(slice, col)
}

/// Absolute char index → `Cursor`. `col` is set to the grapheme col
/// containing `ch`. `preferred_col` is set to match `col`; callers
/// that care about visual semantics run [`refresh_preferred_col`]
/// afterward to convert it to the within-sub-line cell column.
pub(super) fn char_to_cursor(ch: usize, rope: &Rope) -> Cursor {
    let line = rope.char_to_line(ch);
    let line_char = rope.line_to_char(line);
    let slice = rope.line(line);
    let col = char_to_grapheme_col(slice, ch - line_char);
    Cursor {
        line,
        col,
        preferred_col: col,
    }
}

/// Recompute `preferred_col` as the within-sub-line **display cell**
/// column for the cursor's current `(line, col)` under the given
/// `content_cols`. Dispatch calls this at the top-level boundary
/// after any command that may have moved the cursor via an edit /
/// kill / undo / redo / paste. Without it, edit primitives leave
/// `preferred_col` as the raw logical col, which on a wrapped line
/// exceeds any sub-line's width and pins subsequent vertical moves
/// to whatever each sub-line clamps to — presenting as "arrow-up
/// gets stuck" when the line re-wraps.
pub(super) fn refresh_preferred_col(cursor: &mut Cursor, rope: &Rope, content_cols: usize) {
    if cursor.line >= rope.len_lines() {
        cursor.preferred_col = 0;
        return;
    }
    let slice = rope.line(cursor.line);
    let (_, within_cells) = col_to_sub_line(cursor.col, slice, content_cols);
    cursor.preferred_col = within_cells;
}

/// Character count of a buffer line, stripped of trailing `\n` /
/// `\r\n`. Out-of-range lines yield 0.
///
/// Used wherever code needs to slice the rope by char index. For
/// **cursor**-bounded math (col clamps, end-of-line position),
/// reach for [`line_grapheme_len`] instead.
pub(super) fn line_char_len(rope: &Rope, line: usize) -> usize {
    if line >= rope.len_lines() {
        return 0;
    }
    let slice = rope.line(line);
    let mut end = slice.len_chars();
    if end == 0 {
        return 0;
    }
    if slice.char(end - 1) == '\n' {
        end -= 1;
        if end > 0 && slice.char(end - 1) == '\r' {
            end -= 1;
        }
    }
    end
}

/// Grapheme-cluster count of a buffer line, stripped of trailing
/// `\n` / `\r\n`. Use this when measuring cursor positions
/// (`Cursor::col` clamping, end-of-line, line-bound visibility).
pub(super) fn line_grapheme_len(rope: &Rope, line: usize) -> usize {
    if line >= rope.len_lines() {
        return 0;
    }
    led_core::line_grapheme_len(rope.line(line))
}

pub(super) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

pub(super) fn rope_char_at(rope: &Rope, line: usize, col: usize) -> char {
    let base = rope.line_to_char(line);
    rope.char(base + col)
}
