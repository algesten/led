use led_core::TextDoc;
use led_core::lsp_types::EditorTextEdit;
use ropey::Rope;

use crate::syntax::IndentDelta;
use crate::wrap::{compute_chunks, display_col_to_char_idx, expand_tabs, find_sub_line};
use crate::{Buffer, EditKind, EditOp, UndoEntry};

impl Buffer {
    // --- Cursor movement ---

    pub fn move_up(&mut self, doc: &TextDoc) {
        self.break_undo_chain();
        let tw = self.text_width;
        if tw == 0 {
            if self.cursor_row > 0 {
                self.cursor_row -= 1;
                self.clamp_cursor_col(doc);
            }
            return;
        }

        let (display, char_map) = expand_tabs(&doc.line(self.cursor_row));
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
            self.clamp_cursor_col(doc);
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            let (prev_display, prev_cm) = expand_tabs(&doc.line(self.cursor_row));
            let prev_chunks = compute_chunks(prev_display.len(), tw);
            let (cs, ce) = *prev_chunks.last().unwrap();
            let target_dcol = cs + visual_col.min(ce - cs);
            self.cursor_col = display_col_to_char_idx(&prev_cm, target_dcol);
            self.clamp_cursor_col(doc);
        }
    }

    pub fn move_down(&mut self, doc: &TextDoc) {
        self.break_undo_chain();
        let tw = self.text_width;
        if tw == 0 {
            if self.cursor_row + 1 < doc.line_count() {
                self.cursor_row += 1;
                self.clamp_cursor_col(doc);
            }
            return;
        }

        let (display, char_map) = expand_tabs(&doc.line(self.cursor_row));
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
            self.clamp_cursor_col(doc);
        } else if self.cursor_row + 1 < doc.line_count() {
            self.cursor_row += 1;
            let (next_display, next_cm) = expand_tabs(&doc.line(self.cursor_row));
            let next_chunks = compute_chunks(next_display.len(), tw);
            let (cs, ce) = next_chunks[0];
            let target_dcol = cs + visual_col.min(ce - cs);
            self.cursor_col = display_col_to_char_idx(&next_cm, target_dcol);
            self.clamp_cursor_col(doc);
        }
    }

    pub fn move_left(&mut self, doc: &TextDoc) {
        self.break_undo_chain();
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = doc.line_len(self.cursor_row);
        }
    }

    pub fn move_right(&mut self, doc: &TextDoc) {
        self.break_undo_chain();
        let len = doc.line_len(self.cursor_row);
        if self.cursor_col < len {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < doc.line_count() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    pub fn move_to_line_start(&mut self) {
        self.break_undo_chain();
        self.cursor_col = 0;
    }

    pub fn move_to_line_end(&mut self, doc: &TextDoc) {
        self.break_undo_chain();
        self.cursor_col = doc.line_len(self.cursor_row);
    }

    pub fn page_up(&mut self, doc: &TextDoc, page_size: usize) {
        self.break_undo_chain();
        self.cursor_row = self.cursor_row.saturating_sub(page_size);
        self.clamp_cursor_col(doc);
    }

    pub fn page_down(&mut self, doc: &TextDoc, page_size: usize) {
        self.break_undo_chain();
        self.cursor_row = (self.cursor_row + page_size).min(doc.line_count().saturating_sub(1));
        self.clamp_cursor_col(doc);
    }

    pub fn move_to_file_start(&mut self) {
        self.break_undo_chain();
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    pub fn move_to_file_end(&mut self, doc: &TextDoc) {
        self.break_undo_chain();
        self.cursor_row = doc.line_count().saturating_sub(1);
        self.cursor_col = doc.line_len(self.cursor_row);
    }

    // --- Text editing ---

    pub fn insert_char(&mut self, doc: &mut TextDoc, ch: char) {
        let cursor_before = (self.cursor_row, self.cursor_col);
        let idx = doc.char_idx(self.cursor_row, self.cursor_col);
        let se = self.syntax_edit_insert(&*doc, idx, &ch.to_string());
        doc.insert_char(idx, ch);
        self.apply_syntax_edit(&*doc, se);
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

    pub fn insert_newline(&mut self, doc: &mut TextDoc) {
        let cursor_before = (self.cursor_row, self.cursor_col);

        // Snapshot the tree before the edit for two-pass indent
        let old_tree = self.syntax.as_ref().map(|s| s.clone_tree());

        // Insert newline
        let nl_idx = doc.char_idx(self.cursor_row, self.cursor_col);
        let se = self.syntax_edit_insert(&*doc, nl_idx, "\n");
        doc.insert_char(nl_idx, '\n');
        self.apply_syntax_edit(&*doc, se);
        self.cursor_row += 1;
        self.cursor_col = 0;
        self.dirty = true;

        // Compute and insert auto-indent
        let indent_text = self.compute_auto_indent(doc, self.cursor_row, old_tree.as_ref());
        if !indent_text.is_empty() {
            let indent_idx = doc.char_idx(self.cursor_row, self.cursor_col);
            let se = self.syntax_edit_insert(&*doc, indent_idx, &indent_text);
            doc.insert(indent_idx, &indent_text);
            self.apply_syntax_edit(&*doc, se);
            self.cursor_col = indent_text.chars().count();
        }

        // Single undo entry for the whole operation
        let full_text = format!("\n{indent_text}");
        let cursor_after = (self.cursor_row, self.cursor_col);
        self.flush_pending();
        self.push_undo(UndoEntry {
            op: EditOp::Insert {
                char_idx: nl_idx,
                text: full_text,
            },
            cursor_before,
            cursor_after,
            direction: 1,
        });
    }

    /// Compute auto-indent for a line using two-pass tree-sitter analysis with regex fallback.
    fn compute_auto_indent(
        &self,
        doc: &TextDoc,
        line: usize,
        old_tree: Option<&tree_sitter::Tree>,
    ) -> String {
        let rope = doc.rope();

        // Try tree-sitter based indent
        if let Some(ref syntax) = self.syntax {
            // Pass 1: compute suggestion using old tree (before newline)
            let old_suggestion =
                old_tree.and_then(|tree| syntax.suggest_indent_with_tree(rope, tree, line));

            // Pass 2: compute suggestion using current tree (after newline)
            let new_suggestion = syntax.suggest_indent(rope, line);

            // Resolve: use new suggestion if it differs from old and passes error filter
            let suggestion = match (old_suggestion, new_suggestion) {
                (Some(old), Some(new)) => {
                    if old.delta != new.delta && (!new.within_error || old.within_error) {
                        Some(new)
                    } else {
                        Some(old)
                    }
                }
                (None, Some(new)) => Some(new),
                (Some(old), None) => Some(old),
                (None, None) => None,
            };

            if let Some(suggestion) = suggestion {
                // If within error and we have regex patterns, try regex fallback
                if suggestion.within_error {
                    if let Some(indent) = self.regex_indent(doc, line) {
                        return indent;
                    }
                }

                let basis_indent = get_line_indent(rope, suggestion.basis_row);
                return apply_indent_delta(&basis_indent, suggestion.delta);
            }
        }

        // Fallback: regex only
        if let Some(indent) = self.regex_indent(doc, line) {
            return indent;
        }

        // Last resort: copy previous line's indentation
        if let Some(basis) = find_prev_nonempty_line(rope, line) {
            return get_line_indent(rope, basis);
        }

        String::new()
    }

    /// Regex-based indent fallback for when tree is in error state.
    fn regex_indent(&self, doc: &TextDoc, line: usize) -> Option<String> {
        let syntax = self.syntax.as_ref()?;
        let rope = doc.rope();

        let basis = find_prev_nonempty_line(rope, line)?;
        let basis_text: String = rope.line(basis).chars().collect();
        let basis_indent = get_line_indent(rope, basis);

        // Check if basis line matches increase_indent_pattern
        if let Some(ref re) = syntax.increase_indent_pattern {
            if re.is_match(&basis_text) {
                return Some(apply_indent_delta(&basis_indent, IndentDelta::Greater));
            }
        }

        // Check if current line matches decrease_indent_pattern
        let current_text: String = rope.line(line).chars().collect();
        if let Some(ref re) = syntax.decrease_indent_pattern {
            if re.is_match(&current_text) {
                return Some(apply_indent_delta(&basis_indent, IndentDelta::Less));
            }
        }

        None
    }

    pub fn delete_char_backward(&mut self, doc: &mut TextDoc) {
        let cursor_before = (self.cursor_row, self.cursor_col);
        if self.cursor_col > 0 {
            let idx = doc.char_idx(self.cursor_row, self.cursor_col);
            let removed = doc.char(idx - 1);
            let se = self.syntax_edit_remove(&*doc, idx - 1, idx);
            doc.remove(idx - 1, idx);
            self.apply_syntax_edit(&*doc, se);
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
            let idx = doc.char_idx(self.cursor_row, 0);
            let new_col = doc.line_len(self.cursor_row - 1);
            let se = self.syntax_edit_remove(&*doc, idx - 1, idx);
            doc.remove(idx - 1, idx);
            self.apply_syntax_edit(&*doc, se);
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

    pub fn delete_char_forward(&mut self, doc: &mut TextDoc) {
        let cursor_before = (self.cursor_row, self.cursor_col);
        let len = doc.line_len(self.cursor_row);
        if self.cursor_col < len {
            let idx = doc.char_idx(self.cursor_row, self.cursor_col);
            let removed = doc.char(idx);
            let se = self.syntax_edit_remove(&*doc, idx, idx + 1);
            doc.remove(idx, idx + 1);
            self.apply_syntax_edit(&*doc, se);
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
        } else if self.cursor_row + 1 < doc.line_count() {
            let idx = doc.char_idx(self.cursor_row, self.cursor_col);
            let se = self.syntax_edit_remove(&*doc, idx, idx + 1);
            doc.remove(idx, idx + 1);
            self.apply_syntax_edit(&*doc, se);
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

    pub fn kill_line(&mut self, doc: &mut TextDoc) -> Option<String> {
        let cursor_before = (self.cursor_row, self.cursor_col);
        let col = self.cursor_col;
        let len = doc.line_len(self.cursor_row);
        if col < len {
            let start = doc.char_idx(self.cursor_row, col);
            let end = doc.char_idx(self.cursor_row, len);
            let text: String = doc.slice(start, end).to_string();
            let se = self.syntax_edit_remove(&*doc, start, end);
            doc.remove(start, end);
            self.apply_syntax_edit(&*doc, se);
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
        } else if self.cursor_row + 1 < doc.line_count() {
            let idx = doc.char_idx(self.cursor_row, col);
            let se = self.syntax_edit_remove(&*doc, idx, idx + 1);
            doc.remove(idx, idx + 1);
            self.apply_syntax_edit(&*doc, se);
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
    pub fn highlight_match(&mut self, doc: &TextDoc, row: usize, col: usize, len: usize) {
        let r = row.min(doc.line_count().saturating_sub(1));
        let line_len = doc.line_len(r);
        let c = col.min(line_len);
        self.cursor_row = r;
        if len > 0 {
            self.mark = Some((r, c));
            self.cursor_col = (c + len).min(line_len);
        } else {
            self.cursor_col = c;
        }
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

    pub(crate) fn selected_text(&self, doc: &TextDoc) -> Option<String> {
        let ((sr, sc), (er, ec)) = self.selection_range()?;
        let start = doc.char_idx(sr, sc);
        let end = doc.char_idx(er, ec);
        if start == end {
            return None;
        }
        Some(doc.slice(start, end).to_string())
    }

    pub(crate) fn kill_region(&mut self, doc: &mut TextDoc) -> Option<String> {
        let ((sr, sc), (er, ec)) = self.selection_range()?;
        let start_idx = doc.char_idx(sr, sc);
        let end_idx = doc.char_idx(er, ec);
        if start_idx == end_idx {
            self.clear_mark();
            return None;
        }
        let text: String = doc.slice(start_idx, end_idx).to_string();
        let cursor_before = (self.cursor_row, self.cursor_col);
        let se = self.syntax_edit_remove(&*doc, start_idx, end_idx);
        doc.remove(start_idx, end_idx);
        self.apply_syntax_edit(&*doc, se);
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

    pub(crate) fn yank_text(&mut self, doc: &mut TextDoc, text: &str) {
        if text.is_empty() {
            return;
        }
        let cursor_before = (self.cursor_row, self.cursor_col);
        let idx = doc.char_idx(self.cursor_row, self.cursor_col);
        let se = self.syntax_edit_insert(&*doc, idx, text);
        doc.insert(idx, text);
        self.apply_syntax_edit(&*doc, se);
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

    // --- LSP support ---

    /// Apply a set of text edits from LSP (formatting, rename, code actions).
    /// Edits are applied in reverse document order to preserve positions.
    pub fn apply_text_edits(&mut self, doc: &mut TextDoc, mut edits: Vec<EditorTextEdit>) {
        if edits.is_empty() {
            return;
        }
        self.flush_pending();
        let cursor_before = (self.cursor_row, self.cursor_col);

        // Compute cursor char offset and per-edit ranges against the original doc
        let cursor_offset = doc.char_idx(self.cursor_row, self.cursor_col);
        // Collect original char ranges for cursor adjustment (document order)
        let mut edit_ranges: Vec<(usize, usize, usize)> = edits
            .iter()
            .map(|edit| {
                let sr = edit.range.start.row.min(doc.line_count().saturating_sub(1));
                let sc = edit.range.start.col.min(doc.line_len(sr));
                let er = edit.range.end.row.min(doc.line_count().saturating_sub(1));
                let ec = edit.range.end.col.min(doc.line_len(er));
                let start = doc.char_idx(sr, sc);
                let end = doc.char_idx(er, ec);
                let new_len = edit.new_text.chars().count();
                (start, end, new_len)
            })
            .collect();
        edit_ranges.sort_by_key(|&(s, _, _)| s);

        // Sort edits in reverse document order (later positions first)
        edits.sort_by(|a, b| {
            b.range
                .start
                .row
                .cmp(&a.range.start.row)
                .then(b.range.start.col.cmp(&a.range.start.col))
        });

        // Snapshot the full text for a compound undo
        let before_text: String = doc.to_string();

        for edit in &edits {
            let start_row = edit.range.start.row.min(doc.line_count().saturating_sub(1));
            let start_col = edit.range.start.col.min(doc.line_len(start_row));
            let end_row = edit.range.end.row.min(doc.line_count().saturating_sub(1));
            let end_col = edit.range.end.col.min(doc.line_len(end_row));

            let start_idx = doc.char_idx(start_row, start_col);
            let end_idx = doc.char_idx(end_row, end_col);

            if start_idx < end_idx {
                let se = self.syntax_edit_remove(&*doc, start_idx, end_idx);
                doc.remove(start_idx, end_idx);
                self.apply_syntax_edit(&*doc, se);
            }
            if !edit.new_text.is_empty() {
                let se = self.syntax_edit_insert(&*doc, start_idx, &edit.new_text);
                doc.insert(start_idx, &edit.new_text);
                self.apply_syntax_edit(&*doc, se);
            }
        }

        // Adjust cursor through edit deltas (edits in document order, original coords)
        let mut new_cursor = cursor_offset;
        let mut delta: isize = 0;
        for &(start, end, new_len) in &edit_ranges {
            let old_len = end - start;
            if cursor_offset < start {
                break;
            } else if cursor_offset >= end {
                delta += new_len as isize - old_len as isize;
            } else {
                // Cursor inside the replaced range — snap to end of new text
                new_cursor = start + new_len;
                delta = 0; // already accounted for
                break;
            }
        }
        new_cursor = (new_cursor as isize + delta).max(0) as usize;
        let total_chars = doc.len_chars();
        if new_cursor > total_chars {
            new_cursor = total_chars.saturating_sub(1);
        }
        self.cursor_row = doc.char_to_line(new_cursor);
        let line_start = doc.line_to_char(self.cursor_row);
        self.cursor_col = new_cursor - line_start;

        self.dirty = true;
        self.clamp_cursor_col(&*doc);
        let cursor_after = (self.cursor_row, self.cursor_col);

        // Record compound undo: remove everything, insert before_text
        self.push_undo(UndoEntry {
            op: EditOp::Remove {
                char_idx: 0,
                text: doc.to_string(),
            },
            cursor_before,
            cursor_after,
            direction: 0, // special: won't be inverted normally
        });
        // Actually store a restorable undo: swap entire content
        // Simpler approach: just record the before_text as an Insert undo
        self.undo_history.pop(); // remove the placeholder
        self.distance_from_save -= 1;
        self.push_undo(UndoEntry {
            op: EditOp::Insert {
                char_idx: 0,
                text: before_text,
            },
            cursor_before: cursor_after,
            cursor_after: cursor_before,
            direction: 1,
        });
    }

    /// Get the word under the cursor (alphanumeric + underscore).
    pub fn word_at_cursor(&self, doc: &TextDoc) -> Option<String> {
        if self.cursor_row >= doc.line_count() {
            return None;
        }
        let line = doc.line(self.cursor_row);
        let chars: Vec<char> = line.chars().collect();
        let col = self.cursor_col.min(chars.len());

        if col >= chars.len() || !is_word_char(chars[col]) {
            // Try one position back
            if col == 0 || !is_word_char(chars[col - 1]) {
                return None;
            }
        }

        let mut start = col;
        while start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        }
        let mut end = col;
        while end < chars.len() && is_word_char(chars[end]) {
            end += 1;
        }

        if start == end {
            return None;
        }
        Some(chars[start..end].iter().collect())
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Get the leading whitespace of a line as a string.
fn get_line_indent(rope: &Rope, line: usize) -> String {
    let line_text = rope.line(line);
    let mut indent = String::new();
    for ch in line_text.chars() {
        if ch == ' ' || ch == '\t' {
            indent.push(ch);
        } else {
            break;
        }
    }
    indent
}

/// Apply an indent delta to a basis indentation string.
fn apply_indent_delta(basis_indent: &str, delta: IndentDelta) -> String {
    match delta {
        IndentDelta::Greater => {
            let mut s = basis_indent.to_string();
            s.push('\t');
            s
        }
        IndentDelta::Less => {
            // Remove one indent level (one tab or N spaces)
            let s = basis_indent.to_string();
            if s.ends_with('\t') {
                s[..s.len() - 1].to_string()
            } else {
                // Remove up to 4 trailing spaces
                let trimmed = s.trim_end_matches(' ');
                let removed = s.len() - trimmed.len();
                if removed > 0 {
                    let remove_count = removed.min(4);
                    s[..s.len() - remove_count].to_string()
                } else {
                    s
                }
            }
        }
        IndentDelta::Equal => basis_indent.to_string(),
    }
}

/// Find the previous non-empty line before `line`.
fn find_prev_nonempty_line(rope: &Rope, line: usize) -> Option<usize> {
    for row in (0..line).rev() {
        let line_text = rope.line(row);
        if line_text.chars().any(|c| !c.is_whitespace()) {
            return Some(row);
        }
    }
    if line > 0 { Some(0) } else { None }
}
