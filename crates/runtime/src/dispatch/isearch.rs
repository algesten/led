//! In-buffer incremental search dispatch (M13).
//!
//! Activation, query editing, advance-to-next-match, accept/abort
//! semantics per `docs/spec/search.md` § "In-buffer isearch".
//!
//! Current scope (M13 stage 1): `InBufferSearch` toggles the
//! overlay on (or re-triggers when already active but the query is
//! empty — legacy last-query recall). Abort closes it. Typing,
//! match-finding, advance-to-next, accept-on-passthrough, and
//! visual highlighting land in subsequent stages.

use led_state_buffer_edits::BufferEdits;
use led_state_isearch::{IsearchMatch, IsearchState};
use led_state_jumps::{JumpListState, JumpPosition};
use led_state_tabs::Tabs;
use ropey::Rope;

use crate::keymap::Command;

use super::DispatchOutcome;

/// `Ctrl-s` handler. Starts a new search if inactive (seeding from
/// the active buffer's current cursor); advances if already active
/// (future stages); recalls `last_query` if active with an empty
/// query (future stages).
pub(super) fn in_buffer_search(
    isearch: &mut Option<IsearchState>,
    tabs: &Tabs,
    edits: &BufferEdits,
) {
    if isearch.is_some() {
        // Stage 2+: advance / wrap / recall last_query. For now,
        // re-triggering `Ctrl-s` while already open is a no-op.
        return;
    }
    let Some(active_id) = tabs.active else {
        return;
    };
    let Some(tab) = tabs.open.iter().find(|t| t.id == active_id) else {
        return;
    };
    // Only activate when the active buffer is materialized — no
    // search target otherwise.
    if !edits.buffers.contains_key(&tab.path) {
        return;
    }
    *isearch = Some(IsearchState::start(
        tab.cursor,
        tab.scroll,
        tab.last_search.clone(),
    ));
}

/// Abort: restore origin cursor/scroll and close the overlay.
/// Stashes the current query into `last_query` so a subsequent
/// `Ctrl-s`-on-empty recalls it.
pub(super) fn deactivate(isearch: &mut Option<IsearchState>, tabs: &mut Tabs) {
    let Some(state) = isearch.take() else {
        return;
    };
    // Restore the active tab to the origin + stash last_search.
    if let Some(active_id) = tabs.active
        && let Some(tab) = tabs.open.iter_mut().find(|t| t.id == active_id)
    {
        tab.cursor = state.origin_cursor;
        tab.scroll = state.origin_scroll;
        stash_last_search(tab, &state);
    }
}

fn stash_last_search(tab: &mut led_state_tabs::Tab, state: &IsearchState) {
    // Non-empty query wins; otherwise keep whatever was recalled.
    if !state.query.text.is_empty() {
        tab.last_search = Some(state.query.text.clone());
    } else if state.last_query.is_some() {
        tab.last_search = state.last_query.clone();
    }
}

/// Accept: keep the current cursor where it is, stash the query
/// into `last_query`, push a JumpRecord for the origin if the
/// cursor moved, and close.
fn accept(
    isearch: &mut Option<IsearchState>,
    tabs: &mut Tabs,
    jumps: &mut JumpListState,
) {
    let Some(state) = isearch.take() else {
        return;
    };
    let Some(active_id) = tabs.active else {
        return;
    };
    let Some(tab) = tabs.open.iter_mut().find(|t| t.id == active_id) else {
        return;
    };
    // If the cursor moved from origin, record the origin in the
    // jump list so Alt-Left returns the user.
    if (tab.cursor.line, tab.cursor.col)
        != (state.origin_cursor.line, state.origin_cursor.col)
    {
        jumps.record(JumpPosition {
            path: tab.path.clone(),
            line: state.origin_cursor.line,
            col: state.origin_cursor.col,
        });
    }
    stash_last_search(tab, &state);
}

