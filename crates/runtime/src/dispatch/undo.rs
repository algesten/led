//! Undo / redo (M8).
//!
//! Each reverses / reapplies the most recent [`EditGroup`] in the
//! buffer's history. Cursor is restored to the captured bookend
//! (cursor_before for undo, cursor_after for redo).

use led_state_buffer_edits::{BufferEdits, EditOp};
use led_state_tabs::Tabs;

use super::shared::{bump, with_active};

pub(super) fn undo_active(tabs: &mut Tabs, edits: &mut BufferEdits) {
    with_active(tabs, edits, |tab, eb| {
        let Some(group) = eb.history.take_undo() else {
            return;
        };
        // Apply ops in reverse order, as their inverses.
        let mut rope = (*eb.rope).clone();
        for op in group.ops.iter().rev() {
            match op {
                EditOp::Insert { at, text } => {
                    let len = text.chars().count();
                    rope.remove(*at..*at + len);
                }
                EditOp::Delete { at, text } => {
                    rope.insert(*at, text);
                }
            }
        }
        bump(eb, rope);
        tab.cursor = group.cursor_before;
        tab.cursor.preferred_col = tab.cursor.col;
        eb.history.push_future(group);
    });
}

pub(super) fn redo_active(tabs: &mut Tabs, edits: &mut BufferEdits) {
    with_active(tabs, edits, |tab, eb| {
        let Some(group) = eb.history.take_redo() else {
            return;
        };
        let mut rope = (*eb.rope).clone();
        for op in &group.ops {
            match op {
                EditOp::Insert { at, text } => {
                    rope.insert(*at, text);
                }
                EditOp::Delete { at, text } => {
                    let len = text.chars().count();
                    rope.remove(*at..*at + len);
                }
            }
        }
        bump(eb, rope);
        tab.cursor = group.cursor_after;
        tab.cursor.preferred_col = tab.cursor.col;
        eb.history.push_past(group);
    });
}

#[cfg(test)]
mod tests {
    

    
    
    use led_driver_terminal_core::{Dims, KeyCode, KeyModifiers};
    use led_state_alerts::AlertState;
    use led_state_jumps::JumpListState;
    
    use led_state_kill_ring::KillRing;
    use led_state_tabs::Cursor;
    

    
    use super::super::testutil::*;
    use super::super::{dispatch_key, ChordState};
    use crate::keymap::{default_keymap, Command};

    #[test]
    fn undo_removes_coalesced_word_inserts_in_one_shot() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("", Dims { cols: 20, rows: 5 });

        type_chars("hello", &mut tabs, &mut edits, &store, &term);
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "hello");

        // Ctrl-/ → one group, five chars gone.
        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('/')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "");
    }

    #[test]
    fn undo_with_space_boundary_pops_only_last_word() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("", Dims { cols: 20, rows: 5 });

        type_chars("hello ", &mut tabs, &mut edits, &store, &term);
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "hello ");

        // Space broke coalescing → two groups: "hello" then " ".
        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('/')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "hello");
    }

    #[test]
    fn redo_applies_the_undone_group() {
        // Plain undo is bound; redo isn't — use a custom keymap.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("", Dims { cols: 20, rows: 5 });

        type_chars("hi", &mut tabs, &mut edits, &store, &term);
        let mut km = default_keymap();
        km.bind("ctrl+y", Command::Redo); // override Yank for test
        let mut chord = ChordState::default();
        let mut kr = KillRing::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();

        // Undo: ""
        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('/')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &mut alerts,
            &mut jumps,
            &store,
            &term,
            &km,
            &mut chord,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "");

        // Redo: "hi"
        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('y')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &mut alerts,
            &mut jumps,
            &store,
            &term,
            &km,
            &mut chord,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "hi");
    }

    #[test]
    fn undo_restores_killed_region() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abcdefgh", Dims { cols: 20, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 2,
            preferred_col: 2,
        };
        tabs.open[0].mark = Some(Cursor {
            line: 0,
            col: 6,
            preferred_col: 6,
        });
        let mut kr = KillRing::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('w')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abgh");

        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('/')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abcdefgh");
    }

    #[test]
    fn edit_after_undo_drops_future() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("", Dims { cols: 20, rows: 5 });
        type_chars("hi", &mut tabs, &mut edits, &store, &term);
        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('/')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "");
        // Redo is bound in this test via a custom map; before that,
        // a new edit should drop the future branch.
        type_chars("x", &mut tabs, &mut edits, &store, &term);
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "x");

        let mut km = default_keymap();
        km.bind("ctrl+y", Command::Redo);
        let mut chord = ChordState::default();
        let mut kr = KillRing::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();
        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('y')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &mut alerts,
            &mut jumps,
            &store,
            &term,
            &km,
            &mut chord,
        );
        // Still "x" — nothing to redo because the new edit dropped
        // the future branch.
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "x");
    }

    #[test]
    fn undo_restores_cursor_before() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("", Dims { cols: 20, rows: 5 });
        type_chars("hi", &mut tabs, &mut edits, &store, &term);
        // Cursor is at (0, 2). Move it elsewhere to verify that undo
        // restores to cursor_before.
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 0,
            preferred_col: 0,
        };
        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('/')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "");
        // Undo restored cursor_before, which was (0, 0) for the
        // first char of the coalesced "hi" group.
        assert_eq!(tabs.open[0].cursor.col, 0);
    }
}
