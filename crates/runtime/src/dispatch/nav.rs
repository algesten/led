//! Jump list + match-bracket (M10) + tiered issue navigation (M20a).
//!
//! Primitives:
//! - [`match_bracket`] — scan forward (open) or backward (close) for
//!   the matching bracket, move the cursor there, and record the
//!   pre-jump position on the jump list.
//! - [`jump_back`] — step one entry back in the jump list, restoring
//!   cursor and activating the target tab. From head, saves the
//!   current position first.
//! - [`jump_forward`] — the mirror.
//! - [`next_issue_active`] / [`prev_issue_active`] — walk the
//!   `IssueCategory::NAV_LEVELS` tier ladder to find the next / prev
//!   issue (LSP error, LSP warning, git unstaged, …) and jump there.
//!   Stays inside the first non-empty tier so an error-rich file
//!   doesn't teleport to a stray warning.
//!
//! All are silent no-ops when there's no active tab, no buffer
//! loaded, or (for navigation) nothing to do.

use std::time::{Duration, Instant};

use led_core::{CanonPath, IssueCategory};
use led_driver_terminal_core::Terminal;
use led_state_alerts::AlertState;
use led_state_browser::BrowserUi;
use led_state_buffer_edits::BufferEdits;
use led_state_diagnostics::{DiagnosticSeverity, DiagnosticsStates};
use led_state_git::GitState;
use led_state_jumps::{JumpListState, JumpPosition};
use led_state_tabs::Tabs;

use super::cursor::center_on_cursor;
use super::shared::{char_to_cursor, cursor_to_char, editor_content_cols, line_char_len};

/// Jump to the matching bracket for the character at (or
/// immediately before) the cursor. No-op if no bracket is in
/// scope or no match exists.
pub(super) fn match_bracket(tabs: &mut Tabs, edits: &BufferEdits, jumps: &mut JumpListState) {
    let Some(id) = tabs.active else {
        return;
    };
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    let tab = &tabs.open[idx];
    let Some(eb) = edits.buffers.get(&tab.path) else {
        return;
    };
    let rope = &eb.rope;
    let pos = cursor_to_char(&tab.cursor, rope);

    // Try char AT cursor first, then char BEFORE.
    let mut target: Option<usize> = None;
    if pos < rope.len_chars() {
        target = find_match(rope, pos);
    }
    if target.is_none() && pos > 0 {
        target = find_match(rope, pos - 1);
    }
    let Some(to) = target else {
        return;
    };

    // Record the pre-jump position so Alt-b round-trips.
    let current = JumpPosition {
        path: tab.path.clone(),
        line: tab.cursor.line,
        col: tab.cursor.col,
    };
    jumps.record(current);

    let tab = &mut tabs.open[idx];
    tab.cursor = char_to_cursor(to, rope);
    tab.cursor.preferred_col = tab.cursor.col;
}

/// Step back to the previous jump-list entry. From head, auto-records
/// the current position so `jump_forward` can return.
pub(super) fn jump_back(tabs: &mut Tabs, edits: &BufferEdits, jumps: &mut JumpListState) {
    if !jumps.can_back() {
        return;
    }
    let current = match current_position(tabs) {
        Some(p) => p,
        None => return,
    };
    let Some(target) = jumps.step_back(current) else {
        return;
    };
    apply_jump(tabs, edits, target);
}

/// Step forward to the next jump-list entry.
pub(super) fn jump_forward(tabs: &mut Tabs, edits: &BufferEdits, jumps: &mut JumpListState) {
    let Some(target) = jumps.step_forward() else {
        return;
    };
    apply_jump(tabs, edits, target);
}

/// Snapshot of the active tab's position for recording onto the
/// jump list. Returns None when there's no active tab.
pub(super) fn current_position(tabs: &Tabs) -> Option<JumpPosition> {
    let id = tabs.active?;
    let tab = tabs.open.iter().find(|t| t.id == id)?;
    Some(JumpPosition {
        path: tab.path.clone(),
        line: tab.cursor.line,
        col: tab.cursor.col,
    })
}

