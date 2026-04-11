//! Issue navigation (Alt-. / Alt-,).
//!
//! Listens for `Action::NextIssue` / `Action::PrevIssue`, computes the
//! navigation outcome in a pure function, and emits fine-grained Muts:
//!
//! - `Mut::Alert` — status bar feedback ("Jumped to <type> X/N")
//! - `Mut::BufferUpdate` — when same buffer or other-already-open
//! - `Mut::SetActiveTab` — when crossing buffers
//! - `Mut::RequestOpen` + `Mut::SetTabPendingCursor` — when target file isn't open
//!
//! All decision-making lives in the combinator chains; the reducer just
//! assigns each Mut's payload.

use std::rc::Rc;

use led_core::rx::Stream;
use led_core::{Action, CanonPath, Col, IssueCategory, Row, SubLine};
use led_lsp::DiagnosticSeverity;
use led_state::AppState;

use super::Mut;
use super::mov;

/// Pure result of computing where to jump to.
#[derive(Clone, Debug)]
struct NavOutcome {
    target_path: CanonPath,
    target_row: usize,
    target_col: usize,
    category: IssueCategory,
    /// 1-based position in the cycle (for "X/N" display).
    position: usize,
    /// Total number of items in the cycle.
    total: usize,
}

pub fn nav_of(raw_actions: &Stream<Action>, state: &Stream<Rc<AppState>>) -> Stream<Mut> {
    // Parent: compute the navigation outcome for each NextIssue/PrevIssue.
    // The outcome is paired with the state snapshot it was computed from
    // so child streams can decide which case they apply to.
    let nav_parent = raw_actions
        .filter(|a| matches!(a, Action::NextIssue | Action::PrevIssue))
        .map(|a| matches!(a, Action::NextIssue))
        .sample_combine(state)
        .filter_map(|(forward, s)| {
            let outcome = compute_navigation(&s, forward)?;
            Some((outcome, s))
        })
        .stream();

    // Child: alert with category + position
    let alert_s = nav_parent
        .clone()
        .map(|(nav, _)| {
            let info = nav.category.info();
            Mut::Alert {
                info: Some(format!(
                    "Jumped to {} {}/{}",
                    info.label, nav.position, nav.total
                )),
            }
        })
        .stream();

    // Child: same-buffer cursor + scroll update
    let same_buffer_s = nav_parent
        .clone()
        .filter(|(nav, s)| s.active_tab.as_ref() == Some(&nav.target_path))
        .filter_map(|(nav, s)| same_buffer_update(&s, &nav))
        .map(|(path, buf)| Mut::BufferUpdate(path, buf))
        .stream();

    // Child: other-buffer (already open) cursor + scroll update
    let other_buffer_s = nav_parent
        .clone()
        .filter(|(nav, s)| s.active_tab.as_ref() != Some(&nav.target_path))
        .filter(|(nav, s)| s.tabs.iter().any(|t| *t.path() == nav.target_path))
        .filter_map(|(nav, s)| other_buffer_update(&s, &nav))
        .map(|(path, buf)| Mut::BufferUpdate(path, buf))
        .stream();

    // Child: SetActiveTab when target is in a different (already-open) tab
    let set_active_tab_s = nav_parent
        .clone()
        .filter(|(nav, s)| s.active_tab.as_ref() != Some(&nav.target_path))
        .filter(|(nav, s)| s.tabs.iter().any(|t| *t.path() == nav.target_path))
        .map(|(nav, _)| Mut::SetActiveTab(nav.target_path))
        .stream();

    // Children for not-yet-open file: RequestOpen + SetActiveTab + SetTabPendingCursor.
    // Each is a separate stream — single-purpose, single Mut.
    let needs_open = nav_parent
        .clone()
        .filter(|(nav, s)| !s.tabs.iter().any(|t| *t.path() == nav.target_path))
        .stream();

    let open_request_s = needs_open
        .clone()
        .map(|(nav, _)| Mut::RequestOpen(nav.target_path))
        .stream();

    let open_active_s = needs_open
        .clone()
        .map(|(nav, _)| Mut::SetActiveTab(nav.target_path))
        .stream();

    let open_pending_s = needs_open
        .map(|(nav, s)| {
            let half = s.dims.map_or(10, |d| d.buffer_height() / 2);
            Mut::SetTabPendingCursor {
                path: nav.target_path,
                row: Row(nav.target_row),
                col: Col(nav.target_col),
                scroll_row: Row(nav.target_row.saturating_sub(half)),
            }
        })
        .stream();

    let merged: Stream<Mut> = Stream::new();
    alert_s.forward(&merged);
    same_buffer_s.forward(&merged);
    other_buffer_s.forward(&merged);
    set_active_tab_s.forward(&merged);
    open_request_s.forward(&merged);
    open_active_s.forward(&merged);
    open_pending_s.forward(&merged);
    merged
}

