use led_driver_terminal_core::{Rect, Style, TabBarModel, Theme};

use crate::buffer::Buffer;

pub(crate) fn paint_tab_bar(bar: &TabBarModel, area: Rect, theme: &Theme, buf: &mut Buffer) {
    // Tab bar at the bottom of the editor area: second-to-last row.
    // Matches legacy led's ratatui layout + the goldens.
    let row = area.y;
    let right_edge = area.x.saturating_sub(0).saturating_add(area.cols);
    // Each label paints as ` <label> ` — three runs of `put_str`.
    // Pre-compute the per-tab on-screen width so we can scroll the
    // visible window when the labels overflow.
    let widths: Vec<u16> = bar
        .labels
        .iter()
        .map(|l| 2 + l.chars().count().min(area.cols as usize) as u16)
        .collect();
    // Scroll the start index leftward until the active tab fits
    // within `area.cols`. Without this, long tab lists hide the
    // active tab off the right edge — legacy keeps the active tab
    // pinned in view.
    let mut start = 0usize;
    if let Some(active) = bar.active {
        loop {
            let mut used = 0u16;
            let mut last_visible = start;
            for (i, w) in widths.iter().enumerate().skip(start) {
                let next = used.saturating_add(*w);
                if next > area.cols {
                    break;
                }
                used = next;
                last_visible = i;
            }
            if active <= last_visible || start >= widths.len() {
                break;
            }
            start += 1;
        }
    }
    let mut col = area.x;
    for (i, label) in bar.labels.iter().enumerate().skip(start) {
        if col >= right_edge {
            break;
        }
        let active = bar.active == Some(i);
        let style = if active {
            theme.tab_active
        } else {
            theme.tab_inactive
        };
        col = buf.put_str(row, col, " ", style);
        col = buf.put_str(row, col, label, style);
        col = buf.put_str(row, col, " ", style);
        if col >= right_edge {
            break;
        }
    }
    // Blank the rest of the row at the terminal default — matches
    // the old `Clear(UntilNewLine)`.
    buf.fill_row(row, col, right_edge, Style::default());
}
