//! Find-file / save-as overlay dispatch.
//!
//! The overlay owns a set of overlay-only commands plus interception
//! of most normal editor commands while `Option<FindFileState>` is
//! `Some`. The surface:
//!
//! - **Activation**: Open mode seeds input with parent-dir + `/` of
//!   the active buffer (home-abbreviated); SaveAs seeds with the
//!   active buffer's full path. Both fall back to the workspace root
//!   when no buffer is active.
//! - **Input editing**: InsertChar / DeleteBack / DeleteForward,
//!   CursorLeft / Right + Home / End, KillLine. Each edit resets
//!   the arrow-selection and queues a fresh `FsFindFile` request.
//! - **Tab completion**: single-match completes fully; single-dir
//!   descends; multi-match extends input to the case-insensitive
//!   longest common prefix.
//! - **Arrow navigation**: CursorUp/Down cycle `selected`
//!   (wrapping), show the side panel, rewrite `input` to the
//!   selected target, and create a preview tab for file entries.
//!   The first preview captures `tabs.active` so deactivate can
//!   restore it.
//! - **Enter**: Open mode paths A (arrow selection), B (exact
//!   match), C (create/open). SaveAs commit queues a
//!   `pending_save_as` entry consumed by the save query.
//! - **Abort**: deactivates + closes any preview + restores the
//!   previously-active tab.

use led_state_browser::FsTree;
use led_state_find_file::{FindFileMode, FindFileState, abbreviate_home, expand_path};
use led_state_tabs::Tabs;

use crate::keymap::Command;

use super::DispatchOutcome;
use super::shared::open_or_focus_tab;

/// Enter Open mode. No-op if an overlay is already active — matches
/// legacy's "cannot nest find-file".
pub(super) fn activate_open(
    find_file: &mut Option<FindFileState>,
    tabs: &Tabs,
    fs: &FsTree,
) {
    if find_file.is_some() {
        return;
    }
    let input = compute_open_input(tabs, fs);
    let mut state = FindFileState::open(input);
    state.queue_request();
    *find_file = Some(state);
}

/// Enter SaveAs mode. No-op when already active.
pub(super) fn activate_save_as(
    find_file: &mut Option<FindFileState>,
    tabs: &Tabs,
    fs: &FsTree,
) {
    if find_file.is_some() {
        return;
    }
    let input = compute_save_as_input(tabs, fs);
    let mut state = FindFileState::save_as(input);
    state.queue_request();
    *find_file = Some(state);
}

/// Close the overlay. Closes the preview tab (if any) and restores
/// the previously-active tab. Idempotent.
pub(super) fn deactivate(find_file: &mut Option<FindFileState>, tabs: &mut Tabs) {
    if let Some(state) = find_file.as_ref() {
        close_preview(tabs, state.previous_tab);
    }
    *find_file = None;
}

/// Remove any preview tab. When `restore_to` is `Some`, promote that
/// tab to active; otherwise clear `active` if the preview was the
/// active tab. Matches legacy `close_preview` semantics.
fn close_preview(tabs: &mut Tabs, restore_to: Option<led_state_tabs::TabId>) {
    // Find the preview tab (there's at most one at any time).
    let Some(preview_idx) = tabs.open.iter().position(|t| t.preview) else {
        return;
    };
    let preview_id = tabs.open[preview_idx].id;
    tabs.open.remove(preview_idx);
    // Restore the previously-active tab if we have its id and it
    // still exists. Otherwise pick any remaining tab, or clear.
    if let Some(prev) = restore_to
        && tabs.open.iter().any(|t| t.id == prev)
    {
        tabs.active = Some(prev);
    } else if tabs.active == Some(preview_id) {
        tabs.active = tabs.open.front().map(|t| t.id);
    }
}

