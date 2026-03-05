use crate::wrap::{compute_chunks, display_col_to_char_idx, expand_tabs, find_sub_line};
use crate::{Buffer, EditKind, EditOp, UndoEntry};

impl Buffer {
    // --- Cursor movement ---

    pub fn move_up(&mut self) {
        self.break_undo_chain();
        let tw = self.text_width;
        if tw == 0 {
            if self.cursor_row > 0 {
                self.cursor_row -= 1;
                self.clamp_cursor_col();
            }
            return;
        }

        let (display, char_map) = expand_tabs(&self.line(self.cursor_row));
        let cursor_dcol = char_map
            .get(self.cursor_col)
            .copied()
            .unwrap_or_else(|| char_map.last().copied().unwrap_or(0));
        let chunks = compute_chunks(display.len(), tw);
        let sub = find_sub_line(&chunks, cursor_dcol);
        let visual_col = cursor_dcol - chunks[sub].0;

        if sub > 0 {
            let (cs, ce) = chunks[sub - 1];
            let target_dcol = cs + visual_col.min(ce - cs);
            self.cursor_col = display_col_to_char_idx(&char_map, target_dcol);
            self.clamp_cursor_col();
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            let (prev_display, prev_cm) = expand_tabs(&self.line(self.cursor_row));
            let prev_chunks = compute_chunks(prev_display.len(), tw);
            let (cs, ce) = *prev_chunks.last().unwrap();
            let target_dcol = cs + visual_col.min(ce - cs);
            self.cursor_col = display_col_to_char_idx(&prev_cm, target_dcol);
            self.clamp_cursor_col();
        }
    }

