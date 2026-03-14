use std::io;
use std::sync::Arc;

use ropey::Rope;

#[derive(Clone, Debug, Default)]
pub struct UndoHistory {}

pub trait Doc: Send + Sync {
    // Display
    fn line_count(&self) -> usize;
    fn line(&self, idx: usize) -> String;

    // Identity & change detection
    fn version(&self) -> u64;
    fn dirty(&self) -> bool;

    // Edits
    fn insert(&self, char_idx: usize, text: &str) -> Arc<dyn Doc>;
    fn remove(&self, start: usize, end: usize) -> Arc<dyn Doc>;

    // Persistence
    fn write_to(&self, writer: &mut dyn io::Write) -> io::Result<()>;

    // Undo
    fn undo_history(&self) -> &UndoHistory;

    // Clone support
    fn clone_box(&self) -> Box<dyn Doc>;
}

impl Clone for Box<dyn Doc> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

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

    fn version(&self) -> u64 {
        self.version
    }

    fn dirty(&self) -> bool {
        false
    }

    fn insert(&self, char_idx: usize, text: &str) -> Arc<dyn Doc> {
        let mut rope = self.rope.clone();
        rope.insert(char_idx, text);
        Arc::new(TextDoc {
            rope,
            version: self.version + 1,
            undo: self.undo.clone(),
        })
    }

    fn remove(&self, start: usize, end: usize) -> Arc<dyn Doc> {
        let mut rope = self.rope.clone();
        rope.remove(start..end);
        Arc::new(TextDoc {
            rope,
            version: self.version + 1,
            undo: self.undo.clone(),
        })
    }

    fn write_to(&self, writer: &mut dyn io::Write) -> io::Result<()> {
        for chunk in self.rope.chunks() {
            writer.write_all(chunk.as_bytes())?;
        }
        Ok(())
    }

    fn undo_history(&self) -> &UndoHistory {
        &self.undo
    }

    fn clone_box(&self) -> Box<dyn Doc> {
        Box::new(TextDoc {
            rope: self.rope.clone(),
            version: self.version,
            undo: self.undo.clone(),
        })
    }
}
