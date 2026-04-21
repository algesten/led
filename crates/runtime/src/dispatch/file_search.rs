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
) {
    if file_search.is_some() {
        // Already open — Ctrl+F a second time is a no-op.
        return;
    }
    let mut state = FileSearchState::default();
    state.previous_tab = tabs.active;
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
) -> Option<DispatchOutcome> {
    file_search.as_ref()?;
    match cmd {
        Command::InsertChar(c) => {
            let state = file_search.as_mut()?;
            input_for_selection(state).insert_char(c);
            state.queue_search();
        }
        Command::DeleteBack => {
            let state = file_search.as_mut()?;
            input_for_selection(state).delete_back();
            state.queue_search();
        }
        Command::DeleteForward => {
            let state = file_search.as_mut()?;
            input_for_selection(state).delete_forward();
            state.queue_search();
        }
        Command::CursorLeft => {
            input_for_selection(file_search.as_mut()?).move_left();
        }
        Command::CursorRight => {
            input_for_selection(file_search.as_mut()?).move_right();
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
            state.queue_search();
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
            handle_enter(file_search.as_mut()?, tabs, edits);
        }
        Command::Abort | Command::CloseFileSearch => {
            deactivate(file_search, browser, tabs);
        }
        Command::ReplaceAll => {
            let state = file_search.as_mut()?;
            if state.replace_mode {
                apply_replace_all(state, edits);
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

/// `Enter` behaviour. On any row, jump-preview the currently
/// selected hit (or the first hit when the selection sits on an
/// input row). The overlay stays open so the user can keep scanning.
fn handle_enter(
    state: &mut FileSearchState,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
) {
    if state.flat_hits.is_empty() {
        return;
    }
    let idx = match state.selection {
        FileSearchSelection::Result(i) if i < state.flat_hits.len() => i,
        _ => 0,
    };
    state.selection = FileSearchSelection::Result(idx);
    let hit = state.flat_hits[idx].clone();
    jump_preview(&hit, tabs, edits);
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
    side_rows: usize,
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
    clamp_scroll_to_selection(state, side_rows);
    if let FileSearchSelection::Result(i) = state.selection
        && let Some(hit) = state.flat_hits.get(i).cloned()
    {
        jump_preview(&hit, tabs, edits);
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

/// `Alt+Enter` — apply the replace across every hit's buffer. Only
/// fires when `replace_mode` is on (the caller gates this). For
/// each file with hits that's currently loaded in `edits.buffers`,
/// we rebuild the rope via `regex.replace_all` over the existing
/// text and bump its version so `dirty()` goes true. On-disk replace
/// for files that aren't open yet is a later follow-up — the
/// typical flow opens buffers via arrow-scan first.
fn apply_replace_all(
    state: &led_state_file_search::FileSearchState,
    edits: &mut led_state_buffer_edits::BufferEdits,
) {
    if state.results.is_empty() || state.query.text.is_empty() {
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
    for group in &state.results {
        let Some(eb) = edits.buffers.get_mut(&group.path) else {
            continue;
        };
        let existing = eb.rope.to_string();
        let replaced = re.replace_all(&existing, replacement);
        if replaced.as_ref() != existing {
            super::shared::bump(eb, ropey::Rope::from_str(replaced.as_ref()));
        }
    }
}

/// Open (or focus) the hit's file as a preview tab and position the
/// cursor on the match. `open_or_focus_tab(promote=false)` re-uses an
/// existing tab for the same path, otherwise creates a preview. The
/// cursor goes to 0-indexed `(line-1, col-1)` because ripgrep
/// positions are 1-indexed.
fn jump_preview(hit: &FileSearchHit, tabs: &mut Tabs, edits: &BufferEdits) {
    open_or_focus_tab(tabs, &hit.path, false);
    let Some(active_id) = tabs.active else { return };
    let Some(idx) = tabs.open.iter().position(|t| t.id == active_id) else {
        return;
    };
    // Preview lands the cursor at the start of the hit's line — not
    // at the match column — matching legacy. The user arrows / types
    // to explore from there; the match column only matters for the
    // replace flow (stage 7).
    let line = hit.line.saturating_sub(1);
    let tab = &mut tabs.open[idx];
    tab.cursor = Cursor {
        line,
        col: 0,
        preferred_col: 0,
    };
    tab.scroll.top = line;
    let _ = edits;
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
        activate(&mut fs, &mut browser, &tabs);
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
        activate(&mut fs, &mut browser, &tabs);
        let first = fs.clone();
        activate(&mut fs, &mut browser, &tabs);
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
        activate(&mut fs, &mut browser, &tabs);
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
}