/// Resolve a [`JumpPosition`] against the current tab set + buffers.
/// - If the path is an open tab AND its buffer is loaded, activate
///   it and restore the cursor (clamped to the buffer's extent).
/// - Otherwise silent no-op — M11 / M12 / M21 will add proper
///   re-open logic.
fn apply_jump(tabs: &mut Tabs, edits: &BufferEdits, pos: JumpPosition) {
    let Some(idx) = tabs.open.iter().position(|t| t.path == pos.path) else {
        return;
    };
    let Some(eb) = edits.buffers.get(&pos.path) else {
        return;
    };
    let rope = &eb.rope;
    let line_count = rope.len_lines();
    let line = pos.line.min(line_count.saturating_sub(1));
    let col = pos.col.min(line_char_len(rope, line));

    let tab = &mut tabs.open[idx];
    tab.cursor.line = line;
    tab.cursor.col = col;
    tab.cursor.preferred_col = col;
    tabs.active = Some(tab.id);
}

/// Scan for the bracket matching the char at `at`. Returns the char
/// index of the match, or None if the char isn't a bracket or no
/// match exists in-buffer.
///
/// Naïve depth-counted scan — doesn't skip brackets inside strings
/// or comments. M15 (syntax highlighting) may swap this for a
/// tree-sitter pair query.
fn find_match(rope: &ropey::Rope, at: usize) -> Option<usize> {
    let c = rope.char(at);
    let (open, close, forward) = match c {
        '(' => ('(', ')', true),
        ')' => ('(', ')', false),
        '[' => ('[', ']', true),
        ']' => ('[', ']', false),
        '{' => ('{', '}', true),
        '}' => ('{', '}', false),
        _ => return None,
    };
    let len = rope.len_chars();
    let mut depth: usize = 1;
    if forward {
        for i in (at + 1)..len {
            let ch = rope.char(i);
            if ch == open {
                depth += 1;
            } else if ch == close {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    } else {
        for i in (0..at).rev() {
            let ch = rope.char(i);
            if ch == close {
                depth += 1;
            } else if ch == open {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    }
    None
}

// ── Issue navigation (M20a) ─────────────────────────────────────────

/// One navigable position picked out of the `collect_positions`
/// pool. Sorted by `(path, row, col)` in the caller; `category`
/// rides along so the alert can say "Jumped to Error …" etc.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pos {
    path: CanonPath,
    row: usize,
    col: usize,
    category: IssueCategory,
}

/// Pure result of computing where to jump to. Mirrors legacy's
/// `NavOutcome`.
#[derive(Clone, Debug)]
struct NavOutcome {
    target_path: CanonPath,
    target_row: usize,
    target_col: usize,
    category: IssueCategory,
    /// 1-based index in the cycle — the "X" in "X/N".
    position: usize,
    /// Total items at the chosen tier level.
    total: usize,
}

/// Walk [`IssueCategory::NAV_LEVELS`] in order; first level with
/// any matching positions wins the cycle. Returns `None` when no
/// level has positions.
fn compute_navigation(
    tabs: &Tabs,
    edits: &BufferEdits,
    diagnostics: &DiagnosticsStates,
    git: &GitState,
    forward: bool,
) -> Option<NavOutcome> {
    for &level in IssueCategory::NAV_LEVELS {
        let cats = IssueCategory::at_level(level);
        if let Some(outcome) = scan_level(tabs, edits, diagnostics, git, forward, cats) {
            return Some(outcome);
        }
    }
    None
}

fn scan_level(
    tabs: &Tabs,
    edits: &BufferEdits,
    diagnostics: &DiagnosticsStates,
    git: &GitState,
    forward: bool,
    cats: &[IssueCategory],
) -> Option<NavOutcome> {
    let mut positions = collect_positions(edits, diagnostics, git, cats);
    if positions.is_empty() {
        return None;
    }
    positions.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.row.cmp(&b.row))
            .then(a.col.cmp(&b.col))
    });
    // Dedup by (path, row, col) so multiple categories on one
    // line collapse to a single navigation target. Without this
    // `pick_target_index` would count each category separately
    // and the "N" in "X/N" would desync from what the user sees.
    positions.dedup_by(|a, b| a.path == b.path && a.row == b.row && a.col == b.col);

    let cur = cursor_pos(tabs);
    let target_idx = pick_target_index(&positions, cur, forward);
    let pos = &positions[target_idx];

    Some(NavOutcome {
        target_path: pos.path.clone(),
        target_row: pos.row,
        target_col: pos.col,
        category: pos.category,
        position: target_idx + 1,
        total: positions.len(),
    })
}

