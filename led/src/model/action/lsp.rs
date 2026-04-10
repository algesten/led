use led_core::CanonPath;
use led_core::{Action, PanelSlot};
use led_state::{AppState, LspRequest, RenameState};

use super::super::mov;
use super::helpers::{close_group_on_move, reveal_active_buffer, word_under_cursor};

pub fn handle_completion_action(state: &mut AppState, action: &Action) -> bool {
    match action {
        Action::MoveUp => {
            let lsp = state.lsp_mut();
            if let Some(ref mut comp) = lsp.completion {
                comp.selected = comp.selected.saturating_sub(1);
                if comp.selected < comp.scroll_offset {
                    comp.scroll_offset = comp.selected;
                }
            }
            true
        }
        Action::MoveDown => {
            let lsp = state.lsp_mut();
            if let Some(ref mut comp) = lsp.completion {
                comp.selected = (comp.selected + 1).min(comp.items.len().saturating_sub(1));
                let max_visible = 10;
                if comp.selected >= comp.scroll_offset + max_visible {
                    comp.scroll_offset = comp.selected + 1 - max_visible;
                }
            }
            true
        }
        Action::InsertNewline | Action::InsertTab => {
            // Accept completion
            let comp = state.lsp.completion.clone();
            if let Some(comp) = comp {
                let index = comp.selected;
                if let Some(item) = comp.items.get(index) {
                    if let Some(path) = state.active_tab.clone() {
                        let buf = &state.buffers[&path];
                        let cursor_row = buf.cursor_row();
                        let cursor_col = buf.cursor_col();

                        // Build text edit: replace from prefix_start to current cursor
                        let te = led_lsp::TextEdit {
                            start_row: cursor_row,
                            start_col: led_core::Col(comp.prefix_start_col),
                            end_row: cursor_row,
                            end_col: cursor_col,
                            new_text: item
                                .text_edit
                                .as_ref()
                                .map(|e| e.new_text.clone())
                                .unwrap_or_else(|| item.insert_text.clone()),
                        };

                        // Apply edit and move cursor to end of inserted text
                        let (old_lines, old_ver) = state
                            .buffers
                            .get(&path)
                            .map(|b| (b.doc().line_count(), b.version()))
                            .unwrap_or((0, led_core::DocVersion(0)));
                        let edit_row = te.start_row;
                        if let Some(buf) = state.buf_mut(&path) {
                            super::super::apply_text_edits(buf, &[te.clone()]);
                            let new_text = &te.new_text;
                            let newline_count = new_text.chars().filter(|c| *c == '\n').count();
                            let (r, c) = if newline_count == 0 {
                                (te.start_row, te.start_col + new_text.chars().count())
                            } else {
                                (
                                    te.start_row + newline_count,
                                    led_core::Col(
                                        new_text
                                            .rsplit('\n')
                                            .next()
                                            .map(|l| l.chars().count())
                                            .unwrap_or(0),
                                    ),
                                )
                            };
                            buf.set_cursor(r, c, c);
                            close_group_on_move(buf);
                        }
                        mov::shift_annotations(state, &path, *edit_row, old_lines, old_ver);

                        // Apply additional edits (auto-imports etc.)
                        if !item.additional_edits.is_empty() {
                            let (old_lines, old_ver) = state
                                .buffers
                                .get(&path)
                                .map(|b| (b.doc().line_count(), b.version()))
                                .unwrap_or((0, led_core::DocVersion(0)));
                            let edit_row = item
                                .additional_edits
                                .iter()
                                .map(|e| e.start_row)
                                .min()
                                .unwrap_or(led_core::Row(0));
                            if let Some(buf) = state.buf_mut(&path) {
                                super::super::apply_text_edits(buf, &item.additional_edits);
                                close_group_on_move(buf);
                            }
                            mov::shift_annotations(state, &path, *edit_row, old_lines, old_ver);
                        }
                    }
                    // Request resolve for additional edits from server
                    state
                        .lsp_mut()
                        .pending_request
                        .set(Some(LspRequest::CompleteAccept { index }));
                }
                state.lsp_mut().completion = None;
            }
            true
        }
        Action::Abort => {
            state.lsp_mut().completion = None;
            true
        }
        // Printable chars / backspace: pass through to normal editing, then re-filter
        Action::InsertChar(_) | Action::DeleteBackward => false,
        _ => {
            // Any other action dismisses completion
            state.lsp_mut().completion = None;
            false
        }
    }
}