/// Route a `Command` through the find-file overlay when active.
///
/// Returns `Some(DispatchOutcome::Continue)` when the overlay
/// consumed the command (edit, navigation, deactivation, explicit
/// absorb). Returns `None` when the command should pass through to
/// the normal dispatch path — currently just `Quit`, so
/// `Ctrl-X Ctrl-C` still exits the editor even with the overlay
/// open.
///
/// Stages 5+ will fill in `FindFileTabComplete` (LCP), `CursorUp`/
/// `CursorDown` (arrow nav + preview), and `InsertNewline` (Enter
/// commit). Today those variants absorb silently so the overlay
/// doesn't accidentally edit the buffer under it.
pub(super) fn run_overlay_command(
    cmd: Command,
    find_file: &mut Option<FindFileState>,
    tabs: &mut Tabs,
    edits: &mut led_state_buffer_edits::BufferEdits,
) -> Option<DispatchOutcome> {
    find_file.as_ref()?;
    match cmd {
        Command::InsertChar(c) => insert_char(find_file.as_mut()?, c),
        Command::DeleteBack => delete_back(find_file.as_mut()?),
        Command::DeleteForward => delete_forward(find_file.as_mut()?),
        Command::CursorLeft => move_left(find_file.as_mut()?),
        Command::CursorRight => move_right(find_file.as_mut()?),
        Command::CursorLineStart => find_file.as_mut()?.cursor = 0,
        Command::CursorLineEnd => {
            let s = find_file.as_mut()?;
            s.cursor = s.input.len();
        }
        Command::KillLine => kill_line(find_file.as_mut()?),
        Command::FindFileTabComplete => tab_complete(find_file.as_mut()?),
        Command::CursorUp => move_selection(find_file, tabs, -1),
        Command::CursorDown => move_selection(find_file, tabs, 1),
        Command::InsertNewline => handle_enter(find_file, tabs, edits),
        Command::Abort => deactivate(find_file, tabs),
        // Not-yet-implemented overlay commands. Absorbed so the key
        // doesn't leak to the buffer below.
        Command::CursorPageUp
        | Command::CursorPageDown
        | Command::CursorFileStart
        | Command::CursorFileEnd
        | Command::CursorWordLeft
        | Command::CursorWordRight => {}
        // Quit passes through so the user can still ctrl+x ctrl+c
        // out of the editor.
        Command::Quit => return None,
        // Every other command is absorbed while the overlay owns
        // focus. Legacy's "unbound action deactivates" nuance lands
        // with the later stages.
        _ => {}
    }
    Some(DispatchOutcome::Continue)
}

/// Enter / commit. Paths per legacy `handle_enter_open` /
/// `handle_enter_save_as`, simplified for M12 bootstrap:
///
/// - **Open mode, trailing slash**: no-op (user is exploring a
///   directory; dir-descent-on-Enter lands with Stage 5/6).
/// - **Open mode, matching completion**: if a file, open/focus it
///   and deactivate. Dir → no-op for now.
/// - **Open mode, non-empty input with no match (path C)**:
///   canonicalize + open (creates a fresh tab at that path; the
///   file-read driver will treat a missing path as "create on next
///   save").
/// - **SaveAs**: set `pending_save_as` (not wired yet — Stage 7b).
fn handle_enter(
    find_file: &mut Option<FindFileState>,
    tabs: &mut Tabs,
    edits: &mut led_state_buffer_edits::BufferEdits,
) {
    let Some(state) = find_file.as_ref() else {
        return;
    };
    if state.input.is_empty() {
        return;
    }
    match state.mode {
        FindFileMode::Open => handle_enter_open(find_file, tabs),
        FindFileMode::SaveAs => handle_enter_save_as(find_file, tabs, edits),
    }
}

/// SaveAs commit: write the active buffer's content to the user's
/// target path. The active tab stays at its original path — SaveAs
/// is a "save a copy to", not a rename. The driver write triggers
/// a `WorkspaceClearUndo` and `FileOpen` trace pair on completion
/// (matching legacy's `DocStoreIn::SavedAs` handling).
///
/// Aborts silently when:
/// - Input is empty or ends with `/` (not a file path).
/// - No active tab (nowhere to pull bytes from).
/// - The active buffer hasn't been loaded yet (the `from` side of the
///   save would have no rope).
fn handle_enter_save_as(
    find_file: &mut Option<FindFileState>,
    tabs: &mut Tabs,
    edits: &mut led_state_buffer_edits::BufferEdits,
) {
    let Some(state) = find_file.as_ref() else {
        return;
    };
    if state.input.ends_with('/') {
        return;
    }
    let Some(active_id) = tabs.active else {
        return;
    };
    let Some(active_tab) = tabs.open.iter().find(|t| t.id == active_id) else {
        return;
    };
    let from = active_tab.path.clone();
    if !edits.buffers.contains_key(&from) {
        return;
    }
    let expanded = led_state_find_file::expand_path(&state.input);
    let to = led_core::UserPath::new(expanded).canonicalize();
    edits.pending_save_as.insert(from, to);
    deactivate(find_file, tabs);
}

