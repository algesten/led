use led_state_browser::BrowserUi;
use led_state_buffer_edits::BufferEdits;
use led_state_file_search::{FileSearchHit, FileSearchSelection, FileSearchState};
use led_state_tabs::{Cursor, Tabs};

use super::super::shared::open_or_focus_tab;
use super::{clamp_scroll_to_selection, deactivate, jump_preview, preview_scroll_top};

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
pub(super) fn handle_enter(
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
pub(super) fn move_selection(
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
