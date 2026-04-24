//! LSP rename overlay dispatch (M18 stage 3).
//!
//! Activating the overlay seeds an editable [`TextInput`] with the
//! identifier under the cursor, parks the cursor anchor, and
//! stashes the whole thing on [`LspExtrasState::rename`].
//! While the overlay is active, every keystroke routes through
//! [`run_overlay_command`]:
//!
//! - `InsertChar` / `DeleteBack` / `DeleteForward` / arrows edit
//!   the input in place.
//! - `InsertNewline` commits — queues a `RequestRename` with the
//!   current typed text as `new_name` and dismisses the overlay.
//!   The edits flow back via `LspEvent::Edits { origin: Rename }`
//!   handled in the ingest loop.
//! - `Abort` dismisses without committing.
//! - Every other command is absorbed so the key doesn't leak to
//!   the buffer beneath (matches legacy's modal-dispatch shape).
//!
//! Activation happens in the same file so the seed/anchor logic
//! has access to the buffer rope for word-under-cursor detection.

use std::sync::Arc;

use led_state_buffer_edits::BufferEdits;
use led_state_lsp::{LspExtrasState, RenameState};
use led_state_tabs::Tabs;

use super::shared::{is_word_char, line_char_len};
use super::DispatchOutcome;
use crate::keymap::Command;

/// Open the rename overlay for the active tab's cursor. No-op
/// when no tab is active, the buffer is unloaded, another
/// overlay is already live, or the cursor isn't sitting on an
/// identifier.
pub(super) fn activate(
    lsp_extras: &mut LspExtrasState,
    tabs: &Tabs,
    edits: &BufferEdits,
) {
    if lsp_extras.rename.is_some() {
        return;
    }
    let Some(id) = tabs.active else {
        return;
    };
    let Some(tab) = tabs.open.iter().find(|t| t.id == id) else {
        return;
    };
    if tab.preview {
        return;
    }
    let Some(eb) = edits.buffers.get(&tab.path) else {
        return;
    };
    let rope = &eb.rope;
    let line = tab.cursor.line;
    let col = tab.cursor.col;
    let (word_start, word_text) = match word_at(rope, line, col) {
        Some(pair) => pair,
        None => return,
    };
    lsp_extras.rename = Some(RenameState::open(
        tab.path.clone(),
        line as u32,
        word_start as u32,
        word_text,
    ));
}

/// Route a command through the rename overlay when active.
/// Returns `Some(Continue)` when the command was consumed and
/// `None` when no overlay is open. `Quit` passes through (so
/// `ctrl+x ctrl+c` still exits the editor).
pub(super) fn run_overlay_command(
    cmd: Command,
    lsp_extras: &mut LspExtrasState,
) -> Option<DispatchOutcome> {
    lsp_extras.rename.as_ref()?;
    if matches!(cmd, Command::Quit) {
        return None;
    }
    match cmd {
        Command::InsertChar(c) => {
            if let Some(state) = lsp_extras.rename.as_mut() {
                state.input.insert_char(c);
            }
        }
        Command::DeleteBack => {
            if let Some(state) = lsp_extras.rename.as_mut() {
                state.input.delete_back();
            }
        }
        Command::DeleteForward => {
            if let Some(state) = lsp_extras.rename.as_mut() {
                state.input.delete_forward();
            }
        }
        Command::CursorLeft => {
            if let Some(state) = lsp_extras.rename.as_mut() {
                state.input.move_left();
            }
        }
        Command::CursorRight => {
            if let Some(state) = lsp_extras.rename.as_mut() {
                state.input.move_right();
            }
        }
        Command::CursorLineStart => {
            if let Some(state) = lsp_extras.rename.as_mut() {
                state.input.to_line_start();
            }
        }
        Command::CursorLineEnd => {
            if let Some(state) = lsp_extras.rename.as_mut() {
                state.input.to_line_end();
            }
        }
        Command::KillLine => {
            if let Some(state) = lsp_extras.rename.as_mut() {
                state.input.kill_to_end();
            }
        }
        Command::InsertNewline => commit(lsp_extras),
        Command::Abort => lsp_extras.dismiss_rename(),
        // Every other command is absorbed while the overlay has
        // focus — matches legacy's "rename overlay swallows all"
        // discipline (legacy rename routes every Action through
        // `handle_rename_action`).
        _ => {}
    }
    Some(DispatchOutcome::Continue)
}

