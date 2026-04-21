//! Tab cycling + kill-buffer (M1, M6, M9). Tab cycling also records
//! a jump-list entry for the outgoing tab (M10) so the user can
//! `alt+b` back to wherever they came from.

use led_state_alerts::AlertState;
use led_state_buffer_edits::BufferEdits;
use led_state_jumps::{JumpListState, JumpPosition};
use led_state_tabs::{Tab, TabId, Tabs};

pub(super) fn cycle_active(tabs: &mut Tabs, jumps: &mut JumpListState, delta: isize) {
    if tabs.open.is_empty() {
        return;
    }
    let n = tabs.open.len() as isize;
    let cur_idx = tabs
        .active
        .and_then(|id| tabs.open.iter().position(|t: &Tab| t.id == id))
        .unwrap_or(0) as isize;
    let next_idx = (cur_idx + delta).rem_euclid(n) as usize;

    // Record the outgoing tab's cursor so Alt-b returns here. Skip
    // the no-op case where the tab doesn't actually change.
    if let Some(prev_id) = tabs.active
        && let Some(prev) = tabs.open.iter().find(|t| t.id == prev_id)
        && prev.id != tabs.open[next_idx].id
    {
        jumps.record(JumpPosition {
            path: prev.path.clone(),
            line: prev.cursor.line,
            col: prev.cursor.col,
        });
    }

    tabs.active = Some(tabs.open[next_idx].id);
}

/// Close the active tab. If the buffer is dirty, raise a confirm-kill
/// prompt (user must press `y`/`Y` to proceed); otherwise force-kill
/// immediately.
pub(super) fn kill_active(tabs: &mut Tabs, edits: &mut BufferEdits, alerts: &mut AlertState) {
    let Some(id) = tabs.active else {
        return;
    };
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    let dirty = edits
        .buffers
        .get(&tabs.open[idx].path)
        .map(|eb| eb.dirty())
        .unwrap_or(false);
    if dirty {
        alerts.confirm_kill = Some(id);
        return;
    }
    force_kill(tabs, edits, id);
}

