//! Find-file / save-as overlay dispatch.
//!
//! Stage 1: activation + deactivation only. Future stages add input
//! editing, tab completion, arrow navigation + preview, and commit
//! paths.
//!
//! Activation rules (matching legacy `compute_activate` /
//! `compute_activate_save_as`):
//!
//! - **Open**: initial input is the active buffer's parent directory
//!   with a trailing `/`, home-abbreviated to `~/...` when the parent
//!   lives under `$HOME`. Falls back to the workspace root (or the
//!   filesystem root `/` if no workspace) when there's no active
//!   buffer.
//! - **SaveAs**: initial input is the active buffer's full path
//!   (home-abbreviated). Caller can tweak the filename in place and
//!   hit Enter. Falls back to the workspace root + `/` when there's
//!   no active buffer.
//!
//! Future stages (M12 phases 2+):
//! - Driver completion requests (`FsFindFile` trace).
//! - `handle_action` for InsertChar / DeleteBack / arrows / Tab / Enter.
//! - Preview-tab side effects during arrow navigation.
//! - Open-mode paths A/B/C; SaveAs commit.

use led_state_browser::FsTree;
use led_state_find_file::{FindFileState, abbreviate_home};
use led_state_tabs::Tabs;

use crate::keymap::Command;

use super::DispatchOutcome;

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
    *find_file = Some(FindFileState::open(input));
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
    *find_file = Some(FindFileState::save_as(input));
}

/// Close the overlay. Idempotent. Future stages will also close
/// the preview tab (if any) and restore the previously-active tab.
pub(super) fn deactivate(find_file: &mut Option<FindFileState>) {
    *find_file = None;
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
) -> Option<DispatchOutcome> {
    let state = find_file.as_mut()?;
    match cmd {
        Command::InsertChar(c) => insert_char(state, c),
        Command::DeleteBack => delete_back(state),
        Command::DeleteForward => delete_forward(state),
        Command::CursorLeft => move_left(state),
        Command::CursorRight => move_right(state),
        Command::CursorLineStart => state.cursor = 0,
        Command::CursorLineEnd => state.cursor = state.input.len(),
        Command::KillLine => kill_line(state),
        Command::Abort => deactivate(find_file),
        // Not-yet-implemented overlay commands. Absorbed so the key
        // doesn't leak to the buffer below.
        Command::CursorUp
        | Command::CursorDown
        | Command::CursorPageUp
        | Command::CursorPageDown
        | Command::CursorFileStart
        | Command::CursorFileEnd
        | Command::CursorWordLeft
        | Command::CursorWordRight
        | Command::FindFileTabComplete
        | Command::InsertNewline => {}
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
    s.pending_find_file_list = true;
}

fn delete_back(s: &mut FindFileState) {
    if s.cursor == 0 {
        return;
    }
    let prev = prev_char_boundary(&s.input, s.cursor);
    s.input.replace_range(prev..s.cursor, "");
    s.cursor = prev;
    s.reset_selection();
    s.pending_find_file_list = true;
}

fn delete_forward(s: &mut FindFileState) {
    if s.cursor >= s.input.len() {
        return;
    }
    let next = next_char_boundary(&s.input, s.cursor);
    s.input.replace_range(s.cursor..next, "");
    s.reset_selection();
    s.pending_find_file_list = true;
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
    s.pending_find_file_list = true;
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
        deactivate(&mut ff);
        assert!(ff.is_none());
    }

    // ── Input editing ──────────────────────────────────────────────

    fn overlay(input: &str, cursor: usize) -> Option<FindFileState> {
        let mut s = FindFileState::open(input.to_string());
        s.cursor = cursor;
        // Activation pre-fires pending_find_file_list; tests want to
        // observe the bit re-arming after each edit.
        s.pending_find_file_list = false;
        Some(s)
    }

    #[test]
    fn insert_char_extends_input_and_rearms_pending() {
        let mut ff = overlay("/tmp/", 5);
        run_overlay_command(Command::InsertChar('a'), &mut ff);
        let s = ff.as_ref().unwrap();
        assert_eq!(s.input, "/tmp/a");
        assert_eq!(s.cursor, 6);
        assert!(s.pending_find_file_list);
    }

    #[test]
    fn delete_back_at_end_of_input() {
        let mut ff = overlay("/tmp/a", 6);
        run_overlay_command(Command::DeleteBack, &mut ff);
        assert_eq!(ff.as_ref().unwrap().input, "/tmp/");
        assert_eq!(ff.as_ref().unwrap().cursor, 5);
    }

    #[test]
    fn delete_back_at_start_is_noop() {
        let mut ff = overlay("/tmp/", 0);
        run_overlay_command(Command::DeleteBack, &mut ff);
        assert_eq!(ff.as_ref().unwrap().input, "/tmp/");
        assert!(!ff.as_ref().unwrap().pending_find_file_list);
    }

    #[test]
    fn delete_forward_at_middle() {
        let mut ff = overlay("/tmp/ab", 5);
        run_overlay_command(Command::DeleteForward, &mut ff);
        assert_eq!(ff.as_ref().unwrap().input, "/tmp/b");
    }

    #[test]
    fn cursor_left_and_right_walk_char_boundaries() {
        let mut ff = overlay("/ä/", 3); // 'ä' is 2 bytes at byte 1..3
        run_overlay_command(Command::CursorLeft, &mut ff);
        assert_eq!(ff.as_ref().unwrap().cursor, 1);
        run_overlay_command(Command::CursorRight, &mut ff);
        assert_eq!(ff.as_ref().unwrap().cursor, 3);
    }

    #[test]
    fn line_start_and_end() {
        let mut ff = overlay("/tmp/", 2);
        run_overlay_command(Command::CursorLineStart, &mut ff);
        assert_eq!(ff.as_ref().unwrap().cursor, 0);
        run_overlay_command(Command::CursorLineEnd, &mut ff);
        assert_eq!(ff.as_ref().unwrap().cursor, 5);
    }

    #[test]
    fn kill_line_truncates_at_cursor() {
        let mut ff = overlay("/tmp/abc", 5);
        run_overlay_command(Command::KillLine, &mut ff);
        assert_eq!(ff.as_ref().unwrap().input, "/tmp/");
        assert!(ff.as_ref().unwrap().pending_find_file_list);
    }

    #[test]
    fn abort_closes_the_overlay() {
        let mut ff = overlay("/tmp/", 5);
        let outcome = run_overlay_command(Command::Abort, &mut ff);
        assert_eq!(outcome, Some(DispatchOutcome::Continue));
        assert!(ff.is_none());
    }

    #[test]
    fn quit_passes_through() {
        let mut ff = overlay("/tmp/", 5);
        let outcome = run_overlay_command(Command::Quit, &mut ff);
        // None == "fall through to the normal dispatch path", so
        // the outer `run_command` turns the Quit into a real exit.
        assert!(outcome.is_none());
        assert!(ff.is_some());
    }

    #[test]
    fn inactive_overlay_passes_everything_through() {
        let mut ff: Option<FindFileState> = None;
        assert!(run_overlay_command(Command::InsertChar('a'), &mut ff).is_none());
    }
}
