//! Project-wide file-search overlay dispatch (M14).
//!
//! Stage 1 scope: activation + deactivation only. Typing, driver-
//! fired searches, toggles (`Alt+1/2/3`), result navigation, preview
//! tabs, and the replace flow all land in later stages.

use led_state_browser::{BrowserUi, Focus};
use led_state_file_search::FileSearchState;

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