/// Arrow-driven selection through completions.
///
/// `delta` is +1 for down, -1 for up; the selection wraps at both
/// ends so the user can cycle. On selection change:
/// - `input` is rewritten to `dir_prefix(base_input) + selected.name`
///   so the user sees what will commit on Enter.
/// - `show_side = true` — arrow nav is the primary way to browse.
/// - For file entries (`!is_dir`), a preview tab is created/updated
///   at that path. The first arrow captures `tabs.active` into
///   `previous_tab` so `deactivate` can restore it.
fn move_selection(find_file: &mut Option<FindFileState>, tabs: &mut Tabs, delta: isize) {
    let Some(state) = find_file.as_mut() else {
        return;
    };
    if state.completions.is_empty() {
        return;
    }
    let n = state.completions.len();
    let next = match state.selected {
        None => {
            if delta >= 0 {
                0
            } else {
                n - 1
            }
        }
        Some(i) => {
            let signed = i as isize + delta;
            let wrapped = signed.rem_euclid(n as isize);
            wrapped as usize
        }
    };
    state.selected = Some(next);
    state.show_side = true;

    // Rewrite input to dir_prefix(base_input) + selected.name so the
    // status bar reflects the arrow-selected target.
    let base = led_state_find_file::dir_prefix(&state.base_input).to_string();
    let mut new_input = base;
    new_input.push_str(&state.completions[next].name);
    state.input = new_input;
    state.cursor = state.input.len();

    // Preview non-directory selections.
    let entry = &state.completions[next];
    if !entry.is_dir {
        // First preview: remember the currently-active tab so we
        // can restore on deactivate.
        if state.previous_tab.is_none() {
            state.previous_tab = tabs.active;
        }
        let path = entry.full.clone();
        open_or_focus_tab(tabs, &path, /* promote= */ false);
    }
}

fn handle_enter_open(find_file: &mut Option<FindFileState>, tabs: &mut Tabs) {
    let Some(state) = find_file.as_ref() else {
        return;
    };

    // Path A — arrow-driven selection. If a completion is
    // highlighted, act on that entry regardless of whether `input`
    // exactly matches its path.
    if let Some(idx) = state.selected
        && let Some(entry) = state.completions.get(idx)
    {
        let is_dir = entry.is_dir;
        let full = entry.full.clone();
        let name = entry.name.clone();
        if is_dir {
            descend(find_file, &name);
        } else {
            open_or_focus_tab(tabs, &full, /* promote= */ true);
            deactivate(find_file, tabs);
        }
        return;
    }

    // Trailing slash + no selection = no-op: user is exploring a
    // directory, there's no target to commit. Matches legacy.
    if state.input.ends_with('/') {
        return;
    }

    let expanded = expand_path(&state.input);
    let canon = led_core::UserPath::new(expanded).canonicalize();

    // Path B — exact match against a completion (no arrow
    // selection). Dir → descend; file → open.
    if let Some(entry) = state.completions.iter().find(|e| e.full == canon) {
        let is_dir = entry.is_dir;
        let full = entry.full.clone();
        let name = entry.name.clone();
        if is_dir {
            descend(find_file, &name);
        } else {
            open_or_focus_tab(tabs, &full, /* promote= */ true);
            deactivate(find_file, tabs);
        }
        return;
    }

    // Path C — no selection, no exact match: open (or create) the
    // typed path. The file-read driver handles missing-file-on-save
    // creation; for the read, a missing file surfaces as an error
    // state in the buffer.
    open_or_focus_tab(tabs, &canon, /* promote= */ true);
    deactivate(find_file, tabs);
}

/// Descend into a directory: rewrite `input` to
/// `dir_prefix(base_input) + dir_name` (the display name already
/// carries the trailing `/`) and re-request completions for the
/// new directory.
fn descend(find_file: &mut Option<FindFileState>, dir_name: &str) {
    let Some(state) = find_file.as_mut() else {
        return;
    };
    let base = led_state_find_file::dir_prefix(&state.base_input).to_string();
    state.input = base;
    state.input.push_str(dir_name);
    state.cursor = state.input.len();
    state.base_input = state.input.clone();
    state.reset_selection();
    state.queue_request();
}

// ── Input-editing primitives ───────────────────────────────────────────
//
// Each mutator updates `input`/`cursor`, resets the arrow-driven
// selection (any edit cancels a preview), and re-arms
// `pending_find_file_list` so the next tick fires a fresh
// `FsFindFile` request for the new prefix.

fn insert_char(s: &mut FindFileState, c: char) {
    s.input.insert(s.cursor, c);
    s.cursor += c.len_utf8();
    s.reset_selection();
    s.queue_request();
}

fn delete_back(s: &mut FindFileState) {
    if s.cursor == 0 {
        return;
    }
    let prev = prev_char_boundary(&s.input, s.cursor);
    s.input.replace_range(prev..s.cursor, "");
    s.cursor = prev;
    s.reset_selection();
    s.queue_request();
}

fn delete_forward(s: &mut FindFileState) {
    if s.cursor >= s.input.len() {
        return;
    }
    let next = next_char_boundary(&s.input, s.cursor);
    s.input.replace_range(s.cursor..next, "");
    s.reset_selection();
    s.queue_request();
}

fn move_left(s: &mut FindFileState) {
    if s.cursor == 0 {
        return;
    }
    s.cursor = prev_char_boundary(&s.input, s.cursor);
}