pub fn handle_code_action_picker(state: &mut AppState, action: &Action) -> bool {
    match action {
        Action::MoveUp => {
            let lsp = state.lsp_mut();
            if let Some(ref mut picker) = lsp.code_actions {
                picker.selected = picker.selected.saturating_sub(1);
            }
            true
        }
        Action::MoveDown => {
            let lsp = state.lsp_mut();
            if let Some(ref mut picker) = lsp.code_actions {
                picker.selected = (picker.selected + 1).min(picker.actions.len().saturating_sub(1));
            }
            true
        }
        Action::InsertNewline => {
            // Accept selection
            let index = state
                .lsp
                .code_actions
                .as_ref()
                .map(|p| p.selected)
                .unwrap_or(0);
            state.lsp_mut().code_actions = None;
            state.focus = PanelSlot::Main;
            state
                .lsp_mut()
                .pending_request
                .set(Some(LspRequest::CodeActionSelect { index }));
            true
        }
        Action::Abort => {
            state.lsp_mut().code_actions = None;
            state.focus = PanelSlot::Main;
            true
        }
        _ => true, // Absorb all other actions while picker is open
    }
}

pub fn handle_rename_action(state: &mut AppState, action: &Action) -> bool {
    match action {
        Action::InsertChar(ch) => {
            let lsp = state.lsp_mut();
            if let Some(ref mut rename) = lsp.rename {
                rename.input.insert(rename.cursor, *ch);
                rename.cursor += ch.len_utf8();
            }
            true
        }
        Action::DeleteBackward => {
            let lsp = state.lsp_mut();
            if let Some(ref mut rename) = lsp.rename {
                if rename.cursor > 0 {
                    let ch = rename.input[..rename.cursor]
                        .chars()
                        .last()
                        .map(|c| c.len_utf8())
                        .unwrap_or(1);
                    rename.cursor -= ch;
                    rename.input.remove(rename.cursor);
                }
            }
            true
        }
        Action::InsertNewline => {
            // Submit rename
            let new_name = state
                .lsp
                .rename
                .as_ref()
                .map(|r| r.input.clone())
                .unwrap_or_default();
            state.lsp_mut().rename = None;
            state.focus = PanelSlot::Main;
            if !new_name.is_empty() {
                state
                    .lsp_mut()
                    .pending_request
                    .set(Some(LspRequest::Rename { new_name }));
            }
            true
        }
        Action::Abort => {
            state.lsp_mut().rename = None;
            state.focus = PanelSlot::Main;
            true
        }
        _ => true, // Absorb all other actions while rename overlay is open
    }
}

/// Pure: compute the target position for next/prev issue navigation.
pub fn compute_issue_target(state: &AppState, forward: bool) -> Option<(CanonPath, usize, usize)> {
    use led_core::git::FileStatus;
    use led_lsp::DiagnosticSeverity;

    let cur = cursor_pos(state);

    // Level 1: LSP errors
    if let Some(t) = scan_diagnostics_pure(state, forward, DiagnosticSeverity::Error, &cur) {
        return Some(t);
    }
    // Level 2: LSP warnings
    if let Some(t) = scan_diagnostics_pure(state, forward, DiagnosticSeverity::Warning, &cur) {
        return Some(t);
    }
    // Level 3: Git unstaged
    if let Some(t) = scan_git_changes_pure(
        state,
        forward,
        &[FileStatus::GitWtModified, FileStatus::GitUntracked],
        &cur,
    ) {
        return Some(t);
    }
    // Level 4: Git staged
    scan_git_changes_pure(
        state,
        forward,
        &[FileStatus::GitIndexModified, FileStatus::GitIndexNew],
        &cur,
    )
}

