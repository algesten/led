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
    let basename = tabs.open[idx]
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    force_kill(tabs, edits, id);
    if !basename.is_empty() {
        alerts.set_info(
            format!("Killed {basename}"),
            std::time::Instant::now(),
            std::time::Duration::from_secs(2),
        );
    }
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
    use led_state_completions::CompletionsState;
    use led_state_diagnostics::DiagnosticsStates;
    use led_state_file_search::FileSearchState;
    use led_state_find_file::FindFileState;
    use led_state_git::GitState;
    use led_state_isearch::IsearchState;
    use std::sync::Arc;

    
    use led_driver_buffers_core::BufferStore;
    use led_driver_terminal_core::{Dims, KeyCode, KeyModifiers, Terminal};
    use led_state_alerts::AlertState;
    use led_state_clipboard::ClipboardState;
    use led_state_jumps::JumpListState;
    use led_state_browser::{BrowserUi, FsTree};
    use led_state_buffer_edits::{BufferEdits, EditedBuffer};
    use led_state_kill_ring::KillRing;
    use led_state_lsp::LspExtrasState;
    use led_state_tabs::{Cursor, TabId, Tabs};
    use ropey::Rope;

    
    use super::super::testutil::*;
    use super::super::{ChordState, Dispatcher};
    use crate::keymap::default_keymap;

    // ── Tab switching + quit (M1 behaviour, unchanged) ──────────────────

    #[test]
    fn ctrl_right_cycles_active_forward() {
        // Legacy binds `ctrl+right` / `ctrl+left` to next/prev tab.
        // Plain `Tab` is reserved for `insert_tab` (M23).
        let mut tabs = tabs_with(&[("a", 1), ("b", 2), ("c", 3)], Some(1));
        noop_dispatch(key(KeyModifiers::CONTROL, KeyCode::Right), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(2)));
        noop_dispatch(key(KeyModifiers::CONTROL, KeyCode::Right), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(3)));
        noop_dispatch(key(KeyModifiers::CONTROL, KeyCode::Right), &mut tabs);
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
    fn ctrl_right_on_empty_does_nothing() {
        let mut tabs = Tabs::default();
        noop_dispatch(key(KeyModifiers::CONTROL, KeyCode::Right), &mut tabs);
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
                disk_content_hash: led_core::PersistedContentHash::default(),
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
        let mut kbd_macro = led_state_kbd_macro::KbdMacroState::default();
        let keymap = default_keymap();

        let mut path_chains = std::collections::HashMap::new();
        let mut completions = CompletionsState::default();
        let mut completions_pending = led_state_completions::CompletionsPending::default();
        let mut lsp_extras = LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let diagnostics = DiagnosticsStates::default();
        let lsp_status = led_state_diagnostics::LspStatuses::default();
        let git = GitState::default();
        {
            let mut dispatcher = Dispatcher {
                tabs: &mut tabs,
                edits: &mut edits,
                kill_ring: &mut kill_ring,
                clip: &mut clip,
                alerts: &mut alerts,
                jumps: &mut jumps,
                browser: &mut browser,
                fs: &fs,
                store: &store,
                terminal: &term,
                find_file: &mut find_file,
                isearch: &mut isearch,
                file_search: &mut file_search,
                completions: &mut completions,
                completions_pending: &mut completions_pending,
                lsp_extras: &mut lsp_extras,
                lsp_pending: &mut lsp_pending,
                diagnostics: &diagnostics,
                lsp_status: &lsp_status,
                git: &git,
                path_chains: &mut path_chains,
                keymap: &keymap,
                chord: &mut chord,
                kbd_macro: &mut kbd_macro,
            };
            // Ctrl-x k on dirty active tab → prompt set, tab still open.
            dispatcher.dispatch_key(key(KeyModifiers::CONTROL, KeyCode::Char('x')));
            dispatcher.dispatch_key(key(KeyModifiers::NONE, KeyCode::Char('k')));
        }
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
        let mut kbd_macro = led_state_kbd_macro::KbdMacroState::default();
        let keymap = default_keymap();

        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let mut path_chains = std::collections::HashMap::new();
        let mut completions = CompletionsState::default();
        let mut completions_pending = led_state_completions::CompletionsPending::default();
        let mut lsp_extras = LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        let diagnostics = DiagnosticsStates::default();
        let lsp_status = led_state_diagnostics::LspStatuses::default();
        let git = GitState::default();
        {
            let mut dispatcher = Dispatcher {
                tabs: &mut tabs,
                edits: &mut edits,
                kill_ring: &mut kill_ring,
                clip: &mut clip,
                alerts: &mut alerts,
                jumps: &mut jumps,
                browser: &mut browser,
                fs: &fs,
                store: &store,
                terminal: &term,
                find_file: &mut find_file,
                isearch: &mut isearch,
                file_search: &mut file_search,
                completions: &mut completions,
                completions_pending: &mut completions_pending,
                lsp_extras: &mut lsp_extras,
                lsp_pending: &mut lsp_pending,
                diagnostics: &diagnostics,
                lsp_status: &lsp_status,
                git: &git,
                path_chains: &mut path_chains,
                keymap: &keymap,
                chord: &mut chord,
                kbd_macro: &mut kbd_macro,
            };
            dispatcher.dispatch_key(key(KeyModifiers::NONE, KeyCode::Char('y')));
        }
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
        let mut kbd_macro = led_state_kbd_macro::KbdMacroState::default();
        let keymap = default_keymap();

        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let mut path_chains = std::collections::HashMap::new();
        let mut completions = CompletionsState::default();
        let mut completions_pending = led_state_completions::CompletionsPending::default();
        let mut lsp_extras = LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        let diagnostics = DiagnosticsStates::default();
        let lsp_status = led_state_diagnostics::LspStatuses::default();
        let git = GitState::default();
        {
            let mut dispatcher = Dispatcher {
                tabs: &mut tabs,
                edits: &mut edits,
                kill_ring: &mut kill_ring,
                clip: &mut clip,
                alerts: &mut alerts,
                jumps: &mut jumps,
                browser: &mut browser,
                fs: &fs,
                store: &store,
                terminal: &term,
                find_file: &mut find_file,
                isearch: &mut isearch,
                file_search: &mut file_search,
                completions: &mut completions,
                completions_pending: &mut completions_pending,
                lsp_extras: &mut lsp_extras,
                lsp_pending: &mut lsp_pending,
                diagnostics: &diagnostics,
                lsp_status: &lsp_status,
                git: &git,
                path_chains: &mut path_chains,
                keymap: &keymap,
                chord: &mut chord,
                kbd_macro: &mut kbd_macro,
            };
            dispatcher.dispatch_key(key(KeyModifiers::NONE, KeyCode::Char('Y')));
        }
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
        let mut kbd_macro = led_state_kbd_macro::KbdMacroState::default();
        let keymap = default_keymap();

        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let mut path_chains = std::collections::HashMap::new();
        let mut completions = CompletionsState::default();
        let mut completions_pending = led_state_completions::CompletionsPending::default();
        let mut lsp_extras = LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        let diagnostics = DiagnosticsStates::default();
        let lsp_status = led_state_diagnostics::LspStatuses::default();
        let git = GitState::default();
        {
            let mut dispatcher = Dispatcher {
                tabs: &mut tabs,
                edits: &mut edits,
                kill_ring: &mut kill_ring,
                clip: &mut clip,
                alerts: &mut alerts,
                jumps: &mut jumps,
                browser: &mut browser,
                fs: &fs,
                store: &store,
                terminal: &term,
                find_file: &mut find_file,
                isearch: &mut isearch,
                file_search: &mut file_search,
                completions: &mut completions,
                completions_pending: &mut completions_pending,
                lsp_extras: &mut lsp_extras,
                lsp_pending: &mut lsp_pending,
                diagnostics: &diagnostics,
                lsp_status: &lsp_status,
                git: &git,
                path_chains: &mut path_chains,
                keymap: &keymap,
                chord: &mut chord,
                kbd_macro: &mut kbd_macro,
            };
            dispatcher.dispatch_key(key(KeyModifiers::NONE, KeyCode::Char('n')));
        }
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
        let mut kbd_macro = led_state_kbd_macro::KbdMacroState::default();
        let keymap = default_keymap();

        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let mut path_chains = std::collections::HashMap::new();
        let mut completions = CompletionsState::default();
        let mut completions_pending = led_state_completions::CompletionsPending::default();
        let mut lsp_extras = LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        let diagnostics = DiagnosticsStates::default();
        let lsp_status = led_state_diagnostics::LspStatuses::default();
        let git = GitState::default();
        {
            let mut dispatcher = Dispatcher {
                tabs: &mut tabs,
                edits: &mut edits,
                kill_ring: &mut kill_ring,
                clip: &mut clip,
                alerts: &mut alerts,
                jumps: &mut jumps,
                browser: &mut browser,
                fs: &fs,
                store: &store,
                terminal: &term,
                find_file: &mut find_file,
                isearch: &mut isearch,
                file_search: &mut file_search,
                completions: &mut completions,
                completions_pending: &mut completions_pending,
                lsp_extras: &mut lsp_extras,
                lsp_pending: &mut lsp_pending,
                diagnostics: &diagnostics,
                lsp_status: &lsp_status,
                git: &git,
                path_chains: &mut path_chains,
                keymap: &keymap,
                chord: &mut chord,
                kbd_macro: &mut kbd_macro,
            };
            dispatcher.dispatch_key(key(KeyModifiers::NONE, KeyCode::Esc));
        }
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
        let mut kbd_macro = led_state_kbd_macro::KbdMacroState::default();
        let keymap = default_keymap();

        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let mut path_chains = std::collections::HashMap::new();
        let mut completions = CompletionsState::default();
        let mut completions_pending = led_state_completions::CompletionsPending::default();
        let mut lsp_extras = LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        let diagnostics = DiagnosticsStates::default();
        let lsp_status = led_state_diagnostics::LspStatuses::default();
        let git = GitState::default();
        {
            let mut dispatcher = Dispatcher {
                tabs: &mut tabs,
                edits: &mut edits,
                kill_ring: &mut kill_ring,
                clip: &mut clip,
                alerts: &mut alerts,
                jumps: &mut jumps,
                browser: &mut browser,
                fs: &fs,
                store: &store,
                terminal: &term,
                find_file: &mut find_file,
                isearch: &mut isearch,
                file_search: &mut file_search,
                completions: &mut completions,
                completions_pending: &mut completions_pending,
                lsp_extras: &mut lsp_extras,
                lsp_pending: &mut lsp_pending,
                diagnostics: &diagnostics,
                lsp_status: &lsp_status,
                git: &git,
                path_chains: &mut path_chains,
                keymap: &keymap,
                chord: &mut chord,
                kbd_macro: &mut kbd_macro,
            };
            dispatcher.dispatch_key(key(KeyModifiers::CONTROL, KeyCode::Char('x')));
            dispatcher.dispatch_key(key(KeyModifiers::NONE, KeyCode::Char('k')));
        }
        assert!(alerts.confirm_kill.is_none());
        assert_eq!(tabs.open.len(), 1);
    }
}
