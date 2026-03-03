use std::fs::File;
use std::io::{self, BufReader, BufWriter};
use std::path::PathBuf;
use std::time::Instant;

use ropey::Rope;

// ---------------------------------------------------------------------------
// Undo data structures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum EditOp {
    Insert { char_idx: usize, text: String },
    Remove { char_idx: usize, text: String },
}

#[derive(Debug, Clone)]
struct UndoEntry {
    op: EditOp,
    cursor_before: (usize, usize),
    cursor_after: (usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Insert,
    DeleteBackward,
    DeleteForward,
}

#[derive(Debug)]
struct PendingGroup {
    kind: EditKind,
    op: EditOp,
    cursor_before: (usize, usize),
    cursor_after: (usize, usize),
    last_time: Instant,
}

const GROUP_TIMEOUT_MS: u128 = 1000;

// ---------------------------------------------------------------------------
// Buffer
// ---------------------------------------------------------------------------

pub struct Buffer {
    rope: Rope,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    pub scroll_offset: usize,
    undo_history: Vec<UndoEntry>,
    undo_cursor: Option<usize>,
    pending_group: Option<PendingGroup>,
}

impl Buffer {
    // --- Constructors ---

    pub fn empty() -> Self {
        Self {
            rope: Rope::from_str(""),
            cursor_row: 0,
            cursor_col: 0,
            path: None,
            dirty: false,
            scroll_offset: 0,
            undo_history: Vec::new(),
            undo_cursor: None,
            pending_group: None,
        }
    }

    pub fn from_file(path: &str) -> io::Result<Self> {
        let file = File::open(path)?;
        let rope = Rope::from_reader(BufReader::new(file))?;
        // Ensure at least one line (empty file → empty rope is fine, len_lines() == 1)
        Ok(Self {
            rope,
            cursor_row: 0,
            cursor_col: 0,
            path: Some(PathBuf::from(path)),
            dirty: false,
            scroll_offset: 0,
            undo_history: Vec::new(),
            undo_cursor: None,
            pending_group: None,
        })
    }

