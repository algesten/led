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