// ── Pure helpers ──

/// Compute the navigation outcome for the current state. Walks the canonical
/// nav levels until a level with at least one item is found.
fn compute_navigation(state: &AppState, forward: bool) -> Option<NavOutcome> {
    for &level in IssueCategory::NAV_LEVELS {
        let cats = IssueCategory::at_level(level);
        if let Some(outcome) = scan_level(state, forward, cats) {
            return Some(outcome);
        }
    }
    None
}

#[derive(Clone)]
struct Pos {
    path: CanonPath,
    row: usize,
    col: usize,
    category: IssueCategory,
}

fn scan_level(state: &AppState, forward: bool, cats: &[IssueCategory]) -> Option<NavOutcome> {
    let mut positions = collect_positions(state, cats);
    if positions.is_empty() {
        return None;
    }
    positions.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.row.cmp(&b.row))
            .then(a.col.cmp(&b.col))
    });

    let cur = cursor_pos(state);
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

/// Pick the next target in the sorted cycle relative to the current cursor.
/// Wraps around when there's nothing further in the requested direction.
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
            .unwrap_or(total - 1)
    }
}

fn cursor_pos(state: &AppState) -> Option<(CanonPath, usize, usize)> {
    let path = state.active_tab.as_ref()?;
    let buf = state.buffers.get(path)?;
    Some((path.clone(), buf.cursor_row().0, buf.cursor_col().0))
}

fn collect_positions(state: &AppState, cats: &[IssueCategory]) -> Vec<Pos> {
    let mut out = Vec::new();
    collect_diagnostic_positions(state, cats, &mut out);
    collect_git_positions(state, cats, &mut out);
    collect_pr_positions(state, cats, &mut out);
    out
}

fn collect_diagnostic_positions(state: &AppState, cats: &[IssueCategory], out: &mut Vec<Pos>) {
    for buf in state.buffers.values() {
        let Some(path) = buf.path() else { continue };
        for d in buf.status().diagnostics() {
            let cat = match d.severity {
                DiagnosticSeverity::Error => IssueCategory::LspError,
                DiagnosticSeverity::Warning => IssueCategory::LspWarning,
                _ => continue,
            };
            if !cats.contains(&cat) {
                continue;
            }
            out.push(Pos {
                path: path.clone(),
                row: *d.start_row,
                col: *d.start_col,
                category: cat,
            });
        }
    }
}

fn collect_git_positions(state: &AppState, cats: &[IssueCategory], out: &mut Vec<Pos>) {
    let any_git = cats.iter().any(|c| {
        matches!(
            c,
            IssueCategory::Unstaged
                | IssueCategory::StagedModified
                | IssueCategory::StagedNew
                | IssueCategory::Untracked
        )
    });
    if !any_git {
        return;
    }

    for (path, file_cats) in state.git.file_statuses.iter() {
        if !file_cats.iter().any(|c| cats.contains(c)) {
            continue;
        }
        let line_statuses = state
            .buffers
            .get(path)
            .map(|buf| buf.status().git_line_statuses())
            .unwrap_or(&[]);

        let matching: Vec<_> = line_statuses
            .iter()
            .filter(|ls| cats.contains(&ls.category))
            .collect();

        if matching.is_empty() {
            // File-level fallback (e.g. untracked file with no line ranges).
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
                    row: ls.rows.start,
                    col: 0,
                    category: ls.category,
                });
            }
        }
    }
}

fn collect_pr_positions(state: &AppState, cats: &[IssueCategory], out: &mut Vec<Pos>) {
    let Some(pr) = &state.git.pr else { return };
    if cats.contains(&IssueCategory::PrComment) {
        for (path, comments) in &pr.comments {
            for c in comments {
                out.push(Pos {
                    path: path.clone(),
                    row: *c.line,
                    col: 0,
                    category: IssueCategory::PrComment,
                });
            }
        }
    }
    if cats.contains(&IssueCategory::PrDiff) {
        for (path, line_statuses) in &pr.diff_files {
            for ls in line_statuses {
                out.push(Pos {
                    path: path.clone(),
                    row: ls.rows.start,
                    col: 0,
                    category: IssueCategory::PrDiff,
                });
            }
        }
    }
}

