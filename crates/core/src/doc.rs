use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use ropey::Rope;

use crate::lsp_types::{EditorPosition, EditorRange, EditorTextEdit};

// ---------------------------------------------------------------------------
// TextDoc
// ---------------------------------------------------------------------------

pub struct TextDoc {
    rope: Rope,
    pending_changes: Vec<EditorTextEdit>,
    version: i32,
}

impl TextDoc {
    pub fn new() -> Self {
        Self {
            rope: Rope::from_str(""),
            pending_changes: Vec::new(),
            version: 0,
        }
    }

    pub fn from_reader<R: io::Read>(reader: R) -> io::Result<Self> {
        let rope = Rope::from_reader(reader)?;
        Ok(Self {
            rope,
            pending_changes: Vec::new(),
            version: 0,
        })
    }

    // --- Mutations (track changes) ---

    pub fn insert(&mut self, char_idx: usize, text: &str) {
        let row = self.rope.char_to_line(char_idx);
        let line_start = self.rope.line_to_char(row);
        let col = char_idx - line_start;

        self.pending_changes.push(EditorTextEdit {
            range: EditorRange {
                start: EditorPosition { row, col },
                end: EditorPosition { row, col },
            },
            new_text: text.to_string(),
        });

        self.rope.insert(char_idx, text);
        self.version += 1;
    }

    pub fn insert_char(&mut self, char_idx: usize, ch: char) {
        let row = self.rope.char_to_line(char_idx);
        let line_start = self.rope.line_to_char(row);
        let col = char_idx - line_start;

        self.pending_changes.push(EditorTextEdit {
            range: EditorRange {
                start: EditorPosition { row, col },
                end: EditorPosition { row, col },
            },
            new_text: ch.to_string(),
        });

        self.rope.insert_char(char_idx, ch);
        self.version += 1;
    }

    pub fn remove(&mut self, start: usize, end: usize) {
        let start_row = self.rope.char_to_line(start);
        let start_line_start = self.rope.line_to_char(start_row);
        let start_col = start - start_line_start;

        let end_row = self.rope.char_to_line(end);
        let end_line_start = self.rope.line_to_char(end_row);
        let end_col = end - end_line_start;

        self.pending_changes.push(EditorTextEdit {
            range: EditorRange {
                start: EditorPosition {
                    row: start_row,
                    col: start_col,
                },
                end: EditorPosition {
                    row: end_row,
                    col: end_col,
                },
            },
            new_text: String::new(),
        });

        self.rope.remove(start..end);
        self.version += 1;
    }

    // --- Accessors ---

    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn line(&self, idx: usize) -> String {
        let rope_line = self.rope.line(idx);
        let s = rope_line.to_string();
        s.trim_end_matches('\n').to_string()
    }

    pub fn line_len(&self, idx: usize) -> usize {
        let rope_line = self.rope.line(idx);
        let len = rope_line.len_chars();
        if len > 0 && rope_line.char(len - 1) == '\n' {
            len - 1
        } else {
            len
        }
    }

    pub fn char_idx(&self, row: usize, col: usize) -> usize {
        self.rope.line_to_char(row) + col
    }

    pub fn line_to_char(&self, line: usize) -> usize {
        self.rope.line_to_char(line)
    }

    pub fn char_to_line(&self, char_idx: usize) -> usize {
        self.rope.char_to_line(char_idx)
    }

    pub fn char(&self, idx: usize) -> char {
        self.rope.char(idx)
    }

    pub fn slice(&self, start: usize, end: usize) -> ropey::RopeSlice<'_> {
        self.rope.slice(start..end)
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        self.rope.to_string()
    }

    // --- LSP ---

    pub fn drain_changes(&mut self) -> Vec<EditorTextEdit> {
        std::mem::take(&mut self.pending_changes)
    }

    pub fn version(&self) -> i32 {
        self.version
    }

    // --- I/O ---

    pub fn write_to<W: io::Write>(&self, writer: W) -> io::Result<()> {
        self.rope.write_to(writer)
    }

    /// Replace the rope entirely (e.g., on reload from disk).
    /// Clears pending changes and resets version.
    pub fn replace_rope(&mut self, rope: Rope) {
        self.rope = rope;
        self.pending_changes.clear();
        self.version = 0;
    }
}

// ---------------------------------------------------------------------------
// DocStore
// ---------------------------------------------------------------------------

pub struct DocStore {
    docs: HashMap<PathBuf, TextDoc>,
}

impl DocStore {
    pub fn new() -> Self {
        Self {
            docs: HashMap::new(),
        }
    }

    pub fn insert(&mut self, path: PathBuf, doc: TextDoc) {
        self.docs.insert(path, doc);
    }

    pub fn get(&self, path: &Path) -> Option<&TextDoc> {
        self.docs.get(path)
    }

    pub fn get_mut(&mut self, path: &Path) -> Option<&mut TextDoc> {
        self.docs.get_mut(path)
    }

    pub fn remove(&mut self, path: &Path) -> Option<TextDoc> {
        self.docs.remove(path)
    }

    pub fn content(&self, path: &Path) -> Option<String> {
        self.docs.get(path).map(|d| d.to_string())
    }

    pub fn drain_changes(&mut self, path: &Path) -> Vec<EditorTextEdit> {
        self.docs
            .get_mut(path)
            .map(|d| d.drain_changes())
            .unwrap_or_default()
    }

    pub fn version(&self, path: &Path) -> Option<i32> {
        self.docs.get(path).map(|d| d.version())
    }
}