/// Overlay dispatch. Returns `Some(Continue)` when isearch fully
/// consumed the command, `None` when the command should fall
/// through to normal dispatch (the "accept on passthrough"
/// semantic from `docs/spec/search.md`).
pub(super) fn run_overlay_command(
    cmd: Command,
    isearch: &mut Option<IsearchState>,
    tabs: &mut Tabs,
    edits: &BufferEdits,
    jumps: &mut JumpListState,
) -> Option<DispatchOutcome> {
    isearch.as_ref()?;
    match cmd {
        Command::InsertChar(c) => {
            append_and_search(isearch, tabs, edits, c);
            Some(DispatchOutcome::Continue)
        }
        Command::DeleteBack => {
            pop_and_search(isearch, tabs, edits);
            Some(DispatchOutcome::Continue)
        }
        Command::InsertNewline => {
            accept(isearch, tabs, jumps);
            Some(DispatchOutcome::Continue)
        }
        Command::Abort => {
            deactivate(isearch, tabs);
            Some(DispatchOutcome::Continue)
        }
        Command::InBufferSearch => {
            advance(isearch, tabs, edits);
            Some(DispatchOutcome::Continue)
        }
        // Everything else: "accept on passthrough". The current
        // match becomes the cursor's home; the command then runs
        // normally in the outer dispatch path. Quit + Suspend pass
        // through without even the accept step — they're system-
        // level actions and the edge case matters less.
        Command::Quit | Command::Suspend => None,
        _ => {
            accept(isearch, tabs, jumps);
            None
        }
    }
}

/// Append `c` to the query; recompute matches; jump the active
/// buffer's cursor to the first match at-or-after the current
/// cursor. No forward match → set `failed`.
fn append_and_search(
    isearch: &mut Option<IsearchState>,
    tabs: &mut Tabs,
    edits: &BufferEdits,
    c: char,
) {
    let state = match isearch.as_mut() {
        Some(s) => s,
        None => return,
    };
    state.query.insert_char(c);
    recompute_and_jump(state, tabs, edits);
}

/// Pop the last query char and re-run matching. If the query
/// becomes empty, restore the cursor + scroll to the origin
/// (the user "undid" back to their starting point).
fn pop_and_search(
    isearch: &mut Option<IsearchState>,
    tabs: &mut Tabs,
    edits: &BufferEdits,
) {
    let state = match isearch.as_mut() {
        Some(s) => s,
        None => return,
    };
    if !state.query.delete_back() {
        // Query was already empty — nothing to do.
        return;
    }
    if state.query.text.is_empty() {
        state.matches.clear();
        state.match_idx = None;
        state.failed = false;
        let origin_cursor = state.origin_cursor;
        let origin_scroll = state.origin_scroll;
        if let Some(active_id) = tabs.active
            && let Some(tab) = tabs.open.iter_mut().find(|t| t.id == active_id)
        {
            tab.cursor = origin_cursor;
            tab.scroll = origin_scroll;
        }
        return;
    }
    recompute_and_jump(state, tabs, edits);
}

/// Second+ `Ctrl-s` while isearch is active.
///
/// - Empty query → recall `last_query` (if any), re-run match find.
/// - `failed == true` → wrap to match index 0, clear `failed`,
///   jump the cursor there.
/// - Otherwise → advance `match_idx` by one; if that walks past
///   the last match, set `failed = true` (the next press wraps).
fn advance(
    isearch: &mut Option<IsearchState>,
    tabs: &mut Tabs,
    edits: &BufferEdits,
) {
    let Some(state) = isearch.as_mut() else {
        return;
    };
    if state.query.text.is_empty() {
        // Recall last_query if present.
        let recall = state.last_query.clone();
        if let Some(q) = recall {
            state.query.set(q);
            recompute_and_jump(state, tabs, edits);
        }
        return;
    }
    if state.failed {
        // Wrap to the first match.
        if state.matches.is_empty() {
            return;
        }
        state.failed = false;
        state.match_idx = Some(0);
        let hit = state.matches[0];
        jump_to_match(state, tabs, edits, hit);
        return;
    }
    // Normal advance.
    let Some(idx) = state.match_idx else {
        // No current selection — act like typing: find first forward.
        recompute_and_jump(state, tabs, edits);
        return;
    };
    if idx + 1 >= state.matches.len() {
        // Past the end — flag failed; next Ctrl-s wraps.
        state.failed = true;
        return;
    }
    let next = idx + 1;
    state.match_idx = Some(next);
    let hit = state.matches[next];
    jump_to_match(state, tabs, edits, hit);
}

