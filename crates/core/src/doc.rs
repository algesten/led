use std::hash::{DefaultHasher, Hasher};
use std::io;
use std::sync::Arc;

use ropey::Rope;
use serde::{Deserialize, Serialize};

// ── Undo types ──

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditOp {
    pub offset: usize,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UndoEntry {
    pub op: EditOp,
    pub cursor_before: usize,
    pub cursor_after: usize,
    /// 1 = forward edit, 0 = continuation (same group), -1 = undo inverse.
    pub direction: i32,
}

/// Accumulates rapid edits before they are flushed to the linear history.
#[derive(Clone, Debug)]
struct PendingEdit {
    cursor_before: usize,
    ops: Vec<EditOp>,
}

/// Emacs-style linear undo history.
///
/// All edits (forward and inverse) are appended to a single `entries` vec.
/// Undo appends inverse entries (d = -1); redo re-applies originals.
/// Any non-undo edit breaks the undo chain, making previous inverses
/// undoable themselves — every buffer state is reachable by pressing undo.
#[derive(Clone, Debug)]
pub struct UndoHistory {
    entries: Vec<UndoEntry>,
    /// `None` = at the end of history (normal editing).
    /// `Some(n)` = partway through; entries before `n` have been undone.
    undo_cursor: Option<usize>,
    /// `entries.len()` when the current undo chain started.
    /// Redo is exhausted when `undo_cursor >= undo_chain_base`.
    undo_chain_base: usize,
    /// Net distance from the save point. 0 = clean.
    distance_from_save: i32,
    /// Rapid edits accumulate here until flushed by `flush_pending`.
    pending: Option<PendingEdit>,
}

impl Default for UndoHistory {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            undo_cursor: None,
            undo_chain_base: 0,
            distance_from_save: 0,
            pending: None,
        }
    }
}

fn compute_cursor_after(op: &EditOp) -> usize {
    if !op.new_text.is_empty() {
        op.offset + op.new_text.chars().count()
    } else {
        op.offset
    }
}

/// Apply an EditOp forward: remove old_text, insert new_text.
fn apply_op(rope: &mut Rope, op: &EditOp) {
    if !op.old_text.is_empty() {
        let end = op.offset + op.old_text.chars().count();
        rope.remove(op.offset..end);
    }
    if !op.new_text.is_empty() {
        rope.insert(op.offset, &op.new_text);
    }
}

impl UndoHistory {
    /// Pre-create a pending group with the given cursor position.
    /// If a pending group already exists, this is a no-op.
    /// Subsequent `push_op` calls will join this group.
    pub fn begin_group(&mut self, cursor_before: usize) {
        if self.undo_cursor.is_some() {
            self.flush_pending();
            self.undo_cursor = None;
        }
        self.pending.get_or_insert_with(|| PendingEdit {
            cursor_before,
            ops: Vec::new(),
        });
    }

    /// Append an op to pending (creates one if none exists).
    /// Any new edit breaks the undo chain.
    pub fn push_op(&mut self, op: EditOp, cursor_before: usize) {
        if self.undo_cursor.is_some() {
            self.flush_pending();
            self.undo_cursor = None;
        }
        let pending = self.pending.get_or_insert_with(|| PendingEdit {
            cursor_before,
            ops: Vec::new(),
        });
        pending.ops.push(op);
    }

    /// Convert pending edits into history entries.
    /// First entry gets d=1, subsequent entries get d=0 (continuation).
    pub fn flush_pending(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if pending.ops.is_empty() {
            return;
        }

        let mut cursor = pending.cursor_before;
        for (i, op) in pending.ops.into_iter().enumerate() {
            let direction = if i == 0 { 1 } else { 0 };
            let cursor_after = compute_cursor_after(&op);
            self.entries.push(UndoEntry {
                cursor_before: cursor,
                cursor_after,
                op,
                direction,
            });
            cursor = cursor_after;
        }
        self.distance_from_save += 1; // one group = one unit
    }

    /// Number of committed entries (excludes pending).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Slice of entries from `start` onwards (for incremental flush).
    pub fn entries_from(&self, start: usize) -> &[UndoEntry] {
        let start = start.min(self.entries.len());
        &self.entries[start..]
    }

    /// Whether there are pending ops not yet flushed.
    pub fn has_pending(&self) -> bool {
        self.pending.as_ref().is_some_and(|p| !p.ops.is_empty())
    }

    pub fn undo_cursor(&self) -> Option<usize> {
        self.undo_cursor
    }

    pub fn distance_from_save(&self) -> i32 {
        self.distance_from_save
    }

    pub fn pending_edit_ops(&self) -> Vec<EditOp> {
        self.pending
            .as_ref()
            .map(|p| p.ops.clone())
            .unwrap_or_default()
    }