fn move_right(s: &mut FindFileState) {
    if s.cursor >= s.input.len() {
        return;
    }
    s.cursor = next_char_boundary(&s.input, s.cursor);
}

fn kill_line(s: &mut FindFileState) {
    if s.cursor >= s.input.len() {
        return;
    }
    s.input.truncate(s.cursor);
    s.reset_selection();
    s.queue_request();
}

/// Tab-completion / LCP, matching legacy `tab_complete`:
///
/// - **Trailing slash + completions**: show the side panel without
///   auto-selecting (user is exploring; next keystroke keeps
///   filtering).
/// - **Single match, directory, input doesn't yet end `/`**: append
///   the name (which already has `/`), re-request — descends.
/// - **Single match, any other case**: complete fully to the match.
/// - **Multiple matches**: set `show_side`; extend input to the
///   case-insensitive longest common prefix across the matches'
///   leaf names (trailing `/` stripped for comparison).
/// - **No matches**: no-op.
fn tab_complete(s: &mut FindFileState) {
    use led_state_find_file::dir_prefix;

    // Trailing slash + completions = explore mode.
    if s.input.ends_with('/') && !s.completions.is_empty() {
        s.show_side = true;
        s.selected = None;
        return;
    }
    match s.completions.len() {
        0 => {}
        1 => {
            let only = &s.completions[0];
            let is_dir = only.is_dir;
            // Single match: complete. If it's a directory the
            // display name already carries `/` so "append name"
            // descends.
            let base = dir_prefix(&s.base_input).to_string();
            let mut new_input = base;
            new_input.push_str(&only.name);
            let changed = new_input != s.input;
            s.input = new_input;
            s.cursor = s.input.len();
            if changed {
                s.reset_selection();
                // Only re-fire a completion request when we've
                // descended into a directory. A file completion
                // just fills the input; pressing Enter from here
                // is what actually opens it.
                if is_dir {
                    s.queue_request();
                }
            }
        }
        _ => {
            // Multi-match: extend to case-insensitive LCP.
            let names: Vec<&str> = s
                .completions
                .iter()
                .map(|e| e.name.trim_end_matches('/'))
                .collect();
            let lcp = longest_common_prefix_ci(&names);
            let base = dir_prefix(&s.base_input).to_string();
            let mut new_input = base;
            new_input.push_str(lcp);
            s.show_side = true;
            if new_input != s.input {
                s.input = new_input;
                s.cursor = s.input.len();
                s.reset_selection();
                s.queue_request();
                // reset_selection cleared show_side — re-arm it so
                // the panel stays open after the input grew.
                s.show_side = true;
            }
        }
    }
}

/// Case-insensitive longest common prefix across all `names`. Returns
/// an empty `&str` slice when inputs disagree on the first char.
/// Slice is borrowed from `names[0]` so no allocation is needed.
fn longest_common_prefix_ci<'a>(names: &[&'a str]) -> &'a str {
    let Some((first, rest)) = names.split_first() else {
        return "";
    };
    // Walk char-by-char; stop when any other name diverges on the
    // lowercased char. The slice index tracks bytes of `first`.
    let mut prefix_bytes = 0;
    for (byte_idx, c) in first.char_indices() {
        let c_lower = c.to_ascii_lowercase();
        let all_match = rest.iter().all(|name| {
            name[byte_idx..]
                .chars()
                .next()
                .is_some_and(|nc| nc.to_ascii_lowercase() == c_lower)
        });
        if all_match {
            prefix_bytes = byte_idx + c.len_utf8();
        } else {
            break;
        }
    }
    &first[..prefix_bytes]
}