fn scan_diagnostics_pure(
    state: &AppState,
    forward: bool,
    severity: led_lsp::DiagnosticSeverity,
    cur: &Option<Pos<'_>>,
) -> Option<(CanonPath, usize, usize)> {
    let mut best: Option<Pos<'_>> = None;
    let mut wrap: Option<Pos<'_>> = None;
    for buf in state.buffers.values() {
        let Some(path) = buf.path() else { continue };
        for d in buf.status().diagnostics() {
            if d.severity == severity {
                consider(
                    forward,
                    cur,
                    (path, *d.start_row, *d.start_col),
                    &mut best,
                    &mut wrap,
                );
            }
        }
    }
    let &(p, r, c) = best.or(wrap).as_ref()?;
    Some((p.clone(), r, c))
}

fn scan_git_changes_pure(
    state: &AppState,
    forward: bool,
    match_statuses: &[led_core::git::FileStatus],
    cur: &Option<Pos<'_>>,
) -> Option<(CanonPath, usize, usize)> {
    let mut best: Option<Pos<'_>> = None;
    let mut wrap: Option<Pos<'_>> = None;
    for (path, statuses) in state.git.file_statuses.iter() {
        if !statuses.iter().any(|fs| match_statuses.contains(fs)) {
            continue;
        }
        let line_statuses = state
            .buffers
            .get(path)
            .map(|buf| buf.status().git_line_statuses())
            .unwrap_or_default();
        if line_statuses.is_empty() {
            consider(forward, cur, (path, 0, 0), &mut best, &mut wrap);
        } else {
            for ls in line_statuses {
                consider(forward, cur, (path, ls.rows.start, 0), &mut best, &mut wrap);
            }
        }
    }
    let &(p, r, c) = best.or(wrap).as_ref()?;
    Some((p.clone(), r, c))
}

pub(super) fn navigate_issue(state: &mut AppState, forward: bool) {
    use led_core::git::FileStatus;
    use led_lsp::DiagnosticSeverity;

    // Level 1: LSP errors
    if scan_diagnostics(state, forward, DiagnosticSeverity::Error) {
        return;
    }

    // Level 2: LSP warnings
    if scan_diagnostics(state, forward, DiagnosticSeverity::Warning) {
        return;
    }

    // Level 3: Git unstaged (worktree modified or untracked)
    if scan_git_changes(
        state,
        forward,
        &[FileStatus::GitWtModified, FileStatus::GitUntracked],
    ) {
        return;
    }

    // Level 4: Git staged (index modified or new)
    scan_git_changes(
        state,
        forward,
        &[FileStatus::GitIndexModified, FileStatus::GitIndexNew],
    );
}

type Pos<'a> = (&'a CanonPath, usize, usize);

/// Track the best candidate and wrap-around target in a single pass.
fn consider<'a>(
    forward: bool,
    cur: &Option<Pos<'_>>,
    pos: Pos<'a>,
    best: &mut Option<Pos<'a>>,
    wrap: &mut Option<Pos<'a>>,
) {
    if forward {
        if cur.map_or(true, |c| pos > c) && best.map_or(true, |b| pos < b) {
            *best = Some(pos);
        }
        if wrap.map_or(true, |w| pos < w) {
            *wrap = Some(pos);
        }
    } else {
        if cur.map_or(true, |c| pos < c) && best.map_or(true, |b| pos > b) {
            *best = Some(pos);
        }
        if wrap.map_or(true, |w| pos > w) {
            *wrap = Some(pos);
        }
    }
}

/// Single-pass scan of diagnostics at the given severity. Returns true if navigated.
fn scan_diagnostics(
    state: &mut AppState,
    forward: bool,
    severity: led_lsp::DiagnosticSeverity,
) -> bool {
    let cur = cursor_pos(state);
    let mut best: Option<Pos<'_>> = None;
    let mut wrap: Option<Pos<'_>> = None;

    for buf in state.buffers.values() {
        let Some(path) = buf.path() else { continue };
        for d in buf.status().diagnostics() {
            if d.severity == severity {
                consider(
                    forward,
                    &cur,
                    (path, *d.start_row, *d.start_col),
                    &mut best,
                    &mut wrap,
                );
            }
        }
    }

    let Some(&(p, r, c)) = best.or(wrap).as_ref() else {
        return false;
    };
    let target = p.clone();
    navigate_to_position(state, target, r, c);
    true
}

