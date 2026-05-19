use led_driver_terminal_core::{Rect, StatusBarModel, Theme};

use crate::buffer::Buffer;

pub(crate) fn paint_status_bar(s: &StatusBarModel, area: Rect, _theme: &Theme, buf: &mut Buffer) {
    let row = area.y;
    let mut col = area.x;
    let right_edge = area.x.saturating_add(area.cols);

    // Row-wide styling — the runtime memo picks the warn-vs-normal
    // slot from theme and stamps it onto the model; the painter
    // just writes the cells.
    let row_style = s.row_style;

    let cols = area.cols as usize;
    let left_cols = s.left.chars().count().min(cols);
    let right_cols = s.right.chars().count().min(cols - left_cols);
    let pad = cols - left_cols - right_cols;

    col = buf.put_str(row, col, s.left.as_ref(), row_style);
    for _ in 0..pad {
        if col >= right_edge {
            break;
        }
        col = buf.put_str(row, col, " ", row_style);
    }
    col = buf.put_str(row, col, s.right.as_ref(), row_style);
    // Any trailing width gets blanked with the row's background
    // style so a short right-side string still has the bar tint
    // carry to the edge.
    buf.fill_row(row, col, right_edge, row_style);
}
