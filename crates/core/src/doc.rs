use std::hash::{DefaultHasher, Hasher};
use std::io;
use std::sync::Arc;

use ropey::Rope;
use serde::{Deserialize, Serialize};

use crate::{CharOffset, ContentHash, Row};

// ── Undo types ──

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditOp {
    pub offset: CharOffset,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UndoEntry {
    pub op: EditOp,
    pub cursor_before: CharOffset,
    pub cursor_after: CharOffset,
    /// 1 = forward edit, 0 = continuation (same group), -1 = undo inverse.
    pub direction: i32,
}

/// Accumulates rapid edits before they are flushed to the linear history.
#[derive(Clone, Debug)]
struct PendingEdit {
    cursor_before: CharOffset,
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

fn compute_cursor_after(op: &EditOp) -> CharOffset {
    if !op.new_text.is_empty() {
        CharOffset(*op.offset + op.new_text.chars().count())
    } else {
        op.offset
    }
}

/// Apply an EditOp forward: remove old_text, insert new_text.
pub fn apply_op_to_doc(doc: &Arc<dyn Doc>, op: &EditOp) -> Arc<dyn Doc> {
    let mut d = doc.clone();
    if !op.old_text.is_empty() {
        let end = CharOffset(*op.offset + op.old_text.chars().count());
        d = d.remove(op.offset, end);
    }
    if !op.new_text.is_empty() {
        d = d.insert(op.offset, &op.new_text);
    }
    d
}

impl UndoHistory {
    /// Pre-create a pending group with the given cursor position.
    /// If a pending group already exists, this is a no-op.
    /// Subsequent `push_op` calls will join this group.
    pub fn begin_group(&mut self, cursor_before: CharOffset) {
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
    pub fn push_op(&mut self, op: EditOp, cursor_before: CharOffset) {
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

    /// Start a new undo chain at the current end of history.
    pub fn start_undo_chain(&mut self) {
        self.undo_cursor = Some(self.entries.len());
        self.undo_chain_base = self.entries.len();
    }

    /// Set the undo cursor position.
    pub fn set_undo_cursor(&mut self, cursor: Option<usize>) {
        self.undo_cursor = cursor;
    }

    /// Get the undo chain base.
    pub fn undo_chain_base(&self) -> Option<usize> {
        if self.undo_cursor.is_some() {
            Some(self.undo_chain_base)
        } else {
            None
        }
    }

    /// Append an inverse entry during undo and adjust distance_from_save.
    pub fn push_undo_inverse(&mut self, entry: UndoEntry, original_direction: i32) {
        self.distance_from_save -= original_direction;
        self.entries.push(entry);
    }

    /// During redo, adjust distance_from_save for a replayed entry.
    pub fn apply_redo_entry(&mut self, direction: i32) {
        self.distance_from_save += direction;
    }

    /// Reset distance from save to 0 (called after save completes).
    pub fn reset_distance_from_save(&mut self) {
        self.flush_pending();
        self.distance_from_save = 0;
    }

    /// Set distance_from_save to a specific value (for session restore).
    pub fn set_distance_from_save(&mut self, distance: i32) {
        self.distance_from_save = distance;
    }

    /// Number of committed entries (excludes pending).
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }
}

// ── Doc trait ──

/// Pure content interface for text documents.
///
/// No undo, no version tracking, no dirty state. Those live on BufferState.
pub trait Doc: Send + Sync {
    // Display
    fn line_count(&self) -> usize;
    fn line(&self, idx: Row) -> String;

    // Coordinate conversion
    fn line_to_char(&self, line_idx: Row) -> CharOffset;
    fn char_to_line(&self, char_idx: CharOffset) -> Row;
    fn line_len(&self, line_idx: Row) -> usize;

    // Byte-level access (needed by tree-sitter)
    fn len_bytes(&self) -> usize;
    fn line_to_byte(&self, line_idx: Row) -> usize;
    fn byte_to_line(&self, byte_idx: usize) -> Row;
    fn byte_to_char(&self, byte_idx: usize) -> usize;
    fn char_to_byte(&self, char_idx: usize) -> usize;
    /// Returns (chunk_str, chunk_byte_start) for the chunk containing `byte_offset`.
    fn chunk_at_byte(&self, byte_offset: usize) -> (&str, usize);

    // Identity
    fn content_hash(&self) -> ContentHash;

    // Edits — pure rope mutations
    fn insert(&self, char_idx: CharOffset, text: &str) -> Arc<dyn Doc>;
    fn remove(&self, start: CharOffset, end: CharOffset) -> Arc<dyn Doc>;

    // Text extraction
    fn slice(&self, start: CharOffset, end: CharOffset) -> String;

    // Persistence
    fn write_to(&self, writer: &mut dyn io::Write) -> io::Result<()>;

    // Clone support
    fn clone_box(&self) -> Box<dyn Doc>;
}

impl Clone for Box<dyn Doc> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

// ── InertDoc ──

/// A zero-content Doc for non-materialized buffers.
/// All operations are safe no-ops.
pub struct InertDoc;

impl Doc for InertDoc {
    fn line_count(&self) -> usize {
        0
    }
    fn line(&self, _: Row) -> String {
        String::new()
    }
    fn line_to_char(&self, _: Row) -> CharOffset {
        CharOffset(0)
    }
    fn char_to_line(&self, _: CharOffset) -> Row {
        Row(0)
    }
    fn line_len(&self, _: Row) -> usize {
        0
    }
    fn len_bytes(&self) -> usize {
        0
    }
    fn line_to_byte(&self, _: Row) -> usize {
        0
    }
    fn byte_to_line(&self, _: usize) -> Row {
        Row(0)
    }
    fn byte_to_char(&self, _: usize) -> usize {
        0
    }
    fn char_to_byte(&self, _: usize) -> usize {
        0
    }
    fn chunk_at_byte(&self, _: usize) -> (&str, usize) {
        ("", 0)
    }
    fn content_hash(&self) -> ContentHash {
        ContentHash(0)
    }
    fn insert(&self, _: CharOffset, _: &str) -> Arc<dyn Doc> {
        Arc::new(InertDoc)
    }
    fn remove(&self, _: CharOffset, _: CharOffset) -> Arc<dyn Doc> {
        Arc::new(InertDoc)
    }
    fn slice(&self, _: CharOffset, _: CharOffset) -> String {
        String::new()
    }
    fn write_to(&self, _: &mut dyn io::Write) -> io::Result<()> {
        Ok(())
    }
    fn clone_box(&self) -> Box<dyn Doc> {
        Box::new(InertDoc)
    }
}

// ── TextDoc ──

pub struct TextDoc {
    rope: Rope,
}

impl TextDoc {
    pub fn from_reader(reader: impl io::Read) -> io::Result<Self> {
        let rope = Rope::from_reader(reader)?;
        Ok(TextDoc { rope })
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }
}

impl Doc for TextDoc {
    fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    fn line(&self, idx: Row) -> String {
        if *idx >= self.rope.len_lines() {
            return String::new();
        }
        let line = self.rope.line(*idx);
        let s = line.to_string();
        s.trim_end_matches(&['\n', '\r'][..]).to_string()
    }

    fn line_to_char(&self, line_idx: Row) -> CharOffset {
        CharOffset(self.rope.line_to_char(*line_idx))
    }

    fn char_to_line(&self, char_idx: CharOffset) -> Row {
        Row(self.rope.char_to_line(*char_idx))
    }

    fn line_len(&self, line_idx: Row) -> usize {
        if *line_idx >= self.rope.len_lines() {
            return 0;
        }
        let line = self.rope.line(*line_idx);
        let mut n = line.len_chars();
        while n > 0 && matches!(line.char(n - 1), '\n' | '\r') {
            n -= 1;
        }
        n
    }

    fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    fn line_to_byte(&self, line_idx: Row) -> usize {
        self.rope.line_to_byte(*line_idx)
    }

    fn byte_to_line(&self, byte_idx: usize) -> Row {
        Row(self.rope.byte_to_line(byte_idx))
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

    fn content_hash(&self) -> ContentHash {
        let mut hasher = DefaultHasher::new();
        for chunk in self.rope.chunks() {
            hasher.write(chunk.as_bytes());
        }
        ContentHash(hasher.finish())
    }

    fn insert(&self, char_idx: CharOffset, text: &str) -> Arc<dyn Doc> {
        let mut rope = self.rope.clone();
        rope.insert(*char_idx, text);
        Arc::new(TextDoc { rope })
    }

    fn remove(&self, start: CharOffset, end: CharOffset) -> Arc<dyn Doc> {
        let mut rope = self.rope.clone();
        rope.remove(*start..*end);
        Arc::new(TextDoc { rope })
    }

    fn slice(&self, start: CharOffset, end: CharOffset) -> String {
        self.rope.slice(*start..*end).to_string()
    }

    fn write_to(&self, writer: &mut dyn io::Write) -> io::Result<()> {
        for chunk in self.rope.chunks() {
            writer.write_all(chunk.as_bytes())?;
        }
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn Doc> {
        Box::new(TextDoc {
            rope: self.rope.clone(),
        })
    }
}
