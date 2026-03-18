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

/// Kill from cursor to end of line (or the newline if at EOL).
/// Returns (doc, killed_text, row, col, affinity).
pub fn kill_line(buf: &BufferState) -> Option<(Arc<dyn Doc>, String, usize, usize, usize)> {
    let len = buf.doc.line_len(buf.cursor_row);
    if buf.cursor_col < len {
        // Kill from cursor to end of line
        let start = row_col_to_char(&*buf.doc, buf.cursor_row, buf.cursor_col);
        let end = row_col_to_char(&*buf.doc, buf.cursor_row, len);
        let killed = buf.doc.slice(start, end);
        let doc = buf.doc.remove(start, end);
        Some((doc, killed, buf.cursor_row, buf.cursor_col, buf.cursor_col))
    } else if buf.cursor_row < buf.doc.line_count().saturating_sub(1) {
        // Kill the newline — join with next line
        let idx = row_col_to_char(&*buf.doc, buf.cursor_row, len);
        let killed = buf.doc.slice(idx, idx + 1);
        let doc = buf.doc.remove(idx, idx + 1);
        Some((doc, killed, buf.cursor_row, buf.cursor_col, buf.cursor_col))
    } else {
        None
    }
}

/// Kill the region between mark and cursor.
/// Returns (doc, killed_text, row, col, affinity).
pub fn kill_region(buf: &BufferState) -> Option<(Arc<dyn Doc>, String, usize, usize, usize)> {
    let (mark_row, mark_col) = buf.mark?;
    let start = row_col_to_char(&*buf.doc, mark_row, mark_col);
    let end = row_col_to_char(&*buf.doc, buf.cursor_row, buf.cursor_col);
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    if start == end {
        return None;
    }
    let killed = buf.doc.slice(start, end);
    let doc = buf.doc.remove(start, end);
    let row = doc.char_to_line(start);
    let col = start - doc.line_to_char(row);
    Some((doc, killed, row, col, col))
}

/// Insert text at cursor (yank from kill ring).
/// Returns (doc, row, col, affinity).
pub fn yank(buf: &BufferState, text: &str) -> (Arc<dyn Doc>, usize, usize, usize) {
    let idx = row_col_to_char(&*buf.doc, buf.cursor_row, buf.cursor_col);
    let doc = buf.doc.insert(idx, text);
    let (row, col) = cursor_after_yank(buf.cursor_row, buf.cursor_col, text);
    (doc, row, col, col)
}

/// Compute cursor position after inserting `text` at (row, col).
fn cursor_after_yank(row: usize, col: usize, text: &str) -> (usize, usize) {
    let newline_count = text.chars().filter(|&c| c == '\n').count();
    if newline_count == 0 {
        (row, col + text.chars().count())
    } else {
        let last_line_len = text.rsplit('\n').next().map_or(0, |s| s.chars().count());
        (row + newline_count, last_line_len)
    }
}