/// Single-pass scan of git change hunks for files matching the given statuses.
/// Returns true if navigated.
fn scan_git_changes(
    state: &mut AppState,
    forward: bool,
    match_statuses: &[led_core::git::FileStatus],
) -> bool {
    let cur = cursor_pos(state);
    let mut best: Option<Pos<'_>> = None;
    let mut wrap: Option<Pos<'_>> = None;

    for (path, statuses) in state.git.file_statuses.iter() {
        if !statuses.iter().any(|fs| match_statuses.contains(fs)) {
            continue;
        }
        let line_statuses = state
            .buffers
            .get(path)
            .map(|buf| buf.status().git_line_statuses())
            .unwrap_or(&[]);

        if line_statuses.is_empty() {
            consider(forward, &cur, (path, 0, 0), &mut best, &mut wrap);
        } else {
            for ls in line_statuses {
                consider(
                    forward,
                    &cur,
                    (path, ls.rows.start, 0),
                    &mut best,
                    &mut wrap,
                );
            }
        }
    }

    let Some(&(p, r, c)) = best.or(wrap).as_ref() else {
        return false;
    };
    let target = p.clone();
    navigate_to_position(state, target, r, c);
    true
}

/// Current cursor as (path, row, col) — all borrowed from state.
fn cursor_pos(state: &AppState) -> Option<Pos<'_>> {
    let path = state.active_tab.as_ref()?;
    let buf = state.buffers.get(path)?;
    Some((path, buf.cursor_row().0, buf.cursor_col().0))
}

/// Navigate to a specific (path, row, col), handling same-buffer, cross-tab, and file-open cases.
fn navigate_to_position(
    state: &mut AppState,
    target_path: CanonPath,
    target_row: usize,
    target_col: usize,
) {
    // Same buffer — just move the cursor.
    if state.active_tab.as_ref() == Some(&target_path) {
        let dims = state.dims;
        if let Some(buf) = state.buf_mut(&target_path) {
            close_group_on_move(buf);
            buf.set_cursor(
                led_core::Row(target_row),
                led_core::Col(target_col),
                led_core::Col(target_col),
            );
            if let Some(dims) = dims {
                let (sr, ssl) = mov::adjust_scroll(buf, &dims);
                buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            }
        }
        return;
    }

    // Different file — already open in a tab.
    if state.tabs.iter().any(|t| *t.path() == target_path) {
        log::debug!(
            "[issue] → different file, already open: {}",
            target_path.display()
        );
        let half = state.dims.map_or(10, |d| d.buffer_height() / 2);
        if let Some(buf) = state.buf_mut(&target_path) {
            let r = target_row.min(buf.doc().line_count().saturating_sub(1));
            buf.set_cursor(
                led_core::Row(r),
                led_core::Col(target_col),
                led_core::Col(target_col),
            );
            buf.set_scroll(led_core::Row(r.saturating_sub(half)), led_core::SubLine(0));
        }
        state.active_tab = Some(target_path);
        reveal_active_buffer(state);
        return;
    }

    // Not open — request open.
    log::debug!(
        "[issue] → different file, not open: {}",
        target_path.display()
    );
    super::super::request_open(state, target_path.clone(), false);
    if let Some(tab) = state.tabs.iter_mut().find(|t| *t.path() == target_path) {
        let half = state.dims.map_or(10, |d| d.buffer_height() / 2);
        tab.set_cursor(
            led_core::Row(target_row),
            led_core::Col(target_col),
            led_core::Row(target_row.saturating_sub(half)),
        );
    }
    state.active_tab = Some(target_path);
}

pub(super) fn open_rename_overlay(state: &mut AppState) {
    if let Some(ref path) = state.active_tab {
        if let Some(buf) = state.buffers.get(path) {
            let word = word_under_cursor(buf);
            let cursor = word.len();
            state.lsp_mut().rename = Some(RenameState {
                input: word,
                cursor,
            });
            state.focus = PanelSlot::Overlay;
        }
    }
}
