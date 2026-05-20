use led_driver_terminal_core::{Rect, Style, TabBarModel, Theme};

use crate::buffer::Buffer;

pub(crate) fn paint_tab_bar(bar: &TabBarModel, area: Rect, theme: &Theme, buf: &mut Buffer) {
    // Tab bar at the bottom of the editor area: second-to-last row.
    // Matches legacy led's ratatui layout + the goldens.
    //
    // `bar.scroll_start` was derived by the runtime memo
    // (`query::render::tab_bar::tab_bar_model`) so the painter no
    // longer needs the per-frame `Vec<u16>` width pre-compute + O(n²)
    // "scroll the active tab into view" loop. We just walk from the
    // pre-computed start index until the bar's right edge.
    let row = area.y;
    let right_edge = area.x.saturating_add(area.cols);
    let mut col = area.x;
    for (i, label) in bar.labels.iter().enumerate().skip(bar.scroll_start) {
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
