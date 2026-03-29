use led_core::Doc;
use led_state::BufferState;

// ── Coordinate conversion ──

fn row_col_to_char(doc: &dyn Doc, row: usize, col: usize) -> usize {
    let row = row.min(doc.line_count().saturating_sub(1));
    let len = doc.line_len(row);
    let col = col.min(len);
    doc.line_to_char(row) + col
}

// ── Editing ──

/// Insert a character at cursor. Returns (row, col, affinity).
pub fn insert_char(buf: &mut BufferState, ch: char) -> (usize, usize, usize) {
    let idx = row_col_to_char(&**buf.doc(), buf.cursor_row(), buf.cursor_col());
    buf.insert_text(idx, &ch.to_string());
    let col = buf.cursor_col() + 1;
    (buf.cursor_row(), col, col)
}

/// Insert newline at cursor. Returns (row, col, affinity).
pub fn insert_newline(buf: &mut BufferState) -> (usize, usize, usize) {
    let idx = row_col_to_char(&**buf.doc(), buf.cursor_row(), buf.cursor_col());
    buf.insert_text(idx, "\n");
    (buf.cursor_row() + 1, 0, 0)
}

fn get_line_indent(doc: &dyn Doc, line: usize) -> String {
    let text = doc.line(line);
    let mut indent = String::new();
    for ch in text.chars() {
        if ch == ' ' || ch == '\t' {
            indent.push(ch);
        } else {
            break;
        }
    }
    indent
}

/// Apply computed indent to a line, replacing existing leading whitespace.
pub fn apply_indent(buf: &mut BufferState, row: usize, new_indent: &str, adjust_cursor: bool) {
    let old_indent = get_line_indent(&**buf.doc(), row);
    let old_len = old_indent.chars().count();
    let new_len = new_indent.chars().count();

    if *new_indent == old_indent {
        if adjust_cursor && buf.cursor_col() <= old_len {
            buf.set_cursor(buf.cursor_row(), old_len, old_len);
        }
        return;
    }

    let line_start = buf.doc().line_to_char(row);
    if old_len > 0 {
        buf.remove_text(line_start, line_start + old_len);
    }
    if !new_indent.is_empty() {
        buf.insert_text(line_start, new_indent);
    }

    if adjust_cursor {
        let col = if buf.cursor_col() <= old_len {
            new_len
        } else {
            buf.cursor_col() - old_len + new_len
        };
        buf.set_cursor(buf.cursor_row(), col, col);
    }
}

/// Insert spaces to the next tab stop at the cursor position.
pub fn insert_soft_tab(buf: &mut BufferState, tab_stop: usize) {
    let spaces = tab_stop - (buf.cursor_col() % tab_stop);
    let text: String = " ".repeat(spaces);
    let idx = buf.doc().line_to_char(buf.cursor_row()) + buf.cursor_col();
    buf.insert_text(idx, &text);
    let new_col = buf.cursor_col() + spaces;
    buf.set_cursor(buf.cursor_row(), new_col, new_col);
}

/// Delete backward. Returns Some((row, col, affinity)) or None.
pub fn delete_backward(buf: &mut BufferState) -> Option<(usize, usize, usize)> {
    if buf.cursor_col() > 0 {
        let idx = row_col_to_char(&**buf.doc(), buf.cursor_row(), buf.cursor_col());
        buf.remove_text(idx - 1, idx);
        let col = buf.cursor_col() - 1;
        Some((buf.cursor_row(), col, col))
    } else if buf.cursor_row() > 0 {
        // Join with previous line
        let idx = buf.doc().line_to_char(buf.cursor_row());
        let col = buf.doc().line_len(buf.cursor_row() - 1);
        buf.remove_text(idx - 1, idx);
        Some((buf.cursor_row() - 1, col, col))
    } else {
        None
    }
}

/// Delete forward. Returns Some((row, col, affinity)) or None.
pub fn delete_forward(buf: &mut BufferState) -> Option<(usize, usize, usize)> {
    let len = buf.doc().line_len(buf.cursor_row());
    if buf.cursor_col() < len {
        let idx = row_col_to_char(&**buf.doc(), buf.cursor_row(), buf.cursor_col());
        buf.remove_text(idx, idx + 1);
        Some((buf.cursor_row(), buf.cursor_col(), buf.cursor_col()))
    } else if buf.cursor_row() < buf.doc().line_count().saturating_sub(1) {
        let idx = row_col_to_char(&**buf.doc(), buf.cursor_row(), len);
        buf.remove_text(idx, idx + 1);
        Some((buf.cursor_row(), buf.cursor_col(), buf.cursor_col()))
    } else {
        None
    }
}

/// Kill from cursor to end of line (or the newline if at EOL).
/// Returns Some((killed_text, row, col, affinity)) or None.
pub fn kill_line(buf: &mut BufferState) -> Option<(String, usize, usize, usize)> {
    let len = buf.doc().line_len(buf.cursor_row());
    if buf.cursor_col() < len {
        let start = row_col_to_char(&**buf.doc(), buf.cursor_row(), buf.cursor_col());
        let end = row_col_to_char(&**buf.doc(), buf.cursor_row(), len);
        let killed = buf.doc().slice(start, end);
        buf.remove_text(start, end);
        Some((killed, buf.cursor_row(), buf.cursor_col(), buf.cursor_col()))
    } else if buf.cursor_row() < buf.doc().line_count().saturating_sub(1) {
        let idx = row_col_to_char(&**buf.doc(), buf.cursor_row(), len);
        let killed = buf.doc().slice(idx, idx + 1);
        buf.remove_text(idx, idx + 1);
        Some((killed, buf.cursor_row(), buf.cursor_col(), buf.cursor_col()))
    } else {
        None
    }
}

/// Return the text between mark and cursor without modifying the document.
pub fn selected_text(buf: &BufferState) -> Option<String> {
    let (mark_row, mark_col) = buf.mark()?;
    let start = row_col_to_char(&**buf.doc(), mark_row, mark_col);
    let end = row_col_to_char(&**buf.doc(), buf.cursor_row(), buf.cursor_col());
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    if start == end {
        return None;
    }
    Some(buf.doc().slice(start, end))
}

/// Kill the region between mark and cursor.
/// Returns Some((killed_text, row, col, affinity)) or None.
pub fn kill_region(buf: &mut BufferState) -> Option<(String, usize, usize, usize)> {
    let (mark_row, mark_col) = buf.mark()?;
    let start = row_col_to_char(&**buf.doc(), mark_row, mark_col);
    let end = row_col_to_char(&**buf.doc(), buf.cursor_row(), buf.cursor_col());
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    if start == end {
        return None;
    }
    let killed = buf.doc().slice(start, end);
    buf.remove_text(start, end);
    let row = buf.doc().char_to_line(start);
    let col = start - buf.doc().line_to_char(row);
    Some((killed, row, col, col))
}

/// Insert text at cursor (yank from kill ring). Returns (row, col, affinity).
pub fn yank(buf: &mut BufferState, text: &str) -> (usize, usize, usize) {
    let idx = row_col_to_char(&**buf.doc(), buf.cursor_row(), buf.cursor_col());
    buf.insert_text(idx, text);
    let (row, col) = cursor_after_yank(buf.cursor_row(), buf.cursor_col(), text);
    (row, col, col)
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
