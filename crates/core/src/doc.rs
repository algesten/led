use std::io;
use std::sync::Arc;

use ropey::Rope;

// ── Undo types ──

#[derive(Clone, Debug)]
pub struct EditOp {
    pub offset: usize,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Clone, Debug)]
pub struct UndoGroup {
    pub ops: Vec<EditOp>,
    pub cursor_before: usize,
}

#[derive(Clone, Debug, Default)]
pub struct UndoHistory {
    undo_stack: Vec<UndoGroup>,
    redo_stack: Vec<UndoGroup>,
    open_group: Option<UndoGroup>,
}

impl UndoHistory {
    /// Append an op to the open group (creates one if none exists).
    pub fn push_op(&mut self, op: EditOp, cursor_before: usize) {
        let group = self.open_group.get_or_insert_with(|| UndoGroup {
            ops: Vec::new(),
            cursor_before,
        });
        group.ops.push(op);
    }

    /// Close the open group, moving it to the undo stack. Clears redo.
    pub fn close_group(&mut self) {
        if let Some(group) = self.open_group.take() {
            if !group.ops.is_empty() {
                self.undo_stack.push(group);
                self.redo_stack.clear();
            }
        }
    }

    /// Close any open group, then pop the top undo group.
    pub fn pop_undo(&mut self) -> Option<UndoGroup> {
        self.close_group();
        self.undo_stack.pop()
    }

    pub fn push_redo(&mut self, group: UndoGroup) {
        self.redo_stack.push(group);
    }

    pub fn pop_redo(&mut self) -> Option<UndoGroup> {
        self.redo_stack.pop()
    }
}

// ── Doc trait ──

pub trait Doc: Send + Sync {
    // Display
    fn line_count(&self) -> usize;
    fn line(&self, idx: usize) -> String;

    // Coordinate conversion
    fn line_to_char(&self, line_idx: usize) -> usize;
    fn char_to_line(&self, char_idx: usize) -> usize;
    fn line_len(&self, line_idx: usize) -> usize;

    // Identity & change detection
    fn version(&self) -> u64;
    fn dirty(&self) -> bool;

    // Edits — record undo ops into the open group
    fn insert(&self, char_idx: usize, text: &str) -> Arc<dyn Doc>;
    fn remove(&self, start: usize, end: usize) -> Arc<dyn Doc>;

    // Undo
    fn close_undo_group(&self) -> Arc<dyn Doc>;
    fn undo(&self) -> Option<(Arc<dyn Doc>, usize)>;
    fn redo(&self) -> Option<(Arc<dyn Doc>, usize)>;

    // Persistence
    fn write_to(&self, writer: &mut dyn io::Write) -> io::Result<()>;
    fn mark_saved(&self) -> Arc<dyn Doc>;

    // Clone support
    fn clone_box(&self) -> Box<dyn Doc>;
}

impl Clone for Box<dyn Doc> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

// ── TextDoc ──

pub struct TextDoc {
    rope: Rope,
    version: u64,
    saved_version: u64,
    undo: UndoHistory,
}

impl TextDoc {
    pub fn from_reader(reader: impl io::Read) -> io::Result<Self> {
        let rope = Rope::from_reader(reader)?;
        Ok(TextDoc {
            rope,
            version: 0,
            saved_version: 0,
            undo: UndoHistory::default(),
        })
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    fn with_rope_and_undo(&self, rope: Rope, undo: UndoHistory) -> Self {
        TextDoc {
            rope,
            version: self.version + 1,
            saved_version: self.saved_version,
            undo,
        }
    }
}

impl Doc for TextDoc {
    fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    fn line(&self, idx: usize) -> String {
        if idx >= self.rope.len_lines() {
            return String::new();
        }
        let line = self.rope.line(idx);
        let s = line.to_string();
        s.trim_end_matches(&['\n', '\r'][..]).to_string()
    }

    fn line_to_char(&self, line_idx: usize) -> usize {
        self.rope.line_to_char(line_idx)
    }

    fn char_to_line(&self, char_idx: usize) -> usize {
        self.rope.char_to_line(char_idx)
    }

    fn line_len(&self, line_idx: usize) -> usize {
        if line_idx >= self.rope.len_lines() {
            return 0;
        }
        let line = self.rope.line(line_idx);
        let s = line.to_string();
        s.trim_end_matches(&['\n', '\r'][..]).len()
    }

    fn version(&self) -> u64 {
        self.version
    }

