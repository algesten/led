use std::sync::Arc;

use led_core::Doc;
use led_state::{BufferState, Dimensions};

// ── Coordinate conversion ──

fn row_col_to_char(doc: &dyn Doc, row: usize, col: usize) -> usize {
    let row = row.min(doc.line_count().saturating_sub(1));
    let len = doc.line_len(row);
    let col = col.min(len);
    doc.line_to_char(row) + col
}

// ── Editing ──

pub fn insert_char(buf: &BufferState, ch: char) -> (Arc<dyn Doc>, usize, usize, usize) {
    let idx = row_col_to_char(&*buf.doc, buf.cursor_row, buf.cursor_col);
    let doc = buf.doc.insert(idx, &ch.to_string());
    let col = buf.cursor_col + 1;
    (doc, buf.cursor_row, col, col)
}

pub fn insert_newline(buf: &BufferState) -> (Arc<dyn Doc>, usize, usize, usize) {
    let idx = row_col_to_char(&*buf.doc, buf.cursor_row, buf.cursor_col);
    let doc = buf.doc.insert(idx, "\n");
    (doc, buf.cursor_row + 1, 0, 0)
}

pub fn insert_tab(buf: &BufferState, dims: &Dimensions) -> (Arc<dyn Doc>, usize, usize, usize) {
    let tab_width = dims.tab_stop;
    let spaces = tab_width - (buf.cursor_col % tab_width);
    let text: String = " ".repeat(spaces);
    let idx = row_col_to_char(&*buf.doc, buf.cursor_row, buf.cursor_col);
    let doc = buf.doc.insert(idx, &text);
    let col = buf.cursor_col + spaces;
    (doc, buf.cursor_row, col, col)
}

pub fn delete_backward(buf: &BufferState) -> Option<(Arc<dyn Doc>, usize, usize, usize)> {
    if buf.cursor_col > 0 {
        let idx = row_col_to_char(&*buf.doc, buf.cursor_row, buf.cursor_col);
        let doc = buf.doc.remove(idx - 1, idx);
        let col = buf.cursor_col - 1;
        Some((doc, buf.cursor_row, col, col))
    } else if buf.cursor_row > 0 {
        // Join with previous line
        let idx = buf.doc.line_to_char(buf.cursor_row);
        let col = buf.doc.line_len(buf.cursor_row - 1);
        let doc = buf.doc.remove(idx - 1, idx);
        Some((doc, buf.cursor_row - 1, col, col))
    } else {
        None
    }
}

pub fn delete_forward(buf: &BufferState) -> Option<(Arc<dyn Doc>, usize, usize, usize)> {
    let len = buf.doc.line_len(buf.cursor_row);
    if buf.cursor_col < len {
        let idx = row_col_to_char(&*buf.doc, buf.cursor_row, buf.cursor_col);
        let doc = buf.doc.remove(idx, idx + 1);
        Some((doc, buf.cursor_row, buf.cursor_col, buf.cursor_col))
    } else if buf.cursor_row < buf.doc.line_count().saturating_sub(1) {
        // Join with next line — remove the newline at end of current line
        let idx = row_col_to_char(&*buf.doc, buf.cursor_row, len);
        let doc = buf.doc.remove(idx, idx + 1);
        Some((doc, buf.cursor_row, buf.cursor_col, buf.cursor_col))
    } else {
        None
    }
}

pub fn kill_line(buf: &BufferState) -> Option<(Arc<dyn Doc>, usize, usize, usize)> {
    let len = buf.doc.line_len(buf.cursor_row);
    if buf.cursor_col < len {
        // Kill from cursor to end of line
        let start = row_col_to_char(&*buf.doc, buf.cursor_row, buf.cursor_col);
        let end = row_col_to_char(&*buf.doc, buf.cursor_row, len);
        let doc = buf.doc.remove(start, end);
        Some((doc, buf.cursor_row, buf.cursor_col, buf.cursor_col))
    } else if buf.cursor_row < buf.doc.line_count().saturating_sub(1) {
        // Kill the newline — join with next line
        let idx = row_col_to_char(&*buf.doc, buf.cursor_row, len);
        let doc = buf.doc.remove(idx, idx + 1);
        Some((doc, buf.cursor_row, buf.cursor_col, buf.cursor_col))
    } else {
        None
    }
}
