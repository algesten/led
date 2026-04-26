//! Mark + region helpers (M7). Mark is per-tab state; region is
//! derived on read from mark + cursor.

use led_state_tabs::{Tab, Tabs};
use ropey::Rope;

use super::shared::cursor_to_char;

pub(super) fn set_mark_active(tabs: &mut Tabs) {
    let Some(id) = tabs.active else {
        return;
    };
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    let tab = &mut tabs.open[idx];
    tab.mark = Some(tab.cursor);
}

pub(super) fn clear_mark(tabs: &mut Tabs) {
    let Some(id) = tabs.active else {
        return;
    };
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    tabs.open[idx].mark = None;
}

/// Region bounds in char indices: `(start, end)` with
/// `start <= end`, both clamped to the rope. Returns `None` when
/// there's no active tab, no mark, or mark and cursor resolve to
/// the same char index (empty region).
pub(super) fn region_range(tab: &Tab, rope: &Rope) -> Option<(usize, usize)> {
    let mark = tab.mark?;
    let a = cursor_to_char(&mark, rope);
    let b = cursor_to_char(&tab.cursor, rope);
    if a == b {
        return None;
    }
    Some((a.min(b), a.max(b)))
}

#[cfg(test)]
mod tests {
    

    
    
    use led_driver_terminal_core::{Dims, KeyCode, KeyModifiers};
    
    
    use led_state_clipboard::ClipboardState;
    use led_state_kill_ring::KillRing;
    use led_state_tabs::Cursor;
    

    
    use super::super::testutil::*;
    
    

    #[test]
    fn set_mark_captures_current_cursor() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abc\ndef", Dims { cols: 20, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 1,
            col: 2,
            preferred_col: 2,
        };
        let mut kr = KillRing::default();
        let mut clip = ClipboardState::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char(' ')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &mut clip,
            &store,
            &term,
        );
        assert_eq!(
            tabs.open[0].mark,
            Some(Cursor {
                line: 1,
                col: 2,
                preferred_col: 2,
            })
        );
    }

    #[test]
    fn abort_clears_mark() {
        let mut tabs = tabs_with(&[("a", 1)], Some(1));
        tabs.open[0].mark = Some(Cursor {
            line: 0,
            col: 3,
            preferred_col: 3,
        });
        noop_dispatch(key(KeyModifiers::NONE, KeyCode::Esc), &mut tabs);
        assert!(tabs.open[0].mark.is_none());
    }
}
