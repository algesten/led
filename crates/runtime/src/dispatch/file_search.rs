//! Project-wide file-search overlay dispatch (M14).
//!
//! Current scope: activation / deactivation, typing into the query +
//! replace inputs, the three toggles (`Alt+1/2/3`), and per-edit
//! queueing of ripgrep requests via `FileSearchState::queue_search`.
//! Actual driver + results rendering + navigation + replace commit
//! land in later stages.

use led_state_browser::{BrowserUi, Focus};
use led_state_file_search::{FileSearchSelection, FileSearchState};

use crate::keymap::Command;

use super::DispatchOutcome;

pub(super) fn activate(
    file_search: &mut Option<FileSearchState>,
    browser: &mut BrowserUi,
) {
    if file_search.is_some() {
        // Already open — Ctrl+F a second time is a no-op for now.
        // Stage 6 may repurpose it (e.g. refresh results).
        return;
    }
    *file_search = Some(FileSearchState::default());
    // Overlay lives in the side panel slot; focus moves there so
    // keystrokes route through the overlay.
    browser.visible = true;
    browser.focus = Focus::Side;
}

pub(super) fn deactivate(
    file_search: &mut Option<FileSearchState>,
    browser: &mut BrowserUi,
) {
    *file_search = None;
    // Return focus to the main editor pane.
    browser.focus = Focus::Main;
}

/// Route a `Command` through the overlay when active.
///
/// Returns `Some(Continue)` when fully consumed, `None` to fall
/// through to the normal dispatch path (`Quit` is the only
/// current pass-through).
pub(super) fn run_overlay_command(
    cmd: Command,
    file_search: &mut Option<FileSearchState>,
    browser: &mut BrowserUi,
) -> Option<DispatchOutcome> {
    let state = file_search.as_mut()?;
    match cmd {
        Command::InsertChar(c) => {
            input_for_selection(state).insert_char(c);
            state.queue_search();
        }
        Command::DeleteBack => {
            input_for_selection(state).delete_back();
            state.queue_search();
        }
        Command::DeleteForward => {
            input_for_selection(state).delete_forward();
            state.queue_search();
        }
        Command::CursorLeft => {
            input_for_selection(state).move_left();
        }
        Command::CursorRight => {
            input_for_selection(state).move_right();
        }
        Command::CursorLineStart => {
            input_for_selection(state).to_line_start();
        }
        Command::CursorLineEnd => {
            input_for_selection(state).to_line_end();
        }
        Command::KillLine => {
            input_for_selection(state).kill_to_end();
            state.queue_search();
        }
        Command::ToggleSearchCase => {
            state.case_sensitive = !state.case_sensitive;
            state.queue_search();
        }
        Command::ToggleSearchRegex => {
            state.use_regex = !state.use_regex;
            state.queue_search();
        }
        Command::ToggleSearchReplace => {
            state.replace_mode = !state.replace_mode;
            // No re-search — replace mode only toggles the extra
            // input row; existing results stay.
        }
        Command::Abort | Command::CloseFileSearch => {
            deactivate(file_search, browser);
        }
        // Enter / result navigation / ReplaceAll land in later stages.
        Command::InsertNewline | Command::ReplaceAll => {}
        // Fall-through list: the overlay is focused in the sidebar,
        // so normal editor commands stay absorbed for now.
        Command::Quit => return None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activate_opens_overlay_and_moves_focus_to_side() {
        let mut fs = None;
        let mut browser = BrowserUi::default();
        assert_eq!(browser.focus, Focus::Main);
        activate(&mut fs, &mut browser);
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
        deactivate(&mut fs, &mut browser);
        assert!(fs.is_none());
        assert_eq!(browser.focus, Focus::Main);
    }

    #[test]
    fn activate_twice_is_noop_on_the_second_call() {
        let mut fs = None;
        let mut browser = BrowserUi::default();
        activate(&mut fs, &mut browser);
        let first = fs.clone();
        activate(&mut fs, &mut browser);
        assert_eq!(fs, first);
    }
}