    /// Append a remote entry directly to history with its original direction.
    pub fn push_remote_entry(&mut self, entry: UndoEntry) {
        self.undo_cursor = None;
        self.distance_from_save += entry.direction;
        self.entries.push(entry);
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

    // Byte-level access (needed by tree-sitter)
    fn len_bytes(&self) -> usize;
    fn line_to_byte(&self, line_idx: usize) -> usize;
    fn byte_to_line(&self, byte_idx: usize) -> usize;
    fn byte_to_char(&self, byte_idx: usize) -> usize;
    fn char_to_byte(&self, char_idx: usize) -> usize;
    /// Returns (chunk_str, chunk_byte_start) for the chunk containing `byte_offset`.
    fn chunk_at_byte(&self, byte_offset: usize) -> (&str, usize);

    // Identity & change detection
    fn version(&self) -> u64;
    fn dirty(&self) -> bool;
    fn content_hash(&self) -> u64;
    fn undo_history_len(&self) -> usize;
    fn undo_entries_from(&self, start: usize) -> Vec<UndoEntry>;
    fn undo_cursor(&self) -> Option<usize>;
    fn distance_from_save(&self) -> i32;
    /// Return the edit ops accumulated in the current (unflushed) pending group.
    fn pending_edit_ops(&self) -> Vec<EditOp>;

    // Edits — record undo ops into pending
    fn begin_undo_group(&self, cursor_before: usize) -> Arc<dyn Doc>;
    fn insert(&self, char_idx: usize, text: &str) -> Arc<dyn Doc>;
    fn remove(&self, start: usize, end: usize) -> Arc<dyn Doc>;

    // Undo
    fn close_undo_group(&self) -> Arc<dyn Doc>;
    fn undo(&self) -> Option<(Arc<dyn Doc>, usize)>;
    fn redo(&self) -> Option<(Arc<dyn Doc>, usize)>;

    // Remote entry application (preserves original direction for sync)
    fn apply_remote_entry(&self, entry: &UndoEntry) -> Arc<dyn Doc>;
    fn with_distance_from_save(&self, distance: i32) -> Arc<dyn Doc>;

    // Text extraction
    fn slice(&self, start: usize, end: usize) -> String;

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
    undo: UndoHistory,
}

impl TextDoc {
    pub fn from_reader(reader: impl io::Read) -> io::Result<Self> {
        let rope = Rope::from_reader(reader)?;
        Ok(TextDoc {
            rope,
            version: 0,
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
        let mut n = line.len_chars();
        while n > 0 && matches!(line.char(n - 1), '\n' | '\r') {
            n -= 1;
        }
        n
    }

    fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    fn line_to_byte(&self, line_idx: usize) -> usize {
        self.rope.line_to_byte(line_idx)
    }

    fn byte_to_line(&self, byte_idx: usize) -> usize {
        self.rope.byte_to_line(byte_idx)
    }

    fn byte_to_char(&self, byte_idx: usize) -> usize {
        self.rope.byte_to_char(byte_idx)
    }

    fn char_to_byte(&self, char_idx: usize) -> usize {
        self.rope.char_to_byte(char_idx)
    }

    fn chunk_at_byte(&self, byte_offset: usize) -> (&str, usize) {
        let (chunk, chunk_byte_start, _, _) = self.rope.chunk_at_byte(byte_offset);
        (chunk, chunk_byte_start)
    }

    fn version(&self) -> u64 {
        self.version
    }

    fn dirty(&self) -> bool {
        self.undo.has_pending() || self.undo.distance_from_save != 0
    }

    fn content_hash(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        for chunk in self.rope.chunks() {
            hasher.write(chunk.as_bytes());
        }
        hasher.finish()
    }

    fn undo_history_len(&self) -> usize {
        self.undo.len()
    }

    fn undo_entries_from(&self, start: usize) -> Vec<UndoEntry> {
        self.undo.entries_from(start).to_vec()
    }

    fn undo_cursor(&self) -> Option<usize> {
        self.undo.undo_cursor()
    }

    fn distance_from_save(&self) -> i32 {
        self.undo.distance_from_save()
    }

    fn pending_edit_ops(&self) -> Vec<EditOp> {
        self.undo.pending_edit_ops()
    }

    fn begin_undo_group(&self, cursor_before: usize) -> Arc<dyn Doc> {
        let mut undo = self.undo.clone();
        undo.begin_group(cursor_before);
        Arc::new(self.with_rope_and_undo(self.rope.clone(), undo))
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
        undo.flush_pending();
        Arc::new(TextDoc {
            rope: self.rope.clone(),
            version: self.version,
            undo,
        })
    }

    fn undo(&self) -> Option<(Arc<dyn Doc>, usize)> {
        let mut rope = self.rope.clone();
        let mut undo = self.undo.clone();

        undo.flush_pending();

        if undo.entries.is_empty() {
            return None;
        }

        // Start a new undo chain if not already in one
        if undo.undo_cursor.is_none() {
            undo.undo_cursor = Some(undo.entries.len());
            undo.undo_chain_base = undo.entries.len();
        }

        let cursor = undo.undo_cursor.unwrap();
        if cursor == 0 {
            return None;
        }

        let mut pos = cursor - 1;
        let mut restore_cursor;

        loop {
            let (inv_op, inv_cb, inv_ca, direction) = {
                let entry = &undo.entries[pos];
                let inv_op = EditOp {
                    offset: entry.op.offset,
                    old_text: entry.op.new_text.clone(),
                    new_text: entry.op.old_text.clone(),
                };
                (
                    inv_op,
                    entry.cursor_after,
                    entry.cursor_before,
                    entry.direction,
                )
            };

            apply_op(&mut rope, &inv_op);
            undo.distance_from_save -= direction;
            restore_cursor = inv_ca; // = original entry's cursor_before

            undo.entries.push(UndoEntry {
                op: inv_op,
                cursor_before: inv_cb,
                cursor_after: inv_ca,
                direction: -direction,
            });

            // Stop after processing the group start (d != 0)
            if direction != 0 {
                break;
            }
            if pos == 0 {
                break;
            }
            pos -= 1;
        }

        undo.undo_cursor = Some(pos);

        let doc = TextDoc {
            rope,
            version: self.version + 1,
            undo,
        };
        Some((Arc::new(doc), restore_cursor))
    }

    fn redo(&self) -> Option<(Arc<dyn Doc>, usize)> {
        let mut rope = self.rope.clone();
        let mut undo = self.undo.clone();

        let cursor = undo.undo_cursor?;
        if cursor >= undo.undo_chain_base {
            return None;
        }

        let mut pos = cursor;
        let mut last_cursor_after;

        loop {
            let (op_clone, cb, ca, direction) = {
                let entry = &undo.entries[pos];
                (
                    entry.op.clone(),
                    entry.cursor_before,
                    entry.cursor_after,
                    entry.direction,
                )
            };

            apply_op(&mut rope, &op_clone);
            undo.distance_from_save += direction;
            last_cursor_after = ca;

            undo.entries.push(UndoEntry {
                op: op_clone,
                cursor_before: cb,
                cursor_after: ca,
                direction,
            });

            pos += 1;

            // Stop at chain boundary or when next entry is not a continuation
            if pos >= undo.undo_chain_base {
                break;
            }
            if undo.entries[pos].direction != 0 {
                break;
            }
        }

        if pos >= undo.undo_chain_base {
            undo.undo_cursor = None;
        } else {
            undo.undo_cursor = Some(pos);
        }

        let doc = TextDoc {
            rope,
            version: self.version + 1,
            undo,
        };
        Some((Arc::new(doc), last_cursor_after))
    }

    fn apply_remote_entry(&self, entry: &UndoEntry) -> Arc<dyn Doc> {
        let mut rope = self.rope.clone();
        let mut undo = self.undo.clone();

        // Break any active undo chain
        undo.undo_cursor = None;

        apply_op(&mut rope, &entry.op);
        undo.distance_from_save += entry.direction;
        undo.entries.push(entry.clone());

        Arc::new(TextDoc {
            rope,
            version: self.version + 1,
            undo,
        })
    }

    fn with_distance_from_save(&self, distance: i32) -> Arc<dyn Doc> {
        let mut undo = self.undo.clone();
        undo.distance_from_save = distance;
        Arc::new(TextDoc {
            rope: self.rope.clone(),
            version: self.version,
            undo,
        })
    }

    fn slice(&self, start: usize, end: usize) -> String {
        self.rope.slice(start..end).to_string()
    }

    fn write_to(&self, writer: &mut dyn io::Write) -> io::Result<()> {
        for chunk in self.rope.chunks() {
            writer.write_all(chunk.as_bytes())?;
        }
        Ok(())
    }

    fn mark_saved(&self) -> Arc<dyn Doc> {
        let mut undo = self.undo.clone();
        undo.flush_pending();
        undo.distance_from_save = 0;
        Arc::new(TextDoc {
            rope: self.rope.clone(),
            version: self.version,
            undo,
        })
    }

    fn clone_box(&self) -> Box<dyn Doc> {
        Box::new(TextDoc {
            rope: self.rope.clone(),
            version: self.version,
            undo: self.undo.clone(),
        })
    }
}
