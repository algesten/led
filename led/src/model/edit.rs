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
    let indent = compute_newline_indent(&*buf.doc, buf.cursor_row, buf.cursor_col);
    let text = format!("\n{indent}");
    let doc = buf.doc.insert(idx, &text);
    let col = indent.chars().count();
    (doc, buf.cursor_row + 1, col, col)
}

/// Compute the indentation to insert after a newline.
/// Uses a simple heuristic: copy previous line's indent, increase if the
/// line before cursor ends with an opener (`{`, `(`, `[`), decrease if the
/// first non-whitespace of the upcoming text is a closer.
fn compute_newline_indent(doc: &dyn Doc, row: usize, col: usize) -> String {
    let line = doc.line(row);
    let indent_unit = detect_indent_unit(doc);

    // Get the text before the cursor on this line
    let before_cursor: String = line.chars().take(col).collect();

    // Base indent: copy current line's leading whitespace
    let mut base = String::new();
    for ch in line.chars() {
        if ch == ' ' || ch == '\t' {
            base.push(ch);
        } else {
            break;
        }
    }

    // Check if the text before cursor ends with an opener
    let trimmed = before_cursor.trim_end();
    let opens = trimmed.ends_with('{') || trimmed.ends_with('(') || trimmed.ends_with('[');

    if opens {
        base.push_str(&indent_unit);
    }

    base
}

fn detect_indent_unit(doc: &dyn Doc) -> String {
    let lines = doc.line_count().min(100);
    for i in 0..lines {
        let line = doc.line(i);
        let mut indent = String::new();
        for ch in line.chars() {
            if ch == '\t' {
                return "\t".to_string();
            } else if ch == ' ' {
                indent.push(' ');
            } else {
                break;
            }
        }
        if !indent.is_empty() {
            return indent;
        }
    }
    "    ".to_string()
}

/// Insert a closing bracket character. If the line before the cursor is all
/// whitespace (i.e., the bracket will be the first non-whitespace char),
/// re-indent the line to one level less than the previous non-blank line.
pub fn insert_close_bracket(buf: &BufferState, ch: char) -> (Arc<dyn Doc>, usize, usize, usize) {
    let line = buf.doc.line(buf.cursor_row);
    let before_cursor: String = line.chars().take(buf.cursor_col).collect();
    let all_ws = before_cursor.chars().all(|c| c == ' ' || c == '\t');

    if all_ws && buf.cursor_row > 0 {
        // Re-indent: find the matching opener's line indent
        let indent_unit = detect_indent_unit(&*buf.doc);
        let target_indent = find_dedent_for_close(&*buf.doc, buf.cursor_row, &indent_unit);

        // Replace the whitespace before cursor with the target indent + bracket
        let line_start = buf.doc.line_to_char(buf.cursor_row);
        let cursor_char = line_start + buf.cursor_col;

        // Remove existing whitespace
        let doc = if buf.cursor_col > 0 {
            buf.doc.remove(line_start, cursor_char)
        } else {
            buf.doc.clone()
        };
        // Insert target_indent + bracket
        let text = format!("{target_indent}{ch}");
        let col = text.chars().count();
        let doc = doc.insert(line_start, &text);
        (doc, buf.cursor_row, col, col)
    } else {
        insert_char(buf, ch)
    }
}

/// Find the indent to apply for a closing bracket on `line`.
/// Looks backwards for the matching opener line's indent.
fn find_dedent_for_close(doc: &dyn Doc, line: usize, indent_unit: &str) -> String {
    // Simple heuristic: find the previous non-blank line and dedent by one level
    // from its indent. This works because the previous line is typically inside the
    // block that the closing bracket is terminating.
    for row in (0..line).rev() {
        let text = doc.line(row);
        if text.chars().all(|c| c.is_whitespace()) {
            continue;
        }
        let mut indent = String::new();
        for ch in text.chars() {
            if ch == ' ' || ch == '\t' {
                indent.push(ch);
            } else {
                break;
            }
        }
        // If this line opened a block (ends with opener), return its indent
        let trimmed = text.trim_end();
        if trimmed.ends_with('{') || trimmed.ends_with('(') || trimmed.ends_with('[') {
            return indent;
        }
        // Otherwise dedent one level from this line's indent
        if indent.ends_with(indent_unit) {
            return indent[..indent.len() - indent_unit.len()].to_string();
        }
        return indent;
    }
    String::new()
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