fn prev_char_boundary(s: &str, byte_pos: usize) -> usize {
    s[..byte_pos]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_char_boundary(s: &str, byte_pos: usize) -> usize {
    match s[byte_pos..].chars().next() {
        Some(c) => byte_pos + c.len_utf8(),
        None => byte_pos,
    }
}

fn compute_open_input(tabs: &Tabs, fs: &FsTree) -> String {
    let parent = active_path(tabs).and_then(|p| {
        p.as_path()
            .parent()
            .map(|pp| pp.to_path_buf())
    });
    let dir = parent.or_else(|| fs.root.as_ref().map(|r| r.as_path().to_path_buf()));
    let mut s = match dir {
        Some(p) => abbreviate_home(&p.to_string_lossy()),
        None => "/".to_string(),
    };
    if !s.ends_with('/') {
        s.push('/');
    }
    s
}

fn compute_save_as_input(tabs: &Tabs, fs: &FsTree) -> String {
    match active_path(tabs) {
        Some(p) => abbreviate_home(&p.as_path().to_string_lossy()),
        None => {
            let mut s = fs
                .root
                .as_ref()
                .map(|r| abbreviate_home(&r.as_path().to_string_lossy()))
                .unwrap_or_else(|| "/".to_string());
            if !s.ends_with('/') {
                s.push('/');
            }
            s
        }
    }
}

fn active_path(tabs: &Tabs) -> Option<&led_core::CanonPath> {
    let id = tabs.active?;
    tabs.open.iter().find(|t| t.id == id).map(|t| &t.path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;
    use led_state_tabs::{Tab, TabId};

    fn canon(s: &str) -> led_core::CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn tabs_with_active(path: &str) -> Tabs {
        let mut t = Tabs::default();
        t.open.push_back(Tab {
            id: TabId(1),
            path: canon(path),
            ..Default::default()
        });
        t.active = Some(TabId(1));
        t
    }

    #[test]
    fn activate_open_uses_parent_dir_of_active_buffer() {
        let tabs = tabs_with_active("/tmp/xyz/a.txt");
        let fs = FsTree::default();
        let mut ff = None;
        activate_open(&mut ff, &tabs, &fs);
        let s = ff.expect("activated");
        assert_eq!(s.input, "/tmp/xyz/");
        // Cursor sits at end-of-input for immediate editing.
        assert_eq!(s.cursor, s.input.len());
    }

    #[test]
    fn activate_save_as_uses_full_active_path() {
        let tabs = tabs_with_active("/tmp/xyz/a.txt");
        let fs = FsTree::default();
        let mut ff = None;
        activate_save_as(&mut ff, &tabs, &fs);
        let s = ff.expect("activated");
        assert_eq!(s.input, "/tmp/xyz/a.txt");
    }

    #[test]
    fn activate_open_no_active_falls_back_to_fs_root() {
        let tabs = Tabs::default();
        let fs = FsTree {
            root: Some(canon("/workspace")),
            ..Default::default()
        };
        let mut ff = None;
        activate_open(&mut ff, &tabs, &fs);
        let s = ff.expect("activated");
        assert_eq!(s.input, "/workspace/");
    }

    #[test]
    fn activate_is_no_op_when_already_active() {
        let tabs = tabs_with_active("/tmp/a.txt");
        let fs = FsTree::default();
        let mut ff = Some(FindFileState::open("preserved/".to_string()));
        activate_open(&mut ff, &tabs, &fs);
        assert_eq!(ff.as_ref().unwrap().input, "preserved/");
    }

    #[test]
    fn deactivate_clears_state() {
        let mut ff = Some(FindFileState::open("x/".into()));
        deactivate(&mut ff, &mut Tabs::default());
        assert!(ff.is_none());
    }

    // ── Input editing ──────────────────────────────────────────────

    fn overlay(input: &str, cursor: usize) -> Option<FindFileState> {
        let mut s = FindFileState::open(input.to_string());
        s.cursor = cursor;
        // Activation pre-fires a completion request; tests want to
        // observe the list re-filling after each edit.
        s.pending_find_file_list.clear();
        Some(s)
    }

    #[test]
    fn insert_char_extends_input_and_rearms_pending() {
        let mut ff = overlay("/tmp/", 5);
        run_overlay_command(Command::InsertChar('a'), &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        let s = ff.as_ref().unwrap();
        assert_eq!(s.input, "/tmp/a");
        assert_eq!(s.cursor, 6);
        assert_eq!(s.pending_find_file_list.len(), 1);
    }

    #[test]
    fn delete_back_at_end_of_input() {
        let mut ff = overlay("/tmp/a", 6);
        run_overlay_command(Command::DeleteBack, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        assert_eq!(ff.as_ref().unwrap().input, "/tmp/");
        assert_eq!(ff.as_ref().unwrap().cursor, 5);
    }

    #[test]
    fn delete_back_at_start_is_noop() {
        let mut ff = overlay("/tmp/", 0);
        run_overlay_command(Command::DeleteBack, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        assert_eq!(ff.as_ref().unwrap().input, "/tmp/");
        assert!(ff.as_ref().unwrap().pending_find_file_list.is_empty());
    }

    #[test]
    fn delete_forward_at_middle() {
        let mut ff = overlay("/tmp/ab", 5);
        run_overlay_command(Command::DeleteForward, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        assert_eq!(ff.as_ref().unwrap().input, "/tmp/b");
    }

    #[test]
    fn cursor_left_and_right_walk_char_boundaries() {
        let mut ff = overlay("/ä/", 3); // 'ä' is 2 bytes at byte 1..3
        run_overlay_command(Command::CursorLeft, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        assert_eq!(ff.as_ref().unwrap().cursor, 1);
        run_overlay_command(Command::CursorRight, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        assert_eq!(ff.as_ref().unwrap().cursor, 3);
    }

    #[test]
    fn line_start_and_end() {
        let mut ff = overlay("/tmp/", 2);
        run_overlay_command(Command::CursorLineStart, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        assert_eq!(ff.as_ref().unwrap().cursor, 0);
        run_overlay_command(Command::CursorLineEnd, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        assert_eq!(ff.as_ref().unwrap().cursor, 5);
    }

    #[test]
    fn kill_line_truncates_at_cursor() {
        let mut ff = overlay("/tmp/abc", 5);
        run_overlay_command(Command::KillLine, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        assert_eq!(ff.as_ref().unwrap().input, "/tmp/");
        assert_eq!(ff.as_ref().unwrap().pending_find_file_list.len(), 1);
    }

    #[test]
    fn abort_closes_the_overlay() {
        let mut ff = overlay("/tmp/", 5);
        let outcome = run_overlay_command(Command::Abort, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        assert_eq!(outcome, Some(DispatchOutcome::Continue));
        assert!(ff.is_none());
    }

    #[test]
    fn quit_passes_through() {
        let mut ff = overlay("/tmp/", 5);
        let outcome = run_overlay_command(Command::Quit, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        // None == "fall through to the normal dispatch path", so
        // the outer `run_command` turns the Quit into a real exit.
        assert!(outcome.is_none());
        assert!(ff.is_some());
    }

    #[test]
    fn inactive_overlay_passes_everything_through() {
        let mut ff: Option<FindFileState> = None;
        assert!(run_overlay_command(Command::InsertChar('a'), &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default()).is_none());
    }

    #[test]
    fn enter_on_non_trailing_path_opens_and_deactivates() {
        let mut ff = overlay("/tmp/newfile.txt", 16);
        let mut tabs = Tabs::default();
        run_overlay_command(Command::InsertNewline, &mut ff, &mut tabs, &mut led_state_buffer_edits::BufferEdits::default());
        assert!(ff.is_none(), "overlay closes on commit");
        assert_eq!(tabs.open.len(), 1);
        assert_eq!(tabs.active, Some(tabs.open[0].id));
        // Real tab, not preview.
        assert!(!tabs.open[0].preview);
    }

    #[test]
    fn enter_on_trailing_slash_is_noop() {
        let mut ff = overlay("/tmp/", 5);
        let mut tabs = Tabs::default();
        run_overlay_command(Command::InsertNewline, &mut ff, &mut tabs, &mut led_state_buffer_edits::BufferEdits::default());
        // Overlay stays open; no tab created.
        assert!(ff.is_some());
        assert!(tabs.open.is_empty());
    }

    #[test]
    fn enter_on_empty_input_is_noop() {
        let mut ff = overlay("", 0);
        let mut tabs = Tabs::default();
        run_overlay_command(Command::InsertNewline, &mut ff, &mut tabs, &mut led_state_buffer_edits::BufferEdits::default());
        assert!(ff.is_some());
        assert!(tabs.open.is_empty());
    }

    // ── Tab completion / LCP ───────────────────────────────────────

    fn entry(name: &str, is_dir: bool) -> led_state_find_file::FindFileEntry {
        let display = if is_dir && !name.ends_with('/') {
            format!("{name}/")
        } else {
            name.to_string()
        };
        led_state_find_file::FindFileEntry {
            name: display,
            full: canon(&format!("/tmp/{name}")),
            is_dir,
        }
    }

    #[test]
    fn tab_lcp_empty_completions_is_noop() {
        let mut ff = overlay("/tmp/xyz", 8);
        let before = ff.as_ref().unwrap().input.clone();
        run_overlay_command(Command::FindFileTabComplete, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        assert_eq!(ff.as_ref().unwrap().input, before);
    }

    #[test]
    fn tab_lcp_single_file_match_completes_fully() {
        let mut ff = overlay("/tmp/mai", 8);
        ff.as_mut().unwrap().completions = vec![entry("main.rs", false)];
        run_overlay_command(Command::FindFileTabComplete, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        assert_eq!(ff.as_ref().unwrap().input, "/tmp/main.rs");
    }

    #[test]
    fn tab_lcp_single_dir_match_descends_with_trailing_slash() {
        let mut ff = overlay("/tmp/sr", 7);
        ff.as_mut().unwrap().completions = vec![entry("src", true)];
        run_overlay_command(Command::FindFileTabComplete, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        // `name` for dirs already carries the trailing `/`.
        assert_eq!(ff.as_ref().unwrap().input, "/tmp/src/");
        // Descent re-arms the request queue.
        assert_eq!(
            ff.as_ref().unwrap().pending_find_file_list.len(),
            1,
            "descent should queue a fresh listing",
        );
    }

    #[test]
    fn tab_lcp_multi_match_extends_to_longest_common_prefix_and_shows_panel() {
        let mut ff = overlay("/tmp/m", 6);
        ff.as_mut().unwrap().completions = vec![
            entry("main.rs", false),
            entry("mailbox.rs", false),
            entry("make.rs", false),
        ];
        run_overlay_command(Command::FindFileTabComplete, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        // LCP of "main.rs", "mailbox.rs", "make.rs" is "ma".
        assert_eq!(ff.as_ref().unwrap().input, "/tmp/ma");
        assert!(ff.as_ref().unwrap().show_side);
    }

    #[test]
    fn tab_lcp_trailing_slash_just_shows_panel() {
        let mut ff = overlay("/tmp/", 5);
        ff.as_mut().unwrap().completions = vec![entry("a.rs", false), entry("b.rs", false)];
        run_overlay_command(Command::FindFileTabComplete, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        assert!(ff.as_ref().unwrap().show_side);
        assert_eq!(ff.as_ref().unwrap().input, "/tmp/");
        assert!(ff.as_ref().unwrap().selected.is_none());
    }

    // ── Arrow navigation + preview ─────────────────────────────────

    #[test]
    fn move_down_selects_first_completion_and_previews_file() {
        let mut ff = overlay("/tmp/", 5);
        ff.as_mut().unwrap().completions = vec![entry("a.rs", false), entry("b.rs", false)];
        let mut tabs = Tabs::default();
        run_overlay_command(Command::CursorDown, &mut ff, &mut tabs, &mut led_state_buffer_edits::BufferEdits::default());
        let s = ff.as_ref().unwrap();
        assert_eq!(s.selected, Some(0));
        assert!(s.show_side);
        // Input rewritten to dir_prefix + selected name.
        assert_eq!(s.input, "/tmp/a.rs");
        // Preview tab exists.
        assert_eq!(tabs.open.len(), 1);
        assert_eq!(tabs.open[0].path, canon("/tmp/a.rs"));
        assert!(tabs.open[0].preview);
    }

    #[test]
    fn move_up_from_none_wraps_to_last() {
        let mut ff = overlay("/tmp/", 5);
        ff.as_mut().unwrap().completions = vec![entry("a.rs", false), entry("b.rs", false)];
        run_overlay_command(Command::CursorUp, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        assert_eq!(ff.as_ref().unwrap().selected, Some(1));
    }

    #[test]
    fn move_down_wraps_at_last_back_to_zero() {
        let mut ff = overlay("/tmp/", 5);
        ff.as_mut().unwrap().selected = Some(1);
        ff.as_mut().unwrap().completions = vec![entry("a.rs", false), entry("b.rs", false)];
        run_overlay_command(Command::CursorDown, &mut ff, &mut Tabs::default(), &mut led_state_buffer_edits::BufferEdits::default());
        assert_eq!(ff.as_ref().unwrap().selected, Some(0));
    }

    #[test]
    fn move_selection_on_dir_entry_skips_preview() {
        let mut ff = overlay("/tmp/", 5);
        ff.as_mut().unwrap().completions = vec![entry("src", true)];
        let mut tabs = Tabs::default();
        run_overlay_command(Command::CursorDown, &mut ff, &mut tabs, &mut led_state_buffer_edits::BufferEdits::default());
        assert_eq!(ff.as_ref().unwrap().selected, Some(0));
        // Dir preview isn't meaningful → no preview tab created.
        assert!(tabs.open.is_empty());
    }

    #[test]
    fn deactivate_closes_preview_and_restores_previous_active() {
        use led_state_tabs::{Tab, TabId};
        let mut ff = overlay("/tmp/", 5);
        let mut tabs = Tabs::default();
        let prev_id = TabId(42);
        tabs.open.push_back(Tab {
            id: prev_id,
            path: canon("/tmp/real.rs"),
            ..Default::default()
        });
        tabs.active = Some(prev_id);
        ff.as_mut().unwrap().completions = vec![entry("a.rs", false)];
        // Arrow-down creates a preview + captures previous_tab.
        run_overlay_command(Command::CursorDown, &mut ff, &mut tabs, &mut led_state_buffer_edits::BufferEdits::default());
        assert_eq!(tabs.open.len(), 2);
        assert!(tabs.open.iter().any(|t| t.preview));
        assert_eq!(ff.as_ref().unwrap().previous_tab, Some(prev_id));
        // Abort closes the preview and restores the original tab.
        run_overlay_command(Command::Abort, &mut ff, &mut tabs, &mut led_state_buffer_edits::BufferEdits::default());
        assert!(ff.is_none());
        assert_eq!(tabs.open.len(), 1);
        assert_eq!(tabs.active, Some(prev_id));
    }

    #[test]
    fn lcp_case_insensitive_returns_prefix_from_first_input() {
        let names = ["Main.rs", "mailbox.rs", "MAKE.rs"];
        let p = longest_common_prefix_ci(&names);
        // LCP in case-insensitive terms is 2 chars. Returns the slice
        // from `names[0]`, preserving its original case.
        assert_eq!(p, "Ma");
    }

    #[test]
    fn enter_on_selected_dir_descends_and_keeps_overlay_open() {
        let mut ff = overlay("/tmp/sr", 7);
        {
            let s = ff.as_mut().unwrap();
            s.completions = vec![entry("src", true)];
            s.selected = Some(0);
        }
        let mut tabs = Tabs::default();
        run_overlay_command(Command::InsertNewline, &mut ff, &mut tabs, &mut led_state_buffer_edits::BufferEdits::default());
        // Descent keeps the overlay open, no tab opened.
        let s = ff.as_ref().expect("stays open");
        assert_eq!(s.input, "/tmp/src/");
        assert_eq!(s.base_input, "/tmp/src/");
        assert!(s.selected.is_none());
        assert!(tabs.open.is_empty());
        // Descent queues a re-request.
        assert_eq!(s.pending_find_file_list.len(), 1);
    }

    #[test]
    fn enter_on_selected_file_opens_and_deactivates() {
        let mut ff = overlay("/tmp/mai", 8);
        {
            let s = ff.as_mut().unwrap();
            s.completions = vec![entry("main.rs", false)];
            s.selected = Some(0);
        }
        let mut tabs = Tabs::default();
        run_overlay_command(Command::InsertNewline, &mut ff, &mut tabs, &mut led_state_buffer_edits::BufferEdits::default());
        assert!(ff.is_none());
        assert_eq!(tabs.open.len(), 1);
        assert!(!tabs.open[0].preview);
    }

    #[test]
    fn enter_on_exact_match_dir_descends() {
        let mut ff = overlay("/tmp/src", 8);
        {
            let s = ff.as_mut().unwrap();
            s.completions = vec![entry("src", true)];
        }
        let mut tabs = Tabs::default();
        run_overlay_command(Command::InsertNewline, &mut ff, &mut tabs, &mut led_state_buffer_edits::BufferEdits::default());
        let s = ff.as_ref().expect("stays open");
        assert_eq!(s.input, "/tmp/src/");
    }

    #[test]
    fn save_as_enter_queues_pending_save_as_and_deactivates() {
        use led_state_tabs::{Tab, TabId};
        let mut tabs = Tabs::default();
        let active_id = TabId(1);
        let active_path = canon("/tmp/orig.txt");
        tabs.open.push_back(Tab {
            id: active_id,
            path: active_path.clone(),
            ..Default::default()
        });
        tabs.active = Some(active_id);

        let mut edits = led_state_buffer_edits::BufferEdits::default();
        edits.buffers.insert(
            active_path.clone(),
            led_state_buffer_edits::EditedBuffer::fresh(
                std::sync::Arc::new(ropey::Rope::from_str("payload\n")),
            ),
        );

        let mut ff = Some(FindFileState::save_as("/tmp/copy.txt".to_string()));
        ff.as_mut().unwrap().pending_find_file_list.clear();
        run_overlay_command(Command::InsertNewline, &mut ff, &mut tabs, &mut edits);

        // Overlay closed.
        assert!(ff.is_none());
        // pending_save_as carries the from→to mapping for the next query.
        assert_eq!(
            edits.pending_save_as.get(&active_path),
            Some(&canon("/tmp/copy.txt")),
        );
        // Active tab stays at the original path — SaveAs doesn't rebind.
        assert_eq!(tabs.active, Some(active_id));
        assert_eq!(tabs.open[0].path, active_path);
    }

    #[test]
    fn save_as_enter_on_trailing_slash_is_noop() {
        let mut ff = Some(FindFileState::save_as("/tmp/".to_string()));
        let mut tabs = Tabs::default();
        let mut edits = led_state_buffer_edits::BufferEdits::default();
        run_overlay_command(Command::InsertNewline, &mut ff, &mut tabs, &mut edits);
        assert!(ff.is_some());
        assert!(edits.pending_save_as.is_empty());
    }

    #[test]
    fn enter_focuses_existing_tab_when_path_matches() {
        let mut tabs = Tabs::default();
        let id = led_state_tabs::TabId(42);
        tabs.open.push_back(led_state_tabs::Tab {
            id,
            path: canon("/tmp/existing.txt"),
            preview: true,
            ..Default::default()
        });
        let mut ff = overlay("/tmp/existing.txt", 17);
        run_overlay_command(Command::InsertNewline, &mut ff, &mut tabs, &mut led_state_buffer_edits::BufferEdits::default());
        // Same tab, now active + promoted.
        assert_eq!(tabs.open.len(), 1);
        assert_eq!(tabs.active, Some(id));
        assert!(!tabs.open[0].preview);
    }
}
