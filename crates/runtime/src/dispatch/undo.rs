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