// ── Buffer mutation helpers (pure: take state, return cloned buffer) ──

fn same_buffer_update(
    state: &AppState,
    nav: &NavOutcome,
) -> Option<(CanonPath, led_state::BufferState)> {
    let dims = state.dims?;
    let buf = state.buffers.get(&nav.target_path)?;
    let mut buf = (**buf).clone();
    super::action::close_group_on_move(&mut buf);
    buf.set_cursor(
        Row(nav.target_row),
        Col(nav.target_col),
        Col(nav.target_col),
    );
    let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
    buf.set_scroll(Row(sr), SubLine(ssl));
    Some((nav.target_path.clone(), buf))
}

fn other_buffer_update(
    state: &AppState,
    nav: &NavOutcome,
) -> Option<(CanonPath, led_state::BufferState)> {
    let half = state.dims.map_or(10, |d| d.buffer_height() / 2);
    let buf = state.buffers.get(&nav.target_path)?;
    let buf = (**buf).clone();
    let r = nav.target_row.min(buf.doc().line_count().saturating_sub(1));
    buf.set_cursor(Row(r), Col(nav.target_col), Col(nav.target_col));
    buf.set_scroll(Row(r.saturating_sub(half)), SubLine(0));
    Some((nav.target_path.clone(), buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;

    fn p(path: &str, row: usize, col: usize) -> Pos {
        Pos {
            path: UserPath::new(path).canonicalize(),
            row,
            col,
            category: IssueCategory::Unstaged,
        }
    }

    // ── pick_target_index ──

    #[test]
    fn pick_target_no_cursor_picks_first() {
        let ps = vec![p("/a", 1, 0), p("/a", 5, 0), p("/b", 0, 0)];
        assert_eq!(pick_target_index(&ps, None, true), 0);
        assert_eq!(pick_target_index(&ps, None, false), 0);
    }

    #[test]
    fn pick_target_forward_picks_next_after_cursor() {
        let ps = vec![p("/a", 1, 0), p("/a", 5, 0), p("/a", 9, 0)];
        let cur = Some((UserPath::new("/a").canonicalize(), 5, 0));
        assert_eq!(pick_target_index(&ps, cur, true), 2);
    }

    #[test]
    fn pick_target_forward_wraps_around() {
        let ps = vec![p("/a", 1, 0), p("/a", 5, 0)];
        let cur = Some((UserPath::new("/a").canonicalize(), 9, 0));
        assert_eq!(pick_target_index(&ps, cur, true), 0); // wraps to first
    }

    #[test]
    fn pick_target_backward_picks_prev_before_cursor() {
        let ps = vec![p("/a", 1, 0), p("/a", 5, 0), p("/a", 9, 0)];
        let cur = Some((UserPath::new("/a").canonicalize(), 5, 0));
        assert_eq!(pick_target_index(&ps, cur, false), 0);
    }

    #[test]
    fn pick_target_backward_wraps_around() {
        let ps = vec![p("/a", 1, 0), p("/a", 5, 0)];
        let cur = Some((UserPath::new("/a").canonicalize(), 0, 0));
        assert_eq!(pick_target_index(&ps, cur, false), 1); // wraps to last
    }

    #[test]
    fn pick_target_crosses_files() {
        let ps = vec![p("/a", 5, 0), p("/b", 1, 0)];
        let cur = Some((UserPath::new("/a").canonicalize(), 5, 0));
        assert_eq!(pick_target_index(&ps, cur, true), 1); // /b/1 > /a/5
    }

    // ── compute_navigation: smoke test via empty state ──

    #[test]
    fn compute_navigation_empty_state_returns_none() {
        let state = led_state::AppState::new(led_core::Startup {
            headless: true,
            enable_watchers: false,
            arg_paths: vec![],
            arg_dir: None,
            start_dir: std::sync::Arc::new(UserPath::new("/tmp").canonicalize()),
            user_start_dir: UserPath::new("/tmp"),
            config_dir: UserPath::new("/tmp/config"),
            test_lsp_server: None,
            test_gh_binary: None,
        });
        assert!(compute_navigation(&state, true).is_none());
        assert!(compute_navigation(&state, false).is_none());
    }
}
