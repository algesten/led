//! Low-level helpers shared across the dispatch submodules.
//!
//! Kept `pub(super)` so the sibling modules (`cursor`, `edit`, `kill`,
//! `undo`, …) can call them without re-exporting. Nothing here is part
//! of the dispatch public API.

use led_core::CanonPath;
use led_state_buffer_edits::{BufferEdits, EditedBuffer};
use led_state_tabs::{Cursor, Tab, TabId, Tabs};
use ropey::Rope;
use std::sync::Arc;

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
/// - **No preview**: create a fresh tab — preview if `!promote`, real
///   otherwise.
///
/// Shared between M11 browser (`open_selected` / `open_selected_bg`)
/// and M12 find-file commit.
pub(super) fn open_or_focus_tab(tabs: &mut Tabs, path: &CanonPath, promote: bool) {
    if let Some(idx) = tabs.open.iter().position(|t| &t.path == path) {
        let id = tabs.open[idx].id;
        tabs.active = Some(id);
        if promote {
            tabs.open[idx].preview = false;
        }
        return;
    }
    if let Some(idx) = tabs.open.iter().position(|t| t.preview) {
        let id = tabs.open[idx].id;
        tabs.open[idx].path = path.clone();
        tabs.open[idx].preview = !promote;
        tabs.open[idx].cursor = Default::default();
        tabs.open[idx].scroll = Default::default();
        tabs.open[idx].mark = None;
        tabs.active = Some(id);
        return;
    }
    let id = next_tab_id(tabs);
    tabs.open.push_back(Tab {
        id,
        path: path.clone(),
        preview: !promote,
        ..Default::default()
    });
    tabs.active = Some(id);
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
    eb.version = eb.version.saturating_add(1);
}

/// `Cursor { line, col }` → absolute char index in the rope, with
/// bounds clamping (out-of-range lines / cols are pulled in).
pub(super) fn cursor_to_char(c: &Cursor, rope: &Rope) -> usize {
    let line_count = rope.len_lines().max(1);
    let line = c.line.min(line_count - 1);
    let line_len = line_char_len(rope, line);
    let col = c.col.min(line_len);
    rope.line_to_char(line) + col
}

/// Absolute char index → `Cursor` with `preferred_col` anchored to
/// the resulting column.
pub(super) fn char_to_cursor(ch: usize, rope: &Rope) -> Cursor {
    let line = rope.char_to_line(ch);
    let col = ch - rope.line_to_char(line);
    Cursor {
        line,
        col,
        preferred_col: col,
    }
}

/// Character count of a buffer line, stripped of trailing `\n` /
/// `\r\n`. Out-of-range lines yield 0.
///
/// Walks the rope directly — no intermediate `String` allocation.
/// Called on every cursor keystroke, so this needs to stay cheap.
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

pub(super) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

pub(super) fn rope_char_at(rope: &Rope, line: usize, col: usize) -> char {
    let base = rope.line_to_char(line);
    rope.char(base + col)
}