/// Unconditionally remove the tab with the given id. Drops its
/// pending-save entry and buffer edits. After removal, activates the
/// neighbour tab or `None` if this was the last.
pub(super) fn force_kill(tabs: &mut Tabs, edits: &mut BufferEdits, id: TabId) {
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    let path = tabs.open[idx].path.clone();
    tabs.open.remove(idx);
    edits.buffers.remove(&path);
    edits.pending_saves.remove(&path);
    if tabs.open.is_empty() {
        tabs.active = None;
    } else if tabs.active == Some(id) {
        let next = idx.min(tabs.open.len() - 1);
        tabs.active = Some(tabs.open[next].id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    
    use led_driver_buffers_core::BufferStore;
    use led_driver_terminal_core::{Dims, KeyCode, KeyModifiers, Terminal};
    use led_state_alerts::AlertState;
    use led_state_clipboard::ClipboardState;
    use led_state_jumps::JumpListState;
    use led_state_browser::{BrowserUi, FsTree};
    use led_state_buffer_edits::{BufferEdits, EditedBuffer};
    use led_state_kill_ring::KillRing;
    use led_state_tabs::{Cursor, TabId, Tabs};
    use ropey::Rope;

    
    use super::super::testutil::*;
    use super::super::{dispatch_key, ChordState};
    use crate::keymap::default_keymap;

    // ── Tab switching + quit (M1 behaviour, unchanged) ──────────────────

    #[test]
    fn tab_cycles_active_forward() {
        let mut tabs = tabs_with(&[("a", 1), ("b", 2), ("c", 3)], Some(1));
        noop_dispatch(key(KeyModifiers::NONE, KeyCode::Tab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(2)));
        noop_dispatch(key(KeyModifiers::NONE, KeyCode::Tab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(3)));
        noop_dispatch(key(KeyModifiers::NONE, KeyCode::Tab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(1)));
    }

    #[test]
    fn shift_tab_cycles_backward() {
        // Terminals may encode Shift+Tab either as `{Tab, SHIFT}`
        // (modifier) or `{BackTab, NONE}` (special key code). The
        // default keymap binds both forms.
        let mut tabs = tabs_with(&[("a", 1), ("b", 2), ("c", 3)], Some(1));
        noop_dispatch(key(KeyModifiers::SHIFT, KeyCode::Tab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(3)));
        noop_dispatch(key(KeyModifiers::NONE, KeyCode::BackTab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(2)));
    }

    #[test]
    fn tab_on_empty_does_nothing() {
        let mut tabs = Tabs::default();
        noop_dispatch(key(KeyModifiers::NONE, KeyCode::Tab), &mut tabs);
        assert_eq!(tabs.active, None);
    }

    #[test]
    fn kill_buffer_closes_clean_active_tab() {
        let mut tabs = tabs_with(&[("a", 1), ("b", 2)], Some(1));
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("A"))),
        );
        edits.buffers.insert(
            canon("b"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("B"))),
        );
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));

        dispatch_chord_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            key(KeyModifiers::NONE, KeyCode::Char('k')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(tabs.open.len(), 1);
        assert_eq!(tabs.open[0].id, TabId(2));
        assert!(!edits.buffers.contains_key(&canon("a")));
    }

    // ── M9: confirm-kill on dirty ─────────────────────────────────────

    fn dirty_tabs_with_confirm_scenario() -> (Tabs, BufferEdits, BufferStore, Terminal) {
        let tabs = tabs_with(&[("a", 1), ("b", 2)], Some(1));
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("A")),
                version: 1,
                saved_version: 0,
                history: Default::default(),
            },
        );
        edits.buffers.insert(
            canon("b"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("B"))),
        );
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));
        (tabs, edits, store, term)
    }

    #[test]
    fn kill_buffer_on_dirty_raises_confirm_prompt() {
        let (mut tabs, mut edits, store, term) = dirty_tabs_with_confirm_scenario();
        let mut kill_ring = KillRing::default();
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();
        let mut chord = ChordState::default();
        let keymap = default_keymap();

        // Ctrl-x k on dirty active tab → prompt set, tab still open.
        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
            &keymap,
            &mut chord,
        );
        dispatch_key(
            key(KeyModifiers::NONE, KeyCode::Char('k')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
            &keymap,
            &mut chord,
        );
        assert_eq!(alerts.confirm_kill, Some(TabId(1)));
        assert_eq!(tabs.open.len(), 2);
    }

    #[test]
    fn confirm_kill_y_force_kills_and_clears_prompt() {
        let (mut tabs, mut edits, store, term) = dirty_tabs_with_confirm_scenario();
        let mut kill_ring = KillRing::default();
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState {
            confirm_kill: Some(TabId(1)),
            ..Default::default()
        };
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();
        let mut chord = ChordState::default();
        let keymap = default_keymap();

        dispatch_key(
            key(KeyModifiers::NONE, KeyCode::Char('y')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
            &keymap,
            &mut chord,
        );
        assert!(alerts.confirm_kill.is_none());
        assert_eq!(tabs.open.len(), 1);
        assert_eq!(tabs.open[0].id, TabId(2));
        // 'y' must NOT have been inserted into the (now-gone) buffer —
        // force-kill returns early.
        assert!(!edits.buffers.contains_key(&canon("a")));
    }

    #[test]
    fn confirm_kill_capital_y_also_confirms() {
        let (mut tabs, mut edits, store, term) = dirty_tabs_with_confirm_scenario();
        let mut kill_ring = KillRing::default();
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState {
            confirm_kill: Some(TabId(1)),
            ..Default::default()
        };
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();
        let mut chord = ChordState::default();
        let keymap = default_keymap();

        dispatch_key(
            key(KeyModifiers::NONE, KeyCode::Char('Y')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
            &keymap,
            &mut chord,
        );
        assert!(alerts.confirm_kill.is_none());
        assert_eq!(tabs.open.len(), 1);
    }

    #[test]
    fn confirm_kill_n_dismisses_and_inserts() {
        let (mut tabs, mut edits, store, term) = dirty_tabs_with_confirm_scenario();
        let mut kill_ring = KillRing::default();
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState {
            confirm_kill: Some(TabId(1)),
            ..Default::default()
        };
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();
        let mut chord = ChordState::default();
        let keymap = default_keymap();

        dispatch_key(
            key(KeyModifiers::NONE, KeyCode::Char('n')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
            &keymap,
            &mut chord,
        );
        // Prompt dismissed.
        assert!(alerts.confirm_kill.is_none());
        // Tab stays open.
        assert_eq!(tabs.open.len(), 2);
        // 'n' inserted into active buffer.
        assert_eq!(rope_of(&edits, "a").to_string(), "nA");
    }

    #[test]
    fn confirm_kill_esc_dismisses_and_clears_mark() {
        let (mut tabs, mut edits, store, term) = dirty_tabs_with_confirm_scenario();
        // Set a mark so we can verify Esc's Abort runs.
        tabs.open[0].mark = Some(Cursor {
            line: 0,
            col: 0,
            preferred_col: 0,
        });
        let mut kill_ring = KillRing::default();
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState {
            confirm_kill: Some(TabId(1)),
            ..Default::default()
        };
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();
        let mut chord = ChordState::default();
        let keymap = default_keymap();

        dispatch_key(
            key(KeyModifiers::NONE, KeyCode::Esc),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
            &keymap,
            &mut chord,
        );
        assert!(alerts.confirm_kill.is_none());
        assert_eq!(tabs.open.len(), 2);
        // Esc's Abort command still ran → mark cleared.
        assert!(tabs.open[0].mark.is_none());
    }

    #[test]
    fn kill_buffer_on_clean_still_kills_without_prompt() {
        // M9 regression guard: the clean path must not accidentally
        // route through confirm_kill.
        let mut tabs = tabs_with(&[("a", 1), ("b", 2)], Some(1));
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("A"))),
        );
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));
        let mut kill_ring = KillRing::default();
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();
        let mut chord = ChordState::default();
        let keymap = default_keymap();

        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
            &keymap,
            &mut chord,
        );
        dispatch_key(
            key(KeyModifiers::NONE, KeyCode::Char('k')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
            &keymap,
            &mut chord,
        );
        assert!(alerts.confirm_kill.is_none());
        assert_eq!(tabs.open.len(), 1);
    }
}
