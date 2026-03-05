mod wrap;
mod editing;
mod undo;
mod watcher;
mod component;
mod color_hint;

pub use component::BufferFactory;

use std::fs::File;
use std::hash::Hasher;
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use led_core::Waker;
use ropey::Rope;
use serde::{Deserialize, Serialize};
use twox_hash::XxHash64;

// ---------------------------------------------------------------------------
// Undo data structures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EditOp {
    Insert { char_idx: usize, text: String },
    Remove { char_idx: usize, text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoEntry {
    pub op: EditOp,
    pub cursor_before: (usize, usize),
    pub cursor_after: (usize, usize),
    pub direction: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditKind {
    Insert,
    DeleteBackward,
    DeleteForward,
}

#[derive(Debug)]
pub(crate) struct PendingGroup {
    pub(crate) kind: EditKind,
    pub(crate) op: EditOp,
    pub(crate) cursor_before: (usize, usize),
    pub(crate) cursor_after: (usize, usize),
    pub(crate) last_time: Instant,
}

pub(crate) const GROUP_TIMEOUT_MS: u128 = 1000;

// ---------------------------------------------------------------------------
// Buffer
// ---------------------------------------------------------------------------

pub fn notify_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("led/notify"))
}

pub struct Buffer {
    pub(crate) rope: Rope,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    pub scroll_offset: usize,
    pub(crate) undo_history: Vec<UndoEntry>,
    pub(crate) undo_cursor: Option<usize>,
    pub(crate) pending_group: Option<PendingGroup>,
    pub(crate) distance_from_save: i32,
    pub(crate) save_history_len: usize,
    pub(crate) persisted_undo_len: usize,
    pub(crate) base_content_hash: u64,
    pub(crate) self_notified: bool,
    pub(crate) chain_id: Option<String>,
    pub(crate) last_seen_seq: i64,
    pub(crate) mark: Option<(usize, usize)>,
    pub(crate) kill_accumulator: Option<String>,
    pub(crate) cursor_screen_pos: Option<(u16, u16)>,
    pub(crate) text_width: usize,
    pub(crate) scroll_sub_line: usize,
    // File watching
    pub(crate) _watcher: Option<notify::RecommendedWatcher>,
    #[allow(dead_code)]
    pub(crate) waker: Option<Waker>,
    pub(crate) changed: Arc<AtomicBool>,
    pub(crate) disk_modified: bool,
    pub(crate) disk_deleted: bool,
    pub preview: bool,
}

impl Buffer {
    // --- Constructors ---

    pub fn empty() -> Self {
        let rope = Rope::from_str("");
        let base_content_hash = Self::hash_rope(&rope);
        Self {
            rope,
            cursor_row: 0,
            cursor_col: 0,
            path: None,
            dirty: false,
            scroll_offset: 0,
            undo_history: Vec::new(),
            undo_cursor: None,
            pending_group: None,
            distance_from_save: 0,
            save_history_len: 0,
            persisted_undo_len: 0,
            base_content_hash,
            self_notified: false,
            chain_id: None,
            last_seen_seq: 0,
            mark: None,
            kill_accumulator: None,
            cursor_screen_pos: None,
            text_width: 0,
            scroll_sub_line: 0,
            _watcher: None,
            waker: None,
            changed: Arc::new(AtomicBool::new(false)),
            disk_modified: false,
            disk_deleted: false,
            preview: false,
        }
    }

    pub fn from_file(path: &str) -> io::Result<Self> {
        Self::from_file_with_waker(path, None)
    }

    pub fn from_file_with_waker(path: &str, waker: Option<Waker>) -> io::Result<Self> {
        // Reject binary files by checking for null bytes in the first 8KB
        {
            let mut probe = File::open(path)?;
            let mut buf = [0u8; 8192];
            let n = io::Read::read(&mut probe, &mut buf)?;
            if buf[..n].contains(&0) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "binary file"));
            }
        }
        let file = File::open(path)?;
        let rope = Rope::from_reader(BufReader::new(file))?;
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
        let base_content_hash = Self::hash_rope(&rope);
        let changed = Arc::new(AtomicBool::new(false));
        let _watcher = Self::create_watcher(&canonical, &changed, waker.as_ref());
        Ok(Self {
            rope,
            cursor_row: 0,
            cursor_col: 0,
            path: Some(canonical),
            dirty: false,
            scroll_offset: 0,
            undo_history: Vec::new(),
            undo_cursor: None,
            pending_group: None,
            distance_from_save: 0,
            save_history_len: 0,
            persisted_undo_len: 0,
            base_content_hash,
            self_notified: false,
            chain_id: None,
            last_seen_seq: 0,
            mark: None,
            kill_accumulator: None,
            cursor_screen_pos: None,
            text_width: 0,
            scroll_sub_line: 0,
            _watcher,
            waker,
            changed,
            disk_modified: false,
            disk_deleted: false,
            preview: false,
        })
    }

    pub(crate) fn generate_chain_id() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{}-{}", std::process::id(), ts)
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

    pub(crate) fn char_idx(&self, row: usize, col: usize) -> usize {
        self.rope.line_to_char(row) + col
    }

    // --- Helpers ---

    pub(crate) fn current_line_len(&self) -> usize {
        self.line_len(self.cursor_row)
    }

    pub(crate) fn clamp_cursor_col(&mut self) {
        let len = self.current_line_len();
        if self.cursor_col > len {
            self.cursor_col = len;
        }
    }

    // --- Hashing ---

    pub(crate) fn hash_rope(rope: &Rope) -> u64 {
        let mut hasher = XxHash64::with_seed(0);
        for chunk in rope.chunks() {
            hasher.write(chunk.as_bytes());
        }
        hasher.finish()
    }

    pub fn base_content_hash(&self) -> u64 {
        self.base_content_hash
    }

    pub fn content_hash(&self) -> u64 {
        Self::hash_rope(&self.rope)
    }
}