/// Enter-commit: queue the rename RPC with the current input
/// text, then close the overlay. An empty input or one equal
/// to the seed word is a silent no-op — no point churning the
/// server for "rename foo to foo".
fn commit(lsp_extras: &mut LspExtrasState) {
    let Some(state) = lsp_extras.rename.as_ref() else {
        return;
    };
    let trimmed = state.input.text.trim();
    if trimmed.is_empty() || trimmed == state.seed_word.as_ref() {
        lsp_extras.dismiss_rename();
        return;
    }
    let path = state.anchor_path.clone();
    let line = state.anchor_line;
    let col = state.anchor_col;
    let new_name: Arc<str> = Arc::<str>::from(trimmed);
    lsp_extras.queue_rename(path, line, col, new_name);
    lsp_extras.dismiss_rename();
}

/// Find the identifier sitting at `(line, col)` on the rope.
/// Returns `(start_col, text)` where `start_col` is the char
/// offset of the first ident char on `line`. `None` when the
/// cursor isn't on an identifier.
///
/// An identifier is a maximal run of `is_word_char` chars. If
/// the cursor is in whitespace between two idents, the
/// preceding ident (if any) is picked — matches legacy's
/// `word_under_cursor` in `led/src/model/buffer.rs`.
fn word_at(
    rope: &ropey::Rope,
    line: usize,
    col: usize,
) -> Option<(usize, Arc<str>)> {
    if line >= rope.len_lines() {
        return None;
    }
    let line_slice = rope.line(line);
    let len = line_char_len(rope, line);
    if len == 0 {
        return None;
    }
    // Pick the ident char the cursor is "on": the char at col,
    // else the char immediately before col (for end-of-word
    // positioning).
    let pivot = if col < len && is_word_char(line_slice.char(col)) {
        col
    } else if col > 0 && is_word_char(line_slice.char(col - 1)) {
        col - 1
    } else {
        return None;
    };
    let mut start = pivot;
    while start > 0 && is_word_char(line_slice.char(start - 1)) {
        start -= 1;
    }
    let mut end = pivot + 1;
    while end < len && is_word_char(line_slice.char(end)) {
        end += 1;
    }
    let text: String = line_slice.slice(start..end).to_string();
    Some((start, Arc::<str>::from(text)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;
    use led_state_buffer_edits::EditedBuffer;
    use led_state_tabs::{Cursor, Tab, TabId};
    use ropey::Rope;
    use std::sync::Arc as StdArc;

    fn canon(s: &str) -> led_core::CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn seed(content: &str, cursor_col: usize) -> (Tabs, BufferEdits) {
        let mut tabs = Tabs::default();
        let id = TabId(1);
        tabs.open.push_back(Tab {
            id,
            path: canon("main.rs"),
            cursor: Cursor {
                line: 0,
                col: cursor_col,
                preferred_col: cursor_col,
            },
            ..Default::default()
        });
        tabs.active = Some(id);
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("main.rs"),
            EditedBuffer::fresh(StdArc::new(Rope::from_str(content))),
        );
        (tabs, edits)
    }

    #[test]
    fn activate_seeds_word_under_cursor() {
        let (tabs, edits) = seed("let foo = 1;", 5); // on 'o' of "foo"
        let mut lsp = LspExtrasState::default();
        activate(&mut lsp, &tabs, &edits);
        let state = lsp.rename.expect("overlay opened");
        assert_eq!(state.seed_word.as_ref(), "foo");
        assert_eq!(state.input.text, "foo");
        assert_eq!(state.anchor_col, 4);
    }

    #[test]
    fn activate_picks_word_before_cursor_when_at_end() {
        let (tabs, edits) = seed("hello world", 5); // just past "hello"
        let mut lsp = LspExtrasState::default();
        activate(&mut lsp, &tabs, &edits);
        let state = lsp.rename.expect("overlay opened");
        assert_eq!(state.seed_word.as_ref(), "hello");
        assert_eq!(state.anchor_col, 0);
    }

    #[test]
    fn activate_noop_on_whitespace() {
        let (tabs, edits) = seed("   ", 1);
        let mut lsp = LspExtrasState::default();
        activate(&mut lsp, &tabs, &edits);
        assert!(lsp.rename.is_none());
    }

    #[test]
    fn activate_noop_when_overlay_already_open() {
        let (tabs, edits) = seed("foo bar", 1);
        let mut lsp = LspExtrasState::default();
        lsp.rename = Some(RenameState::open(
            canon("main.rs"),
            0,
            0,
            Arc::<str>::from("existing"),
        ));
        activate(&mut lsp, &tabs, &edits);
        // Unchanged.
        assert_eq!(lsp.rename.as_ref().unwrap().seed_word.as_ref(), "existing");
    }

    #[test]
    fn insert_char_appends_to_input() {
        let mut lsp = LspExtrasState::default();
        lsp.rename = Some(RenameState::open(
            canon("a.rs"),
            0,
            0,
            Arc::<str>::from("foo"),
        ));
        let out = run_overlay_command(Command::InsertChar('x'), &mut lsp);
        assert_eq!(out, Some(DispatchOutcome::Continue));
        assert_eq!(lsp.rename.as_ref().unwrap().input.text, "foox");
    }

    #[test]
    fn delete_back_trims_input() {
        let mut lsp = LspExtrasState::default();
        lsp.rename = Some(RenameState::open(
            canon("a.rs"),
            0,
            0,
            Arc::<str>::from("foo"),
        ));
        run_overlay_command(Command::DeleteBack, &mut lsp);
        assert_eq!(lsp.rename.as_ref().unwrap().input.text, "fo");
    }

    #[test]
    fn enter_commits_queues_rename_and_dismisses() {
        let mut lsp = LspExtrasState::default();
        lsp.rename = Some(RenameState::open(
            canon("a.rs"),
            2,
            4,
            Arc::<str>::from("foo"),
        ));
        run_overlay_command(Command::InsertChar('s'), &mut lsp);
        run_overlay_command(Command::InsertNewline, &mut lsp);
        assert!(lsp.rename.is_none());
        assert_eq!(lsp.pending_rename.len(), 1);
        let req = &lsp.pending_rename[0];
        assert_eq!(req.new_name.as_ref(), "foos");
        assert_eq!(req.line, 2);
        assert_eq!(req.col, 4);
        assert_eq!(lsp.latest_rename_seq, Some(req.seq));
    }

    #[test]
    fn enter_with_unchanged_input_dismisses_without_queuing() {
        let mut lsp = LspExtrasState::default();
        lsp.rename = Some(RenameState::open(
            canon("a.rs"),
            0,
            0,
            Arc::<str>::from("foo"),
        ));
        run_overlay_command(Command::InsertNewline, &mut lsp);
        assert!(lsp.rename.is_none());
        assert!(lsp.pending_rename.is_empty());
    }

    #[test]
    fn abort_dismisses_without_queuing() {
        let mut lsp = LspExtrasState::default();
        lsp.rename = Some(RenameState::open(
            canon("a.rs"),
            0,
            0,
            Arc::<str>::from("foo"),
        ));
        run_overlay_command(Command::InsertChar('y'), &mut lsp);
        run_overlay_command(Command::Abort, &mut lsp);
        assert!(lsp.rename.is_none());
        assert!(lsp.pending_rename.is_empty());
    }

    #[test]
    fn quit_passes_through() {
        let mut lsp = LspExtrasState::default();
        lsp.rename = Some(RenameState::open(
            canon("a.rs"),
            0,
            0,
            Arc::<str>::from("foo"),
        ));
        let out = run_overlay_command(Command::Quit, &mut lsp);
        assert_eq!(out, None);
    }
}