fn cursor_pos(tabs: &Tabs) -> Option<(CanonPath, usize, usize)> {
    let id = tabs.active?;
    let tab = tabs.open.iter().find(|t| t.id == id)?;
    Some((tab.path.clone(), tab.cursor.line, tab.cursor.col))
}

/// Find the next (or previous) position in the sorted cycle
/// relative to the cursor. Wraps around on either end.
fn pick_target_index(
    positions: &[Pos],
    cur: Option<(CanonPath, usize, usize)>,
    forward: bool,
) -> usize {
    let total = positions.len();
    let Some((cp, cr, cc)) = cur else {
        return 0;
    };
    let key = (&cp, cr, cc);
    if forward {
        positions
            .iter()
            .position(|p| (&p.path, p.row, p.col) > key)
            .unwrap_or(0)
    } else {
        positions
            .iter()
            .rposition(|p| (&p.path, p.row, p.col) < key)
            .unwrap_or(total.saturating_sub(1))
    }
}

fn collect_positions(
    edits: &BufferEdits,
    diagnostics: &DiagnosticsStates,
    git: &GitState,
    cats: &[IssueCategory],
) -> Vec<Pos> {
    let mut out: Vec<Pos> = Vec::new();
    collect_diagnostic_positions(edits, diagnostics, cats, &mut out);
    collect_git_positions(edits, git, cats, &mut out);
    out
}

fn collect_diagnostic_positions(
    edits: &BufferEdits,
    diagnostics: &DiagnosticsStates,
    cats: &[IssueCategory],
    out: &mut Vec<Pos>,
) {
    for (path, bd) in diagnostics.by_path.iter() {
        for d in bd.diagnostics.iter() {
            let cat = match d.severity {
                DiagnosticSeverity::Error => IssueCategory::LspError,
                DiagnosticSeverity::Warning => IssueCategory::LspWarning,
                // Info / Hint are never navigable — legacy parity.
                _ => continue,
            };
            if !cats.contains(&cat) {
                continue;
            }
            let row = clamp_row_to_buffer(edits, path, d.start_line);
            out.push(Pos {
                path: path.clone(),
                row,
                col: d.start_col,
                category: cat,
            });
        }
    }
}

fn collect_git_positions(
    edits: &BufferEdits,
    git: &GitState,
    cats: &[IssueCategory],
    out: &mut Vec<Pos>,
) {
    let any_git = cats.iter().any(|c| {
        matches!(
            c,
            IssueCategory::Unstaged
                | IssueCategory::StagedModified
                | IssueCategory::StagedNew
                | IssueCategory::Untracked,
        )
    });
    if !any_git {
        return;
    }
    for (path, file_cats) in git.file_statuses.iter() {
        if !file_cats.iter().any(|c| cats.contains(c)) {
            continue;
        }
        // Per-line ranges take precedence over the file-level
        // fallback — a dirty tracked file typically carries both.
        let line_statuses = git.line_statuses.get(path);
        let matching: Vec<&led_core::git::LineStatus> = line_statuses
            .map(|arc| arc.iter().filter(|ls| cats.contains(&ls.category)).collect())
            .unwrap_or_default();
        if matching.is_empty() {
            // File-level fallback: untracked/staged-new have no
            // per-line data. Pin to row 0.
            let cat = file_cats
                .iter()
                .find(|c| cats.contains(c))
                .copied()
                .unwrap_or(IssueCategory::Unstaged);
            out.push(Pos {
                path: path.clone(),
                row: 0,
                col: 0,
                category: cat,
            });
        } else {
            for ls in matching {
                out.push(Pos {
                    path: path.clone(),
                    row: clamp_row_to_buffer(edits, path, ls.rows.start),
                    col: 0,
                    category: ls.category,
                });
            }
        }
    }
}