    pub fn save(&mut self) -> io::Result<()> {
        if let Some(ref path) = self.path {
            // Ensure trailing newline
            let len = self.rope.len_chars();
            if len == 0 || self.rope.char(len - 1) != '\n' {
                self.rope.insert_char(len, '\n');
            }
            let file = File::create(path)?;
            self.rope.write_to(BufWriter::new(file))?;
            self.dirty = false;
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::Other, "No file path set"))
        }
    }

    // --- Accessors ---

    pub fn filename(&self) -> &str {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("[scratch]")
    }

    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn line(&self, idx: usize) -> String {
        let rope_line = self.rope.line(idx);
        let s = rope_line.to_string();
        // Strip trailing newline (ropey includes it for all lines except possibly the last)
        s.trim_end_matches('\n').to_string()
    }

    pub fn line_len(&self, idx: usize) -> usize {
        let rope_line = self.rope.line(idx);
        let len = rope_line.len_chars();
        // Subtract trailing newline if present
        if len > 0 && rope_line.char(len - 1) == '\n' {
            len - 1
        } else {
            len
        }
    }

    fn char_idx(&self, row: usize, col: usize) -> usize {
        self.rope.line_to_char(row) + col
    }

    // --- Cursor movement ---

    pub fn move_up(&mut self) {
        self.break_undo_chain();
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.clamp_cursor_col();
        }
    }

    pub fn move_down(&mut self) {
        self.break_undo_chain();
        if self.cursor_row + 1 < self.rope.len_lines() {
            self.cursor_row += 1;
            self.clamp_cursor_col();
        }
    }

    pub fn move_left(&mut self) {
        self.break_undo_chain();
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.current_line_len();
        }
    }

    pub fn move_right(&mut self) {
        self.break_undo_chain();
        let len = self.current_line_len();
        if self.cursor_col < len {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.rope.len_lines() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    pub fn move_to_line_start(&mut self) {
        self.break_undo_chain();
        self.cursor_col = 0;
    }

    pub fn move_to_line_end(&mut self) {
        self.break_undo_chain();
        self.cursor_col = self.current_line_len();
    }

    pub fn page_up(&mut self, page_size: usize) {
        self.break_undo_chain();
        self.cursor_row = self.cursor_row.saturating_sub(page_size);
        self.clamp_cursor_col();
    }

    pub fn page_down(&mut self, page_size: usize) {
        self.break_undo_chain();
        self.cursor_row =
            (self.cursor_row + page_size).min(self.rope.len_lines().saturating_sub(1));
        self.clamp_cursor_col();
    }

    pub fn move_to_file_start(&mut self) {
        self.break_undo_chain();
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    pub fn move_to_file_end(&mut self) {
        self.break_undo_chain();
        self.cursor_row = self.rope.len_lines().saturating_sub(1);
        self.cursor_col = self.current_line_len();
    }

    // --- Text editing ---

    pub fn insert_char(&mut self, ch: char) {
        let cursor_before = (self.cursor_row, self.cursor_col);
        let idx = self.char_idx(self.cursor_row, self.cursor_col);
        self.rope.insert_char(idx, ch);
        if ch == '\n' {
            self.cursor_row += 1;
            self.cursor_col = 0;
        } else {
            self.cursor_col += 1;
        }
        self.dirty = true;
        let cursor_after = (self.cursor_row, self.cursor_col);

        // Newlines break grouping, just push directly
        if ch == '\n' {
            self.flush_pending();
            self.push_undo(UndoEntry {
                op: EditOp::Insert {
                    char_idx: idx,
                    text: "\n".to_string(),
                },
                cursor_before,
                cursor_after,
            });
        } else {
            self.record_edit(
                EditKind::Insert,
                EditOp::Insert {
                    char_idx: idx,
                    text: ch.to_string(),
                },
                cursor_before,
                cursor_after,
            );
        }
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    pub fn delete_char_backward(&mut self) {
        let cursor_before = (self.cursor_row, self.cursor_col);
        if self.cursor_col > 0 {
            let idx = self.char_idx(self.cursor_row, self.cursor_col);
            let removed = self.rope.char(idx - 1);
            self.rope.remove(idx - 1..idx);
            self.cursor_col -= 1;
            self.dirty = true;
            let cursor_after = (self.cursor_row, self.cursor_col);
            if removed == '\n' {
                self.flush_pending();
                self.push_undo(UndoEntry {
                    op: EditOp::Remove {
                        char_idx: idx - 1,
                        text: "\n".to_string(),
                    },
                    cursor_before,
                    cursor_after,
                });
            } else {
                self.record_edit(
                    EditKind::DeleteBackward,
                    EditOp::Remove {
                        char_idx: idx - 1,
                        text: removed.to_string(),
                    },
                    cursor_before,
                    cursor_after,
                );
            }
        } else if self.cursor_row > 0 {
            // Join with previous line: remove the \n at end of previous line
            let idx = self.char_idx(self.cursor_row, 0);
            let new_col = self.line_len(self.cursor_row - 1);
            self.rope.remove(idx - 1..idx);
            self.cursor_row -= 1;
            self.cursor_col = new_col;
            self.dirty = true;
            let cursor_after = (self.cursor_row, self.cursor_col);
            self.flush_pending();
            self.push_undo(UndoEntry {
                op: EditOp::Remove {
                    char_idx: idx - 1,
                    text: "\n".to_string(),
                },
                cursor_before,
                cursor_after,
            });
        }
    }

    pub fn delete_char_forward(&mut self) {
        let cursor_before = (self.cursor_row, self.cursor_col);
        let len = self.current_line_len();
        if self.cursor_col < len {
            let idx = self.char_idx(self.cursor_row, self.cursor_col);
            let removed = self.rope.char(idx);
            self.rope.remove(idx..idx + 1);
            self.dirty = true;
            let cursor_after = (self.cursor_row, self.cursor_col);
            self.record_edit(
                EditKind::DeleteForward,
                EditOp::Remove {
                    char_idx: idx,
                    text: removed.to_string(),
                },
                cursor_before,
                cursor_after,
            );
        } else if self.cursor_row + 1 < self.rope.len_lines() {
            // Join with next line: remove the \n at current position
            let idx = self.char_idx(self.cursor_row, self.cursor_col);
            self.rope.remove(idx..idx + 1);
            self.dirty = true;
            let cursor_after = (self.cursor_row, self.cursor_col);
            self.flush_pending();
            self.push_undo(UndoEntry {
                op: EditOp::Remove {
                    char_idx: idx,
                    text: "\n".to_string(),
                },
                cursor_before,
                cursor_after,
            });
        }
    }

    pub fn kill_line(&mut self) {
        let cursor_before = (self.cursor_row, self.cursor_col);
        let col = self.cursor_col;
        let len = self.current_line_len();
        if col < len {
            // Kill from cursor to end of line (not including newline)
            let start = self.char_idx(self.cursor_row, col);
            let end = self.char_idx(self.cursor_row, len);
            let text: String = self.rope.slice(start..end).to_string();
            self.rope.remove(start..end);
            self.dirty = true;
            let cursor_after = (self.cursor_row, self.cursor_col);
            self.flush_pending();
            self.push_undo(UndoEntry {
                op: EditOp::Remove {
                    char_idx: start,
                    text,
                },
                cursor_before,
                cursor_after,
            });
        } else if self.cursor_row + 1 < self.rope.len_lines() {
            // Kill the newline, joining with next line
            let idx = self.char_idx(self.cursor_row, col);
            self.rope.remove(idx..idx + 1);
            self.dirty = true;
            let cursor_after = (self.cursor_row, self.cursor_col);
            self.flush_pending();
            self.push_undo(UndoEntry {
                op: EditOp::Remove {
                    char_idx: idx,
                    text: "\n".to_string(),
                },
                cursor_before,
                cursor_after,
            });
        }
    }

    // --- Undo system ---

    pub fn undo(&mut self) {
        self.flush_pending();

        if self.undo_cursor.is_none() {
            self.undo_cursor = Some(self.undo_history.len());
        }

        let pos = self.undo_cursor.unwrap();
        if pos == 0 {
            return;
        }

        let entry = self.undo_history[pos - 1].clone();
        let inverse = self.invert_entry(&entry);

        // Apply the inverse operation
        self.apply_op(&inverse.op);
        self.cursor_row = inverse.cursor_after.0;
        self.cursor_col = inverse.cursor_after.1;
        self.dirty = true;

        // Push the inverse to history (for redo-via-undo)
        self.undo_history.push(inverse);
        self.undo_cursor = Some(pos - 1);
    }

    fn invert_entry(&self, entry: &UndoEntry) -> UndoEntry {
        let inv_op = match &entry.op {
            EditOp::Insert { char_idx, text } => EditOp::Remove {
                char_idx: *char_idx,
                text: text.clone(),
            },
            EditOp::Remove { char_idx, text } => EditOp::Insert {
                char_idx: *char_idx,
                text: text.clone(),
            },
        };
        UndoEntry {
            op: inv_op,
            cursor_before: entry.cursor_after,
            cursor_after: entry.cursor_before,
        }
    }

    fn apply_op(&mut self, op: &EditOp) {
        match op {
            EditOp::Insert { char_idx, text } => {
                self.rope.insert(*char_idx, text);
            }
            EditOp::Remove { char_idx, text } => {
                let end = *char_idx + text.chars().count();
                self.rope.remove(*char_idx..end);
            }
        }
    }

    // --- Undo grouping ---

    fn record_edit(
        &mut self,
        kind: EditKind,
        op: EditOp,
        cursor_before: (usize, usize),
        cursor_after: (usize, usize),
    ) {
        let now = Instant::now();

        if let Some(ref mut pg) = self.pending_group {
            let elapsed = now.duration_since(pg.last_time).as_millis();
            if pg.kind == kind && elapsed < GROUP_TIMEOUT_MS {
                // Merge into existing group
                match (&mut pg.op, &op) {
                    (
                        EditOp::Insert { text: acc, .. },
                        EditOp::Insert { text: new, .. },
                    ) => {
                        acc.push_str(new);
                    }
                    (
                        EditOp::Remove {
                            char_idx: acc_idx,
                            text: acc,
                        },
                        EditOp::Remove {
                            char_idx: new_idx,
                            text: new,
                        },
                    ) => {
                        if kind == EditKind::DeleteBackward {
                            // Backspace: new char goes before existing text
                            acc.insert_str(0, new);
                            *acc_idx = *new_idx;
                        } else {
                            // Delete forward: new char appends
                            acc.push_str(new);
                        }
                    }
                    _ => {
                        // Kind mismatch within group — shouldn't happen, flush and start new
                        self.flush_pending_inner();
                        self.pending_group = Some(PendingGroup {
                            kind,
                            op,
                            cursor_before,
                            cursor_after,
                            last_time: now,
                        });
                        return;
                    }
                }
                pg.cursor_after = cursor_after;
                pg.last_time = now;
                return;
            }
        }

        // Flush any existing group and start a new one
        self.flush_pending();
        self.pending_group = Some(PendingGroup {
            kind,
            op,
            cursor_before,
            cursor_after,
            last_time: now,
        });
    }

    fn flush_pending(&mut self) {
        self.flush_pending_inner();
    }

    fn flush_pending_inner(&mut self) {
        if let Some(pg) = self.pending_group.take() {
            self.undo_history.push(UndoEntry {
                op: pg.op,
                cursor_before: pg.cursor_before,
                cursor_after: pg.cursor_after,
            });
            self.undo_cursor = None;
        }
    }

    fn push_undo(&mut self, entry: UndoEntry) {
        self.undo_history.push(entry);
        self.undo_cursor = None;
    }

    fn break_undo_chain(&mut self) {
        self.flush_pending();
        self.undo_cursor = None;
    }

    // --- Helpers ---

    fn current_line_len(&self) -> usize {
        self.line_len(self.cursor_row)
    }

    fn clamp_cursor_col(&mut self) {
        let len = self.current_line_len();
        if self.cursor_col > len {
            self.cursor_col = len;
        }
    }
}