    fn dirty(&self) -> bool {
        self.version != self.saved_version
    }

    fn insert(&self, char_idx: usize, text: &str) -> Arc<dyn Doc> {
        let mut rope = self.rope.clone();
        rope.insert(char_idx, text);
        let mut undo = self.undo.clone();
        undo.push_op(
            EditOp {
                offset: char_idx,
                old_text: String::new(),
                new_text: text.to_string(),
            },
            char_idx,
        );
        Arc::new(self.with_rope_and_undo(rope, undo))
    }

    fn remove(&self, start: usize, end: usize) -> Arc<dyn Doc> {
        let old_text: String = self.rope.slice(start..end).to_string();
        let mut rope = self.rope.clone();
        rope.remove(start..end);
        let mut undo = self.undo.clone();
        undo.push_op(
            EditOp {
                offset: start,
                old_text,
                new_text: String::new(),
            },
            start,
        );
        Arc::new(self.with_rope_and_undo(rope, undo))
    }

    fn close_undo_group(&self) -> Arc<dyn Doc> {
        let mut undo = self.undo.clone();
        undo.close_group();
        Arc::new(TextDoc {
            rope: self.rope.clone(),
            version: self.version,
            saved_version: self.saved_version,
            undo,
        })
    }

    fn undo(&self) -> Option<(Arc<dyn Doc>, usize)> {
        let mut undo = self.undo.clone();
        let group = undo.pop_undo()?;
        let cursor = group.cursor_before;

        // Apply ops in reverse to undo
        let mut rope = self.rope.clone();
        let mut inverse_ops = Vec::with_capacity(group.ops.len());
        for op in group.ops.iter().rev() {
            // Remove what was inserted
            if !op.new_text.is_empty() {
                let end = op.offset + op.new_text.chars().count();
                rope.remove(op.offset..end);
            }
            // Insert what was removed
            if !op.old_text.is_empty() {
                rope.insert(op.offset, &op.old_text);
            }
            // Inverse op for redo
            inverse_ops.push(EditOp {
                offset: op.offset,
                old_text: op.new_text.clone(),
                new_text: op.old_text.clone(),
            });
        }
        inverse_ops.reverse();

        undo.push_redo(UndoGroup {
            ops: inverse_ops,
            cursor_before: cursor,
        });

        let doc = TextDoc {
            rope,
            version: self.version + 1,
            saved_version: self.saved_version,
            undo,
        };
        Some((Arc::new(doc), cursor))
    }

    fn redo(&self) -> Option<(Arc<dyn Doc>, usize)> {
        let mut undo = self.undo.clone();
        let group = undo.pop_redo()?;

        // Apply ops forward to redo
        let mut rope = self.rope.clone();
        let mut cursor = 0usize;
        let mut inverse_ops = Vec::with_capacity(group.ops.len());
        for op in &group.ops {
            // Remove what was there (old_text)
            if !op.old_text.is_empty() {
                let end = op.offset + op.old_text.chars().count();
                rope.remove(op.offset..end);
            }
            // Insert the new text
            if !op.new_text.is_empty() {
                rope.insert(op.offset, &op.new_text);
                cursor = op.offset + op.new_text.chars().count();
            } else {
                cursor = op.offset;
            }
            // Inverse op for undo
            inverse_ops.push(EditOp {
                offset: op.offset,
                old_text: op.new_text.clone(),
                new_text: op.old_text.clone(),
            });
        }

        undo.undo_stack.push(UndoGroup {
            ops: inverse_ops,
            cursor_before: group.cursor_before,
        });

        let doc = TextDoc {
            rope,
            version: self.version + 1,
            saved_version: self.saved_version,
            undo,
        };
        Some((Arc::new(doc), cursor))
    }

    fn write_to(&self, writer: &mut dyn io::Write) -> io::Result<()> {
        for chunk in self.rope.chunks() {
            writer.write_all(chunk.as_bytes())?;
        }
        Ok(())
    }

    fn mark_saved(&self) -> Arc<dyn Doc> {
        Arc::new(TextDoc {
            rope: self.rope.clone(),
            version: self.version,
            saved_version: self.version,
            undo: self.undo.clone(),
        })
    }

    fn clone_box(&self) -> Box<dyn Doc> {
        Box::new(TextDoc {
            rope: self.rope.clone(),
            version: self.version,
            saved_version: self.saved_version,
            undo: self.undo.clone(),
        })
    }
}