/// Clamp `row` to the buffer's last line when the buffer is
/// loaded, else return it unchanged. Raw diagnostic / git line
/// numbers can point past the buffer if the server saw a stale
/// version; clamping keeps the cursor in bounds.
fn clamp_row_to_buffer(edits: &BufferEdits, path: &CanonPath, row: usize) -> usize {
    edits
        .buffers
        .get(path)
        .map(|eb| row.min(eb.rope.len_lines().saturating_sub(1)))
        .unwrap_or(row)
}

const ISSUE_NAV_TTL: Duration = Duration::from_secs(2);

/// Alt-. (forward) — jump to the next issue in the highest
/// non-empty tier.
#[allow(clippy::too_many_arguments)]
pub(super) fn next_issue_active(
    tabs: &mut Tabs,
    edits: &BufferEdits,
    diagnostics: &DiagnosticsStates,
    git: &GitState,
    jumps: &mut JumpListState,
    alerts: &mut AlertState,
    terminal: &Terminal,
    browser: &BrowserUi,
) {
    nav_issue(
        tabs,
        edits,
        diagnostics,
        git,
        jumps,
        alerts,
        terminal,
        browser,
        true,
    );
}

/// Alt-, (backward) — mirror of `next_issue_active`.
#[allow(clippy::too_many_arguments)]
pub(super) fn prev_issue_active(
    tabs: &mut Tabs,
    edits: &BufferEdits,
    diagnostics: &DiagnosticsStates,
    git: &GitState,
    jumps: &mut JumpListState,
    alerts: &mut AlertState,
    terminal: &Terminal,
    browser: &BrowserUi,
) {
    nav_issue(
        tabs,
        edits,
        diagnostics,
        git,
        jumps,
        alerts,
        terminal,
        browser,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
fn nav_issue(
    tabs: &mut Tabs,
    edits: &BufferEdits,
    diagnostics: &DiagnosticsStates,
    git: &GitState,
    jumps: &mut JumpListState,
    alerts: &mut AlertState,
    terminal: &Terminal,
    browser: &BrowserUi,
    forward: bool,
) {
    let Some(outcome) = compute_navigation(tabs, edits, diagnostics, git, forward) else {
        return;
    };

    // Always record the pre-jump position so Alt-b round-trips,
    // regardless of whether the target is already open or not.
    if let Some(current) = current_position(tabs) {
        jumps.record(current);
    }

    let info = outcome.category.info();
    let msg = format!(
        " Jumped to {} {}/{}",
        info.label, outcome.position, outcome.total,
    );

    // Two paths (M21):
    //   * Buffer is already loaded → land cursor + recenter
    //     scroll inline.
    //   * Buffer not yet loaded → open / focus a tab at the
    //     target path and stash the cursor as `pending_cursor`.
    //     The load-completion ingest applies it once the rope
    //     materialises.
    if let Some(target_idx) = tabs
        .open
        .iter()
        .position(|t| t.path == outcome.target_path)
        && let Some(eb) = edits.buffers.get(&outcome.target_path)
    {
        let rope = &eb.rope;
        let line_count = rope.len_lines();
        let line = outcome.target_row.min(line_count.saturating_sub(1));
        let col = outcome.target_col.min(line_char_len(rope, line));
        let body_rows = terminal
            .dims
            .map(|d| {
                led_driver_terminal_core::Layout::compute(d, browser.visible)
                    .editor_area
                    .rows as usize
            })
            .unwrap_or(0);
        let content_cols = editor_content_cols(terminal, browser);
        let tab = &mut tabs.open[target_idx];
        tab.cursor.line = line;
        tab.cursor.col = col;
        tab.cursor.preferred_col = col;
        tab.scroll = center_on_cursor(tab.scroll, tab.cursor, body_rows, rope, content_cols);
        tabs.active = Some(tab.id);
        alerts.set_info(msg, Instant::now(), ISSUE_NAV_TTL);
        return;
    }

    // Open / focus a tab at the target path and stash the
    // pending cursor; the load-completion hook does the apply.
    super::shared::open_or_focus_tab(tabs, &outcome.target_path, true);
    if let Some(tab) = tabs
        .open
        .iter_mut()
        .find(|t| t.path == outcome.target_path)
    {
        tab.pending_cursor = Some(led_state_tabs::Cursor {
            line: outcome.target_row,
            col: outcome.target_col,
            preferred_col: outcome.target_col,
        });
    }
    alerts.set_info(msg, Instant::now(), ISSUE_NAV_TTL);
}

#[cfg(test)]
mod tests {
    use led_driver_terminal_core::Dims;
    use led_state_jumps::JumpListState;
    use led_state_tabs::Cursor;

    use super::super::testutil::*;
    use super::*;

    fn set_cursor(tabs: &mut led_state_tabs::Tabs, line: usize, col: usize) {
        tabs.open[0].cursor = Cursor {
            line,
            col,
            preferred_col: col,
        };
    }

    #[test]
    fn match_bracket_jumps_from_open_to_close() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("a { b } c", Dims { cols: 20, rows: 5 });
        set_cursor(&mut tabs, 0, 2); // on '{'
        let mut jumps = JumpListState::default();
        match_bracket(&mut tabs, &edits, &mut jumps);
        assert_eq!(tabs.open[0].cursor.col, 6); // on '}'
    }

    #[test]
    fn match_bracket_jumps_from_close_to_open() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("a { b } c", Dims { cols: 20, rows: 5 });
        set_cursor(&mut tabs, 0, 6); // on '}'
        let mut jumps = JumpListState::default();
        match_bracket(&mut tabs, &edits, &mut jumps);
        assert_eq!(tabs.open[0].cursor.col, 2); // on '{'
    }

    #[test]
    fn match_bracket_considers_char_before_cursor() {
        // Cursor just past '}' → fall back to char-before.
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("a { b }", Dims { cols: 20, rows: 5 });
        set_cursor(&mut tabs, 0, 7); // past '}'
        let mut jumps = JumpListState::default();
        match_bracket(&mut tabs, &edits, &mut jumps);
        assert_eq!(tabs.open[0].cursor.col, 2);
    }

    #[test]
    fn match_bracket_noop_when_no_bracket() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("no brackets", Dims { cols: 20, rows: 5 });
        set_cursor(&mut tabs, 0, 3);
        let mut jumps = JumpListState::default();
        match_bracket(&mut tabs, &edits, &mut jumps);
        assert_eq!(tabs.open[0].cursor.col, 3);
        assert!(jumps.entries.is_empty());
    }

    #[test]
    fn match_bracket_records_pre_jump_position() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("{abc}", Dims { cols: 20, rows: 5 });
        set_cursor(&mut tabs, 0, 0);
        let mut jumps = JumpListState::default();
        match_bracket(&mut tabs, &edits, &mut jumps);
        assert_eq!(jumps.entries.len(), 1);
        assert_eq!(jumps.entries[0].line, 0);
        assert_eq!(jumps.entries[0].col, 0);
    }

    #[test]
    fn jump_back_noop_on_empty_list() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("hello", Dims { cols: 20, rows: 5 });
        let mut jumps = JumpListState::default();
        jump_back(&mut tabs, &edits, &mut jumps);
        assert_eq!(tabs.open[0].cursor.col, 0);
    }

    #[test]
    fn jump_back_from_head_records_current_and_returns() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("aaaaaaaaaa", Dims { cols: 20, rows: 5 });
        set_cursor(&mut tabs, 0, 2); // prior "interesting" position
        let mut jumps = JumpListState::default();
        // Record a jump position manually; then move cursor and back.
        jumps.record(super::JumpPosition {
            path: canon("file.rs"),
            line: 0,
            col: 2,
        });
        set_cursor(&mut tabs, 0, 9); // "current"
        jump_back(&mut tabs, &edits, &mut jumps);
        // Cursor back at col 2.
        assert_eq!(tabs.open[0].cursor.col, 2);
        // Forward entry now exists (the save-before-back of "9").
        assert!(jumps.can_forward());
    }

    #[test]
    fn jump_back_forward_round_trip() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("aaaaaaaaaa", Dims { cols: 20, rows: 5 });
        let mut jumps = JumpListState::default();
        jumps.record(super::JumpPosition {
            path: canon("file.rs"),
            line: 0,
            col: 3,
        });
        set_cursor(&mut tabs, 0, 8);
        jump_back(&mut tabs, &edits, &mut jumps);
        assert_eq!(tabs.open[0].cursor.col, 3);
        jump_forward(&mut tabs, &edits, &mut jumps);
        assert_eq!(tabs.open[0].cursor.col, 8);
    }

    #[test]
    fn jump_to_closed_tab_is_silent_noop() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("hello", Dims { cols: 20, rows: 5 });
        let mut jumps = JumpListState::default();
        jumps.record(super::JumpPosition {
            path: canon("other.rs"), // not open
            line: 0,
            col: 0,
        });
        set_cursor(&mut tabs, 0, 4);
        jump_back(&mut tabs, &edits, &mut jumps);
        // Cursor unchanged — target tab wasn't open.
        assert_eq!(tabs.open[0].cursor.col, 4);
    }

    #[test]
    fn tab_cycle_records_outgoing_position() {
        // Covered in tabs.rs but also needs a nav-side check: the
        // jump-list should grow by one per tab-switch.
        use super::super::tabs::cycle_active;
        let mut tabs = tabs_with(&[("a", 1), ("b", 2)], Some(1));
        tabs.open[0].cursor = Cursor {
            line: 3,
            col: 5,
            preferred_col: 5,
        };
        let mut jumps = JumpListState::default();
        cycle_active(&mut tabs, &mut jumps, 1);
        assert_eq!(jumps.entries.len(), 1);
        assert_eq!(jumps.entries[0].line, 3);
        assert_eq!(jumps.entries[0].col, 5);
    }

    // ── Issue navigation (M20a) ────────────────────────────────

    use led_core::UserPath;
    use led_state_alerts::AlertState as M20aAlertState;
    use led_state_diagnostics::{
        BufferDiagnostics, Diagnostic, DiagnosticSeverity, DiagnosticsStates,
    };
    use led_state_git::GitState as M20aGitState;
    use led_state_tabs::{Scroll, Tab, TabId};
    use std::sync::Arc;

    fn canon_of(p: &str) -> led_core::CanonPath {
        UserPath::new(p).canonicalize()
    }

    fn pos(path: &str, row: usize, col: usize) -> Pos {
        Pos {
            path: canon_of(path),
            row,
            col,
            category: IssueCategory::Unstaged,
        }
    }

    #[test]
    fn pick_target_no_cursor_picks_first() {
        let ps = vec![pos("/a", 1, 0), pos("/a", 5, 0), pos("/b", 0, 0)];
        assert_eq!(pick_target_index(&ps, None, true), 0);
        assert_eq!(pick_target_index(&ps, None, false), 0);
    }

    #[test]
    fn pick_target_forward_picks_next_after_cursor() {
        let ps = vec![pos("/a", 1, 0), pos("/a", 5, 0), pos("/a", 9, 0)];
        let cur = Some((canon_of("/a"), 5, 0));
        assert_eq!(pick_target_index(&ps, cur, true), 2);
    }

    #[test]
    fn pick_target_forward_wraps_around() {
        let ps = vec![pos("/a", 1, 0), pos("/a", 5, 0)];
        let cur = Some((canon_of("/a"), 9, 0));
        assert_eq!(pick_target_index(&ps, cur, true), 0);
    }

    #[test]
    fn pick_target_backward_picks_prev_before_cursor() {
        let ps = vec![pos("/a", 1, 0), pos("/a", 5, 0), pos("/a", 9, 0)];
        let cur = Some((canon_of("/a"), 5, 0));
        assert_eq!(pick_target_index(&ps, cur, false), 0);
    }

    #[test]
    fn pick_target_backward_wraps_around() {
        let ps = vec![pos("/a", 1, 0), pos("/a", 5, 0)];
        let cur = Some((canon_of("/a"), 0, 0));
        assert_eq!(pick_target_index(&ps, cur, false), 1);
    }

    #[test]
    fn pick_target_crosses_files() {
        let ps = vec![pos("/a", 5, 0), pos("/b", 1, 0)];
        let cur = Some((canon_of("/a"), 5, 0));
        assert_eq!(pick_target_index(&ps, cur, true), 1);
    }

    fn diag(severity: DiagnosticSeverity, start_line: usize, start_col: usize) -> Diagnostic {
        Diagnostic {
            start_line,
            start_col,
            end_line: start_line,
            end_col: start_col + 1,
            severity,
            message: String::new(),
            source: None,
            code: None,
        }
    }

    fn seed_tabs_and_edits(
        path: &str,
        rope_str: &str,
    ) -> (Tabs, BufferEdits) {
        let canon = canon_of(path);
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon.clone(),
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let mut edits = BufferEdits::default();
        use led_state_buffer_edits::EditedBuffer;
        edits.buffers.insert(
            canon,
            EditedBuffer::fresh(Arc::new(ropey::Rope::from_str(rope_str))),
        );
        (tabs, edits)
    }

    #[test]
    fn errors_take_priority_over_warnings() {
        // Level 1 = LspError, level 2 = LspWarning. `compute_navigation`
        // returns the first non-empty level; with both present the
        // cycle stays on errors.
        let (tabs, edits) =
            seed_tabs_and_edits("/p/a.rs", "aaaa\nbbbb\ncccc\ndddd\neeee\n");
        let mut diags = DiagnosticsStates::default();
        let canon = canon_of("/p/a.rs");
        diags.by_path.insert(
            canon.clone(),
            BufferDiagnostics::new(
                led_core::PersistedContentHash(1),
                vec![
                    diag(DiagnosticSeverity::Warning, 1, 0),
                    diag(DiagnosticSeverity::Error, 3, 0),
                ],
            ),
        );
        let git = M20aGitState::default();
        let outcome =
            compute_navigation(&tabs, &edits, &diags, &git, true).expect("nav");
        assert_eq!(outcome.category, IssueCategory::LspError);
        assert_eq!(outcome.target_row, 3);
        assert_eq!(outcome.total, 1);
    }

    #[test]
    fn falls_through_to_warnings_when_no_errors() {
        let (tabs, edits) =
            seed_tabs_and_edits("/p/a.rs", "aaaa\nbbbb\ncccc\n");
        let mut diags = DiagnosticsStates::default();
        let canon = canon_of("/p/a.rs");
        diags.by_path.insert(
            canon.clone(),
            BufferDiagnostics::new(
                led_core::PersistedContentHash(1),
                vec![diag(DiagnosticSeverity::Warning, 1, 0)],
            ),
        );
        let git = M20aGitState::default();
        let outcome = compute_navigation(&tabs, &edits, &diags, &git, true).unwrap();
        assert_eq!(outcome.category, IssueCategory::LspWarning);
        assert_eq!(outcome.target_row, 1);
    }

    #[test]
    fn falls_through_to_git_when_no_diagnostics() {
        let (tabs, edits) =
            seed_tabs_and_edits("/p/a.rs", "aaaa\nbbbb\ncccc\n");
        let diags = DiagnosticsStates::default();
        let canon = canon_of("/p/a.rs");
        let mut git = M20aGitState::default();
        let mut cats = imbl::HashSet::default();
        cats.insert(IssueCategory::Unstaged);
        git.file_statuses.insert(canon.clone(), cats);
        git.line_statuses.insert(
            canon.clone(),
            Arc::new(vec![led_core::git::LineStatus {
                category: IssueCategory::Unstaged,
                rows: 2..3,
            }]),
        );
        let outcome = compute_navigation(&tabs, &edits, &diags, &git, true).unwrap();
        assert_eq!(outcome.category, IssueCategory::Unstaged);
        assert_eq!(outcome.target_row, 2);
    }

    #[test]
    fn compute_returns_none_when_empty() {
        let (tabs, edits) = seed_tabs_and_edits("/p/a.rs", "abc\n");
        let diags = DiagnosticsStates::default();
        let git = M20aGitState::default();
        assert!(compute_navigation(&tabs, &edits, &diags, &git, true).is_none());
        assert!(compute_navigation(&tabs, &edits, &diags, &git, false).is_none());
    }

    #[test]
    fn next_issue_moves_cursor_and_records_jump_and_alert() {
        use led_driver_terminal_core::{Dims, Terminal as TerminalAtom};
        use led_state_browser::BrowserUi;
        let (mut tabs, edits) =
            seed_tabs_and_edits("/p/a.rs", "aaaa\nbbbb\ncccc\ndddd\neeee\n");
        // Cursor at line 0, col 0.
        let mut diags = DiagnosticsStates::default();
        let canon = canon_of("/p/a.rs");
        diags.by_path.insert(
            canon.clone(),
            BufferDiagnostics::new(
                led_core::PersistedContentHash(1),
                vec![diag(DiagnosticSeverity::Error, 3, 0)],
            ),
        );
        let git = M20aGitState::default();
        let mut jumps = JumpListState::default();
        let mut alerts = M20aAlertState::default();
        let term = TerminalAtom {
            dims: Some(Dims { cols: 80, rows: 24 }),
            ..Default::default()
        };
        let browser = BrowserUi {
            visible: false,
            ..Default::default()
        };
        next_issue_active(
            &mut tabs,
            &edits,
            &diags,
            &git,
            &mut jumps,
            &mut alerts,
            &term,
            &browser,
        );
        assert_eq!(tabs.open[0].cursor.line, 3);
        assert_eq!(tabs.open[0].cursor.col, 0);
        assert_eq!(jumps.entries.len(), 1);
        assert!(
            alerts
                .info
                .as_deref()
                .is_some_and(|m| m.contains("Jumped to Error 1/1"))
        );
    }

    #[test]
    fn next_issue_wraps_within_level() {
        use led_driver_terminal_core::{Dims, Terminal as TerminalAtom};
        use led_state_browser::BrowserUi;
        use led_state_tabs::Cursor as TabCursor;
        let (mut tabs, edits) =
            seed_tabs_and_edits("/p/a.rs", "aaaa\nbbbb\ncccc\ndddd\neeee\n");
        // Two errors at lines 1 and 3.
        let mut diags = DiagnosticsStates::default();
        let canon = canon_of("/p/a.rs");
        diags.by_path.insert(
            canon.clone(),
            BufferDiagnostics::new(
                led_core::PersistedContentHash(1),
                vec![
                    diag(DiagnosticSeverity::Error, 1, 0),
                    diag(DiagnosticSeverity::Error, 3, 0),
                ],
            ),
        );
        let git = M20aGitState::default();
        let mut jumps = JumpListState::default();
        let mut alerts = M20aAlertState::default();
        let term = TerminalAtom {
            dims: Some(Dims { cols: 80, rows: 24 }),
            ..Default::default()
        };
        let browser = BrowserUi {
            visible: false,
            ..Default::default()
        };
        // Start cursor past both errors → wrap.
        tabs.open[0].cursor = TabCursor {
            line: 4,
            col: 0,
            preferred_col: 0,
        };
        tabs.open[0].scroll = Scroll::default();
        next_issue_active(
            &mut tabs,
            &edits,
            &diags,
            &git,
            &mut jumps,
            &mut alerts,
            &term,
            &browser,
        );
        assert_eq!(tabs.open[0].cursor.line, 1, "wrapped back to first error");
    }

    #[test]
    fn next_issue_opens_unopened_target_with_pending_cursor() {
        // M21: a diagnostic on an unopened path now opens the
        // tab and stashes a pending cursor that the load
        // completion will apply. Pre-M21 this silently no-op'd.
        use led_driver_terminal_core::Terminal as TerminalAtom;
        use led_state_browser::BrowserUi;
        let (mut tabs, edits) = seed_tabs_and_edits("/p/a.rs", "abc\n");
        let target = canon_of("/p/other.rs");
        let mut diags = DiagnosticsStates::default();
        diags.by_path.insert(
            target.clone(),
            BufferDiagnostics::new(
                led_core::PersistedContentHash(1),
                vec![diag(DiagnosticSeverity::Error, 7, 2)],
            ),
        );
        let git = M20aGitState::default();
        let mut jumps = JumpListState::default();
        let mut alerts = M20aAlertState::default();
        next_issue_active(
            &mut tabs,
            &edits,
            &diags,
            &git,
            &mut jumps,
            &mut alerts,
            &TerminalAtom::default(),
            &BrowserUi::default(),
        );
        let new_tab = tabs
            .open
            .iter()
            .find(|t| t.path == target)
            .expect("opened tab for issue target");
        assert_eq!(tabs.active, Some(new_tab.id));
        assert_eq!(
            new_tab.pending_cursor,
            Some(led_state_tabs::Cursor {
                line: 7,
                col: 2,
                preferred_col: 2,
            }),
        );
        assert_eq!(jumps.entries.len(), 1);
        assert!(alerts.info.is_some(), "alert message set");
    }
}