/// Move the active tab's cursor onto `hit`. Pure side-effect; the
/// caller already updated `state.match_idx` / `state.failed`.
fn jump_to_match(
    _state: &mut IsearchState,
    tabs: &mut Tabs,
    edits: &BufferEdits,
    hit: IsearchMatch,
) {
    let Some(rope) = active_rope(tabs, edits) else {
        return;
    };
    let (line, col) = char_to_line_col(&rope, hit.char_start);
    if let Some(active_id) = tabs.active
        && let Some(tab) = tabs.open.iter_mut().find(|t| t.id == active_id)
    {
        tab.cursor.line = line;
        tab.cursor.col = col;
        tab.cursor.preferred_col = col;
    }
}

/// Rescan the active buffer's rope for the current query and
/// jump the cursor to the first forward match. Sets `failed =
/// true` when no match at or after the current cursor exists.
fn recompute_and_jump(
    state: &mut IsearchState,
    tabs: &mut Tabs,
    edits: &BufferEdits,
) {
    let rope = match active_rope(tabs, edits) {
        Some(r) => r,
        None => return,
    };
    state.matches = find_all_matches(&rope, &state.query.text);
    state.failed = false;
    let Some(active_id) = tabs.active else {
        return;
    };
    let Some(tab_idx) = tabs.open.iter().position(|t| t.id == active_id) else {
        return;
    };
    let tab = &tabs.open[tab_idx];
    let cursor_char = cursor_to_char(&rope, tab.cursor.line, tab.cursor.col);
    let first_forward = state
        .matches
        .iter()
        .position(|m| m.char_start >= cursor_char);
    match first_forward {
        Some(idx) => {
            state.match_idx = Some(idx);
            let hit = state.matches[idx];
            let (line, col) = char_to_line_col(&rope, hit.char_start);
            let tab = &mut tabs.open[tab_idx];
            tab.cursor.line = line;
            tab.cursor.col = col;
            tab.cursor.preferred_col = col;
        }
        None => {
            state.match_idx = None;
            state.failed = true;
        }
    }
}

/// Case-insensitive substring scan. Walks the rope a char at a
/// time; when `query` is non-empty we compare lowercased chars.
/// Empty query → empty match list (matches legacy — no cursor
/// jump while the query is still being typed from scratch).
fn find_all_matches(rope: &Rope, query: &str) -> Vec<IsearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let qlower: Vec<char> = query.chars().flat_map(|c| c.to_lowercase()).collect();
    let mut out = Vec::new();
    let total = rope.len_chars();
    let qlen = qlower.len();
    if qlen == 0 || qlen > total {
        return out;
    }
    // Linear scan; works for M13's buffer sizes. M14's project
    // search uses ripgrep.
    let mut i = 0;
    while i + qlen <= total {
        let mut ok = true;
        for (j, &qc) in qlower.iter().enumerate() {
            let rc = rope.char(i + j);
            // `to_lowercase` returns an iterator; for M13 we only
            // fold on the first mapped char — matches legacy's
            // byte-level lowercase. Unicode edge cases (German ß,
            // Turkish dotless I) are out-of-scope.
            let rcl = rc.to_lowercase().next().unwrap_or(rc);
            if rcl != qc {
                ok = false;
                break;
            }
        }
        if ok {
            out.push(IsearchMatch {
                char_start: i,
                char_end: i + qlen,
            });
            i += qlen;
        } else {
            i += 1;
        }
    }
    out
}

fn active_rope(tabs: &Tabs, edits: &BufferEdits) -> Option<Rope> {
    let active_id = tabs.active?;
    let tab = tabs.open.iter().find(|t| t.id == active_id)?;
    edits.buffers.get(&tab.path).map(|eb| (*eb.rope).clone())
}

fn cursor_to_char(rope: &Rope, line: usize, col: usize) -> usize {
    let line = line.min(rope.len_lines().saturating_sub(1));
    rope.line_to_char(line) + col
}

