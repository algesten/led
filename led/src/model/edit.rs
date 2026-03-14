use std::sync::Arc;

use led_core::Doc;
use led_state::BufferState;

// ── Coordinate conversion ──

pub fn row_col_to_char(doc: &dyn Doc, row: usize, col: usize) -> usize {
    let row = row.min(doc.line_count().saturating_sub(1));
    let len = doc.line_len(row);
    let col = col.min(len);
    doc.line_to_char(row) + col
}

// ── Scroll ──

const SCROLL_MARGIN: usize = 3;

pub fn ensure_cursor_visible(cursor_row: usize, scroll_row: usize, height: usize) -> usize {
    if height == 0 {
        return scroll_row;
    }
    let margin = SCROLL_MARGIN.min(height / 2);
    if cursor_row < scroll_row + margin {
        cursor_row.saturating_sub(margin)
    } else if cursor_row >= scroll_row + height - margin {
        cursor_row + margin + 1 - height
    } else {
        scroll_row
    }
}

// ── Movement ──

pub fn move_up(buf: &BufferState) -> (usize, usize, usize) {
    let row = buf.cursor_row.saturating_sub(1);
    let len = buf.doc.line_len(row);
    let col = buf.cursor_col_affinity.min(len);
    (row, col, buf.cursor_col_affinity)
}

pub fn move_down(buf: &BufferState) -> (usize, usize, usize) {
    let max_row = buf.doc.line_count().saturating_sub(1);
    let row = (buf.cursor_row + 1).min(max_row);
    let len = buf.doc.line_len(row);
    let col = buf.cursor_col_affinity.min(len);
    (row, col, buf.cursor_col_affinity)
}

pub fn move_left(buf: &BufferState) -> (usize, usize, usize) {
    if buf.cursor_col > 0 {
        let col = buf.cursor_col - 1;
        (buf.cursor_row, col, col)
    } else if buf.cursor_row > 0 {
        let row = buf.cursor_row - 1;
        let col = buf.doc.line_len(row);
        (row, col, col)
    } else {
        (0, 0, 0)
    }
}

pub fn move_right(buf: &BufferState) -> (usize, usize, usize) {
    let len = buf.doc.line_len(buf.cursor_row);
    if buf.cursor_col < len {
        let col = buf.cursor_col + 1;
        (buf.cursor_row, col, col)
    } else if buf.cursor_row < buf.doc.line_count().saturating_sub(1) {
        let row = buf.cursor_row + 1;
        (row, 0, 0)
    } else {
        (buf.cursor_row, len, len)
    }
}

pub fn line_start(buf: &BufferState) -> (usize, usize, usize) {
    (buf.cursor_row, 0, 0)
}

pub fn line_end(buf: &BufferState) -> (usize, usize, usize) {
    let col = buf.doc.line_len(buf.cursor_row);
    (buf.cursor_row, col, col)
}

pub fn page_up(buf: &BufferState, height: usize) -> (usize, usize, usize) {
    let page = height.saturating_sub(1).max(1);
    let row = buf.cursor_row.saturating_sub(page);
    let len = buf.doc.line_len(row);
    let col = buf.cursor_col_affinity.min(len);
    (row, col, buf.cursor_col_affinity)
}

pub fn page_down(buf: &BufferState, height: usize) -> (usize, usize, usize) {
    let page = height.saturating_sub(1).max(1);
    let max_row = buf.doc.line_count().saturating_sub(1);
    let row = (buf.cursor_row + page).min(max_row);
    let len = buf.doc.line_len(row);
    let col = buf.cursor_col_affinity.min(len);
    (row, col, buf.cursor_col_affinity)
}

pub fn file_start() -> (usize, usize, usize) {
    (0, 0, 0)
}

pub fn file_end(doc: &dyn Doc) -> (usize, usize, usize) {
    let row = doc.line_count().saturating_sub(1);
    let col = doc.line_len(row);
    (row, col, col)
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

pub fn insert_tab(buf: &BufferState) -> (Arc<dyn Doc>, usize, usize, usize) {
    let tab_width = 4;
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
