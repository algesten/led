//! Dispatch: applies `Event`s to atoms.
//!
//! Kept deliberately small per QUERY-ARCH § "The event handler". Each
//! function mutates atoms directly; no memos, no queries. Returns a
//! [`DispatchOutcome`] so the main loop can learn that a quit was
//! requested without looking for a sentinel in state.

use led_driver_terminal_core::{KeyCode, KeyEvent, KeyModifiers};
use led_state_tabs::{Tab, Tabs};

use crate::Event;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    Continue,
    Quit,
}

/// Top-level entry point used by the main loop.
pub fn dispatch(ev: Event, tabs: &mut Tabs) -> DispatchOutcome {
    match ev {
        Event::Key(k) => dispatch_key(k, tabs),
        // `Resize` is applied inside `TerminalInputDriver.process` —
        // pure state, no dispatch work here. The event is still
        // surfaced so dispatch *could* react later (e.g. clamping
        // cursors in M2+); for now, ignore.
        Event::Resize(_) => DispatchOutcome::Continue,
        Event::Quit => DispatchOutcome::Quit,
    }
}

pub fn dispatch_key(k: KeyEvent, tabs: &mut Tabs) -> DispatchOutcome {
    match (k.modifiers, k.code) {
        (m, KeyCode::Char('c')) if m.contains(KeyModifiers::CONTROL) => DispatchOutcome::Quit,
        (m, KeyCode::Tab) if m.is_empty() => {
            cycle_active(tabs, 1);
            DispatchOutcome::Continue
        }
        (m, KeyCode::BackTab) if m.contains(KeyModifiers::SHIFT) || m.is_empty() => {
            // Many terminals emit BackTab without an explicit SHIFT flag;
            // accept both for robustness.
            cycle_active(tabs, -1);
            DispatchOutcome::Continue
        }
        _ => DispatchOutcome::Continue,
    }
}

fn cycle_active(tabs: &mut Tabs, delta: isize) {
    if tabs.open.is_empty() {
        return;
    }
    let n = tabs.open.len() as isize;
    let cur_idx = tabs
        .active
        .and_then(|id| tabs.open.iter().position(|t: &Tab| t.id == id))
        .unwrap_or(0) as isize;
    let next_idx = (cur_idx + delta).rem_euclid(n) as usize;
    tabs.active = Some(tabs.open[next_idx].id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::{CanonPath, UserPath};
    use led_driver_terminal_core::{KeyCode, KeyEvent, KeyModifiers};
    use led_state_tabs::{Tab, TabId, Tabs};

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn tabs_with(paths: &[(&str, u64)], active: Option<u64>) -> Tabs {
        let mut t = Tabs::default();
        for (p, id) in paths {
            t.open.push_back(Tab {
                id: TabId(*id),
                path: canon(p),
            });
        }
        t.active = active.map(TabId);
        t
    }

    fn key(mods: KeyModifiers, code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
        }
    }

    #[test]
    fn tab_cycles_active_forward() {
        let mut tabs = tabs_with(&[("a", 1), ("b", 2), ("c", 3)], Some(1));
        dispatch_key(key(KeyModifiers::NONE, KeyCode::Tab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(2)));
        dispatch_key(key(KeyModifiers::NONE, KeyCode::Tab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(3)));
        dispatch_key(key(KeyModifiers::NONE, KeyCode::Tab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(1)));
    }

    #[test]
    fn shift_tab_cycles_backward() {
        let mut tabs = tabs_with(&[("a", 1), ("b", 2), ("c", 3)], Some(1));
        dispatch_key(key(KeyModifiers::SHIFT, KeyCode::BackTab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(3)));
        dispatch_key(key(KeyModifiers::SHIFT, KeyCode::BackTab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(2)));
    }

    #[test]
    fn ctrl_c_signals_quit() {
        let mut tabs = tabs_with(&[("a", 1)], Some(1));
        let outcome = dispatch_key(key(KeyModifiers::CONTROL, KeyCode::Char('c')), &mut tabs);
        assert_eq!(outcome, DispatchOutcome::Quit);
    }

    #[test]
    fn tab_on_empty_does_nothing() {
        let mut tabs = Tabs::default();
        dispatch_key(key(KeyModifiers::NONE, KeyCode::Tab), &mut tabs);
        assert_eq!(tabs.active, None);
    }
}