fn char_to_line_col(rope: &Rope, ch: usize) -> (usize, usize) {
    let line = rope.char_to_line(ch);
    let col = ch - rope.line_to_char(line);
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::{CanonPath, UserPath};
    use led_state_buffer_edits::EditedBuffer;
    use led_state_tabs::{Cursor, Scroll, Tab, TabId};
    use ropey::Rope;
    use std::sync::Arc;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn tabs_with_active(path: &str) -> Tabs {
        let mut t = Tabs::default();
        t.open.push_back(Tab {
            id: TabId(1),
            path: canon(path),
            cursor: Cursor { line: 2, col: 4, preferred_col: 4 },
            ..Default::default()
        });
        t.active = Some(TabId(1));
        t
    }

    fn edits_with_buffer(path: &str, body: &str) -> BufferEdits {
        let mut e = BufferEdits::default();
        e.buffers.insert(
            canon(path),
            EditedBuffer::fresh(Arc::new(Rope::from_str(body))),
        );
        e
    }

    #[test]
    fn in_buffer_search_activates_and_captures_origin() {
        let tabs = tabs_with_active("/tmp/buf.txt");
        let edits = edits_with_buffer("/tmp/buf.txt", "hello\nworld\n");
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        let s = isearch.expect("activated");
        assert_eq!(s.origin_cursor.line, 2);
        assert_eq!(s.origin_cursor.col, 4);
        assert_eq!(s.query.text, "");
    }

    #[test]
    fn in_buffer_search_is_noop_without_active_tab() {
        let tabs = Tabs::default();
        let edits = BufferEdits::default();
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        assert!(isearch.is_none());
    }

    #[test]
    fn in_buffer_search_is_noop_when_buffer_not_materialized() {
        let tabs = tabs_with_active("/tmp/pending.txt");
        let edits = BufferEdits::default();
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        assert!(isearch.is_none());
    }

    #[test]
    fn find_all_matches_case_insensitive() {
        let rope = Rope::from_str("Foo fooBAR quux Foo\n");
        let ms = find_all_matches(&rope, "foo");
        assert_eq!(ms.len(), 3);
        assert_eq!(ms[0], IsearchMatch { char_start: 0, char_end: 3 });
        assert_eq!(ms[1], IsearchMatch { char_start: 4, char_end: 7 });
        assert_eq!(ms[2], IsearchMatch { char_start: 16, char_end: 19 });
    }

    #[test]
    fn find_all_matches_empty_query_is_empty() {
        let rope = Rope::from_str("hello\n");
        assert!(find_all_matches(&rope, "").is_empty());
    }

    #[test]
    fn typing_advances_cursor_to_first_forward_match() {
        let mut tabs = tabs_with_active("/tmp/buf.txt");
        // Start at line 0 col 0.
        tabs.open[0].cursor = Cursor::default();
        let edits = edits_with_buffer(
            "/tmp/buf.txt",
            "alpha beta gamma\ndelta echo foxtrot\nalpha zulu yankee\n",
        );
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        let mut jumps = JumpListState::default();
        // Type 'a' 'l' 'p' 'h' 'a' → query = "alpha", first match is at 0.
        for c in "alpha".chars() {
            run_overlay_command(
                Command::InsertChar(c),
                &mut isearch,
                &mut tabs,
                &edits,
                &mut jumps,
            );
        }
        let s = isearch.as_ref().unwrap();
        assert_eq!(s.query.text, "alpha");
        assert_eq!(s.matches.len(), 2);
        assert_eq!(s.match_idx, Some(0));
        assert!(!s.failed);
        // Cursor jumped to match 0, which is already at (0, 0).
        assert_eq!(tabs.open[0].cursor.line, 0);
        assert_eq!(tabs.open[0].cursor.col, 0);
    }

    #[test]
    fn no_forward_match_sets_failed() {
        let mut tabs = tabs_with_active("/tmp/buf.txt");
        // Cursor past the only match.
        tabs.open[0].cursor = Cursor { line: 1, col: 0, preferred_col: 0 };
        let edits = edits_with_buffer("/tmp/buf.txt", "alpha\nbeta\n");
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        let mut jumps = JumpListState::default();
        run_overlay_command(
            Command::InsertChar('a'),
            &mut isearch,
            &mut tabs,
            &edits,
            &mut jumps,
        );
        let s = isearch.as_ref().unwrap();
        // "a" has two matches in "alpha" + one in "beta" (b-e-t-a) at
        // char 9. First forward from line 1 col 0 is "a" in "beta" (9).
        // The cursor at line 1 col 0 → char index = 6, so first match
        // with char_start >= 6 is 9. Not failed.
        assert!(!s.failed);
        assert!(s.match_idx.is_some());

        // Now type 'q' (not in buffer) → no matches at all → failed.
        run_overlay_command(
            Command::InsertChar('q'),
            &mut isearch,
            &mut tabs,
            &edits,
            &mut jumps,
        );
        let s = isearch.as_ref().unwrap();
        assert!(s.failed);
        assert!(s.matches.is_empty());
    }

    #[test]
    fn backspace_to_empty_restores_origin() {
        let mut tabs = tabs_with_active("/tmp/buf.txt");
        let origin = Cursor { line: 0, col: 0, preferred_col: 0 };
        tabs.open[0].cursor = origin;
        let edits = edits_with_buffer("/tmp/buf.txt", "alpha beta\n");
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        let mut jumps = JumpListState::default();
        run_overlay_command(Command::InsertChar('b'), &mut isearch, &mut tabs, &edits, &mut jumps);
        // 'b' is at char 6 (line 0 col 6).
        assert_eq!(tabs.open[0].cursor.col, 6);
        run_overlay_command(Command::DeleteBack, &mut isearch, &mut tabs, &edits, &mut jumps);
        // Query empty → cursor back at origin.
        assert_eq!(tabs.open[0].cursor, origin);
    }

    #[test]
    fn enter_accepts_and_pushes_jump_record_when_cursor_moved() {
        let mut tabs = tabs_with_active("/tmp/buf.txt");
        tabs.open[0].cursor = Cursor::default();
        let edits = edits_with_buffer("/tmp/buf.txt", "alpha beta\n");
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        let mut jumps = JumpListState::default();
        run_overlay_command(Command::InsertChar('b'), &mut isearch, &mut tabs, &edits, &mut jumps);
        run_overlay_command(
            Command::InsertNewline,
            &mut isearch,
            &mut tabs,
            &edits,
            &mut jumps,
        );
        assert!(isearch.is_none());
        // Cursor stays where the match put it.
        assert_eq!(tabs.open[0].cursor.col, 6);
        // JumpList recorded the origin (0, 0).
        assert!(jumps.can_back());
    }

    #[test]
    fn esc_restores_origin_cursor_and_scroll() {
        let mut tabs = tabs_with_active("/tmp/buf.txt");
        let origin_cursor = Cursor { line: 0, col: 3, preferred_col: 3 };
        let origin_scroll = Scroll { top: 0, top_sub_line: led_core::SubLine(0) };
        tabs.open[0].cursor = origin_cursor;
        tabs.open[0].scroll = origin_scroll;
        let edits = edits_with_buffer("/tmp/buf.txt", "alpha beta\n");
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        let mut jumps = JumpListState::default();
        run_overlay_command(Command::InsertChar('b'), &mut isearch, &mut tabs, &edits, &mut jumps);
        assert_ne!(tabs.open[0].cursor, origin_cursor);
        run_overlay_command(Command::Abort, &mut isearch, &mut tabs, &edits, &mut jumps);
        assert!(isearch.is_none());
        assert_eq!(tabs.open[0].cursor, origin_cursor);
        assert_eq!(tabs.open[0].scroll, origin_scroll);
    }

    #[test]
    fn ctrl_s_advances_to_next_match() {
        let mut tabs = tabs_with_active("/tmp/buf.txt");
        tabs.open[0].cursor = Cursor::default();
        let edits = edits_with_buffer(
            "/tmp/buf.txt",
            "alpha\nbeta\nalpha\n",
        );
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        let mut jumps = JumpListState::default();
        for c in "alpha".chars() {
            run_overlay_command(
                Command::InsertChar(c),
                &mut isearch,
                &mut tabs,
                &edits,
                &mut jumps,
            );
        }
        // Starts on match 0 (line 0).
        assert_eq!(isearch.as_ref().unwrap().match_idx, Some(0));
        // Ctrl-s: advance to match 1 (line 2).
        run_overlay_command(
            Command::InBufferSearch,
            &mut isearch,
            &mut tabs,
            &edits,
            &mut jumps,
        );
        let s = isearch.as_ref().unwrap();
        assert_eq!(s.match_idx, Some(1));
        assert_eq!(tabs.open[0].cursor.line, 2);
    }

    #[test]
    fn ctrl_s_past_last_sets_failed_then_wraps() {
        let mut tabs = tabs_with_active("/tmp/buf.txt");
        tabs.open[0].cursor = Cursor::default();
        let edits = edits_with_buffer("/tmp/buf.txt", "alpha\nalpha\n");
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        let mut jumps = JumpListState::default();
        for c in "alpha".chars() {
            run_overlay_command(
                Command::InsertChar(c),
                &mut isearch,
                &mut tabs,
                &edits,
                &mut jumps,
            );
        }
        // Advance to match 1 (line 1).
        run_overlay_command(
            Command::InBufferSearch,
            &mut isearch,
            &mut tabs,
            &edits,
            &mut jumps,
        );
        assert_eq!(isearch.as_ref().unwrap().match_idx, Some(1));
        // Advance again → past end → failed flag.
        run_overlay_command(
            Command::InBufferSearch,
            &mut isearch,
            &mut tabs,
            &edits,
            &mut jumps,
        );
        assert!(isearch.as_ref().unwrap().failed);
        // Third press wraps to match 0 and clears failed.
        run_overlay_command(
            Command::InBufferSearch,
            &mut isearch,
            &mut tabs,
            &edits,
            &mut jumps,
        );
        let s = isearch.as_ref().unwrap();
        assert_eq!(s.match_idx, Some(0));
        assert!(!s.failed);
        assert_eq!(tabs.open[0].cursor.line, 0);
    }

    #[test]
    fn ctrl_s_on_empty_query_recalls_last_search() {
        let mut tabs = tabs_with_active("/tmp/buf.txt");
        tabs.open[0].cursor = Cursor::default();
        tabs.open[0].last_search = Some("beta".into());
        let edits = edits_with_buffer("/tmp/buf.txt", "alpha beta gamma\n");
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        let mut jumps = JumpListState::default();
        // Ctrl-s on the (empty) query should pull in "beta".
        run_overlay_command(
            Command::InBufferSearch,
            &mut isearch,
            &mut tabs,
            &edits,
            &mut jumps,
        );
        let s = isearch.as_ref().unwrap();
        assert_eq!(s.query.text, "beta");
        assert_eq!(s.match_idx, Some(0));
        // Cursor on 'b' of "beta" — char index 6.
        assert_eq!(tabs.open[0].cursor.col, 6);
    }

    #[test]
    fn accept_stashes_query_into_tab_last_search() {
        let mut tabs = tabs_with_active("/tmp/buf.txt");
        tabs.open[0].cursor = Cursor::default();
        let edits = edits_with_buffer("/tmp/buf.txt", "alpha\n");
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        let mut jumps = JumpListState::default();
        for c in "alpha".chars() {
            run_overlay_command(
                Command::InsertChar(c),
                &mut isearch,
                &mut tabs,
                &edits,
                &mut jumps,
            );
        }
        run_overlay_command(
            Command::InsertNewline,
            &mut isearch,
            &mut tabs,
            &edits,
            &mut jumps,
        );
        assert_eq!(tabs.open[0].last_search.as_deref(), Some("alpha"));
    }

    #[test]
    fn deactivate_clears_state() {
        let tabs = tabs_with_active("/tmp/x.txt");
        let edits = edits_with_buffer("/tmp/x.txt", "x");
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        assert!(isearch.is_some());
        let mut tabs = Tabs::default();
        deactivate(&mut isearch, &mut tabs);
        assert!(isearch.is_none());
    }
}