    pub fn move_down(&mut self) {
        self.break_undo_chain();
        let tw = self.text_width;
        if tw == 0 {
            if self.cursor_row + 1 < self.rope.len_lines() {
                self.cursor_row += 1;
                self.clamp_cursor_col();
            }
            return;
        }

        let (display, char_map) = expand_tabs(&self.line(self.cursor_row));
        let cursor_dcol = char_map
            .get(self.cursor_col)
            .copied()
            .unwrap_or_else(|| char_map.last().copied().unwrap_or(0));
        let chunks = compute_chunks(display.len(), tw);
        let sub = find_sub_line(&chunks, cursor_dcol);
        let visual_col = cursor_dcol - chunks[sub].0;

        if sub + 1 < chunks.len() {
            let (cs, ce) = chunks[sub + 1];
            let target_dcol = cs + visual_col.min(ce - cs);
            self.cursor_col = display_col_to_char_idx(&char_map, target_dcol);
            self.clamp_cursor_col();
        } else if self.cursor_row + 1 < self.rope.len_lines() {
            self.cursor_row += 1;
            let (next_display, next_cm) = expand_tabs(&self.line(self.cursor_row));
            let next_chunks = compute_chunks(next_display.len(), tw);
            let (cs, ce) = next_chunks[0];
            let target_dcol = cs + visual_col.min(ce - cs);
            self.cursor_col = display_col_to_char_idx(&next_cm, target_dcol);
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
        let se = self.syntax_edit_insert(idx, &ch.to_string());
        self.rope.insert_char(idx, ch);
        self.apply_syntax_edit(se);
        if ch == '\n' {
            self.cursor_row += 1;
            self.cursor_col = 0;
        } else {
            self.cursor_col += 1;
        }
        self.dirty = true;
        let cursor_after = (self.cursor_row, self.cursor_col);

        if ch == '\n' {
            self.flush_pending();
            self.push_undo(UndoEntry {
                op: EditOp::Insert {
                    char_idx: idx,
                    text: "\n".to_string(),
                },
                cursor_before,
                cursor_after,
                direction: 1,
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
            let se = self.syntax_edit_remove(idx - 1, idx);
            self.rope.remove(idx - 1..idx);
            self.apply_syntax_edit(se);
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
                    direction: 1,
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
            let idx = self.char_idx(self.cursor_row, 0);
            let new_col = self.line_len(self.cursor_row - 1);
            let se = self.syntax_edit_remove(idx - 1, idx);
            self.rope.remove(idx - 1..idx);
            self.apply_syntax_edit(se);
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
                direction: 1,
            });
        }
    }

    pub fn delete_char_forward(&mut self) {
        let cursor_before = (self.cursor_row, self.cursor_col);
        let len = self.current_line_len();
        if self.cursor_col < len {
            let idx = self.char_idx(self.cursor_row, self.cursor_col);
            let removed = self.rope.char(idx);
            let se = self.syntax_edit_remove(idx, idx + 1);
            self.rope.remove(idx..idx + 1);
            self.apply_syntax_edit(se);
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
            let idx = self.char_idx(self.cursor_row, self.cursor_col);
            let se = self.syntax_edit_remove(idx, idx + 1);
            self.rope.remove(idx..idx + 1);
            self.apply_syntax_edit(se);
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
                direction: 1,
            });
        }
    }

    pub fn kill_line(&mut self) -> Option<String> {
        let cursor_before = (self.cursor_row, self.cursor_col);
        let col = self.cursor_col;
        let len = self.current_line_len();
        if col < len {
            let start = self.char_idx(self.cursor_row, col);
            let end = self.char_idx(self.cursor_row, len);
            let text: String = self.rope.slice(start..end).to_string();
            let se = self.syntax_edit_remove(start, end);
            self.rope.remove(start..end);
            self.apply_syntax_edit(se);
            self.dirty = true;
            let cursor_after = (self.cursor_row, self.cursor_col);
            self.flush_pending();
            self.push_undo(UndoEntry {
                op: EditOp::Remove {
                    char_idx: start,
                    text: text.clone(),
                },
                cursor_before,
                cursor_after,
                direction: 1,
            });
            Some(text)
        } else if self.cursor_row + 1 < self.rope.len_lines() {
            let idx = self.char_idx(self.cursor_row, col);
            let se = self.syntax_edit_remove(idx, idx + 1);
            self.rope.remove(idx..idx + 1);
            self.apply_syntax_edit(se);
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
                direction: 1,
            });
            Some("\n".to_string())
        } else {
            None
        }
    }

    // --- Mark / Selection ---

    pub(crate) fn set_mark(&mut self) {
        self.mark = Some((self.cursor_row, self.cursor_col));
    }

    pub(crate) fn clear_mark(&mut self) {
        self.mark = None;
    }

    /// Set a visible highlight from (row, col) spanning `len` chars.
    /// Sets mark at start, cursor at end so the selection system renders it.
    pub fn highlight_match(&mut self, row: usize, col: usize, len: usize) {
        let r = row.min(self.line_count().saturating_sub(1));
        let line_len = self.line_len(r);
        let c = col.min(line_len);
        self.mark = Some((r, c));
        self.cursor_row = r;
        self.cursor_col = (c + len).min(line_len);
        self.preview_highlight = true;
    }

    pub(crate) fn selection_range(&self) -> Option<((usize, usize), (usize, usize))> {
        let mark = self.mark?;
        let cursor = (self.cursor_row, self.cursor_col);
        if mark <= cursor {
            Some((mark, cursor))
        } else {
            Some((cursor, mark))
        }
    }

    pub(crate) fn selected_text(&self) -> Option<String> {
        let ((sr, sc), (er, ec)) = self.selection_range()?;
        let start = self.char_idx(sr, sc);
        let end = self.char_idx(er, ec);
        if start == end {
            return None;
        }
        Some(self.rope.slice(start..end).to_string())
    }

    pub(crate) fn kill_region(&mut self) -> Option<String> {
        let ((sr, sc), (er, ec)) = self.selection_range()?;
        let start_idx = self.char_idx(sr, sc);
        let end_idx = self.char_idx(er, ec);
        if start_idx == end_idx {
            self.clear_mark();
            return None;
        }
        let text: String = self.rope.slice(start_idx..end_idx).to_string();
        let cursor_before = (self.cursor_row, self.cursor_col);
        let se = self.syntax_edit_remove(start_idx, end_idx);
        self.rope.remove(start_idx..end_idx);
        self.apply_syntax_edit(se);
        self.cursor_row = sr;
        self.cursor_col = sc;
        self.dirty = true;
        let cursor_after = (self.cursor_row, self.cursor_col);
        self.flush_pending();
        self.push_undo(UndoEntry {
            op: EditOp::Remove {
                char_idx: start_idx,
                text: text.clone(),
            },
            cursor_before,
            cursor_after,
            direction: 1,
        });
        self.clear_mark();
        Some(text)
    }

    pub(crate) fn yank_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let cursor_before = (self.cursor_row, self.cursor_col);
        let idx = self.char_idx(self.cursor_row, self.cursor_col);
        let se = self.syntax_edit_insert(idx, text);
        self.rope.insert(idx, text);
        self.apply_syntax_edit(se);
        // Advance cursor past inserted text
        let inserted_chars: usize = text.chars().count();
        let newlines: usize = text.chars().filter(|&c| c == '\n').count();
        if newlines > 0 {
            self.cursor_row += newlines;
            let last_line_len = text.rsplit('\n').next().unwrap_or("").chars().count();
            self.cursor_col = last_line_len;
        } else {
            self.cursor_col += inserted_chars;
        }
        self.dirty = true;
        let cursor_after = (self.cursor_row, self.cursor_col);
        self.flush_pending();
        self.push_undo(UndoEntry {
            op: EditOp::Insert {
                char_idx: idx,
                text: text.to_string(),
            },
            cursor_before,
            cursor_after,
            direction: 1,
        });
    }
}
