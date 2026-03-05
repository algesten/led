use std::fs::File;
use std::hash::Hasher;
use std::io::{self, BufReader, BufWriter};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use led_core::{
    Action, Component, Context, DrawContext, Effect, Event, PanelClaim, PanelSlot, TabDescriptor,
    Waker,
};
use notify::{RecursiveMode, Watcher};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
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
// Soft-wrap helpers
// ---------------------------------------------------------------------------

/// Expand tabs to 4 spaces, returning display chars and char-index-to-display-column map.
/// The map has len = num_source_chars + 1 (sentinel at end == display.len()).
fn expand_tabs(line: &str) -> (Vec<char>, Vec<usize>) {
    let mut display: Vec<char> = Vec::with_capacity(line.len());
    let mut char_map = Vec::with_capacity(line.len() + 1);
    for ch in line.chars() {
        char_map.push(display.len());
        if ch == '\t' {
            display.extend([' ', ' ', ' ', ' ']);
        } else {
            display.push(ch);
        }
    }
    char_map.push(display.len());
    (display, char_map)
}

/// Collect a slice of chars into a String.
fn chars_to_string(chars: &[char]) -> String {
    chars.iter().collect()
}

/// How many screen rows a line of `display_width` occupies at the given `text_width`.
fn visual_line_count(display_width: usize, text_width: usize) -> usize {
    if text_width <= 1 || display_width <= text_width {
        return 1;
    }
    let wrap_width = text_width - 1;
    let mut count = 0;
    let mut remaining = display_width;
    while remaining > text_width {
        count += 1;
        remaining -= wrap_width;
    }
    count + 1
}

/// Split a line into (start, end) display-column char ranges per visual line.
/// Non-last chunks have `wrap_width = text_width - 1` content columns (room for `\`).
fn compute_chunks(display_width: usize, text_width: usize) -> Vec<(usize, usize)> {
    if text_width <= 1 || display_width <= text_width {
        return vec![(0, display_width)];
    }
    let wrap_width = text_width - 1;
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < display_width {
        let remaining = display_width - start;
        if remaining <= text_width {
            chunks.push((start, display_width));
            break;
        }
        chunks.push((start, start + wrap_width));
        start += wrap_width;
    }
    chunks
}

/// Find which sub-line (chunk index) contains display column `dcol`.
fn find_sub_line(chunks: &[(usize, usize)], dcol: usize) -> usize {
    for (i, &(_cs, ce)) in chunks.iter().enumerate() {
        if dcol < ce || i == chunks.len() - 1 {
            return i;
        }
    }
    0
}

/// Reverse map from display column to logical char index.
fn display_col_to_char_idx(char_map: &[usize], target_dcol: usize) -> usize {
    let num_chars = char_map.len().saturating_sub(1);
    if num_chars > 0 && target_dcol >= char_map[num_chars] {
        return num_chars;
    }
    for i in (0..num_chars).rev() {
        if char_map[i] <= target_dcol {
            return i;
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Buffer
// ---------------------------------------------------------------------------

pub fn notify_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("led/notify"))
}

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
    distance_from_save: i32,
    save_history_len: usize,
    persisted_undo_len: usize,
    base_content_hash: u64,
    self_notified: bool,
    chain_id: Option<String>,
    last_seen_seq: i64,
    mark: Option<(usize, usize)>,
    kill_accumulator: Option<String>,
    cursor_screen_pos: Option<(u16, u16)>,
    text_width: usize,
    scroll_sub_line: usize,
    // File watching
    _watcher: Option<notify::RecommendedWatcher>,
    #[allow(dead_code)]
    waker: Option<Waker>,
    changed: Arc<AtomicBool>,
    disk_modified: bool,
    disk_deleted: bool,
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

    fn create_watcher(
        source_path: &std::path::Path,
        changed: &Arc<AtomicBool>,
        waker: Option<&Waker>,
    ) -> Option<notify::RecommendedWatcher> {
        let changed = changed.clone();
        let waker = waker.cloned();
        let source_file = source_path.to_path_buf();
        let source_parent = source_path.parent()?.to_path_buf();
        let notify_hash = Self::notify_hash_for_path(source_path);
        let notify_dir = notify_dir();
        let notify_dir_for_closure = notify_dir.clone();

        let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            let Ok(ev) = res else { return };
            match ev.kind {
                notify::EventKind::Create(_)
                | notify::EventKind::Modify(_)
                | notify::EventKind::Remove(_) => {}
                _ => return,
            }
            let dominated = ev.paths.iter().any(|p| {
                if p == &source_file {
                    return true;
                }
                if let Some(ref nd) = notify_dir_for_closure {
                    if *p == nd.join(&notify_hash) {
                        return true;
                    }
                }
                false
            });
            if dominated {
                changed.store(true, Ordering::SeqCst);
                if let Some(ref w) = waker {
                    w();
                }
            }
        }).ok()?;

        watcher.watch(&source_parent, RecursiveMode::NonRecursive).ok()?;
        if let Some(ref nd) = notify_dir {
            let _ = std::fs::create_dir_all(nd);
            let _ = watcher.watch(nd, RecursiveMode::NonRecursive);
        }
        Some(watcher)
    }

    fn has_local_changes(&self) -> bool {
        self.dirty
    }

    fn handle_tick(&mut self, ctx: &mut Context) -> Vec<Effect> {
        if !self.changed.swap(false, Ordering::SeqCst) {
            return vec![];
        }

        // Check cross-instance sync first
        self.check_cross_instance_sync(ctx);

        // Check disk state for external modifications
        let Some(ref path) = self.path else {
            return vec![];
        };

        if !path.exists() {
            if !self.disk_deleted {
                self.disk_deleted = true;
                return vec![Effect::SetMessage(format!(
                    "Warning: {} deleted externally.",
                    self.filename()
                ))];
            }
            return vec![];
        }

        // File exists — read and hash it
        let disk_hash = match File::open(path) {
            Ok(f) => {
                match Rope::from_reader(BufReader::new(f)) {
                    Ok(rope) => Self::hash_rope(&rope),
                    Err(_) => return vec![],
                }
            }
            Err(_) => return vec![],
        };

        // If disk hash matches our base, no external change (covers own save)
        if disk_hash == self.base_content_hash {
            return vec![];
        }

        // Clear deleted flag since file exists now
        self.disk_deleted = false;

        if self.has_local_changes() {
            // Buffer is dirty — flag conflict, don't reload
            if !self.disk_modified {
                self.disk_modified = true;
                return vec![Effect::SetMessage(format!(
                    "Warning: {} changed on disk (you have unsaved changes).",
                    self.filename()
                ))];
            }
            vec![]
        } else {
            // Buffer is clean — auto-reload
            self.reload_from_disk();
            self.disk_modified = false;
            self.disk_deleted = false;
            let max_line = self.line_count().saturating_sub(1);
            if self.cursor_row > max_line {
                self.cursor_row = max_line;
            }
            self.clamp_cursor_col();
            vec![Effect::SetMessage(format!(
                "Reloaded {} (changed on disk).",
                self.filename()
            ))]
        }
    }

    fn check_cross_instance_sync(&mut self, ctx: &mut Context) {
        if self.self_notified {
            self.self_notified = false;
            return;
        }
        let Some(conn) = ctx.db else { return };
        let file_str = match self.path {
            Some(ref p) => p.to_string_lossy().into_owned(),
            None => return,
        };
        let root_str = ctx.root.to_string_lossy();

        struct Row {
            chain_id: String,
            content_hash: i64,
            seq: Option<i64>,
            entry_data: Option<Vec<u8>>,
        }
        let rows: Vec<Row> = conn
            .prepare(
                "SELECT s.chain_id, s.content_hash, e.seq, e.entry_data
                 FROM buffer_undo_state s
                 LEFT JOIN undo_entries e
                   ON e.root_path = s.root_path AND e.file_path = s.file_path AND e.seq > ?3
                 WHERE s.root_path = ?1 AND s.file_path = ?2
                 ORDER BY e.seq",
            )
            .and_then(|mut stmt| {
                let mapped = stmt.query_map(
                    rusqlite::params![&*root_str, &*file_str, self.last_seen_seq],
                    |row| {
                        Ok(Row {
                            chain_id: row.get(0)?,
                            content_hash: row.get(1)?,
                            seq: row.get(2)?,
                            entry_data: row.get(3)?,
                        })
                    },
                )?;
                Ok(mapped.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();

        if rows.is_empty() {
            if self.dirty {
                self.reload_from_disk();
                self.mark_externally_saved();
            }
            return;
        }

        let remote_chain_id = &rows[0].chain_id;
        let remote_content_hash = rows[0].content_hash as u64;
        let same_chain = self.chain_id.as_deref() == Some(remote_chain_id);

        if same_chain {
            let mut entries = Vec::new();
            let mut max_seq = self.last_seen_seq;
            for row in &rows {
                if let (Some(seq), Some(data)) = (row.seq, &row.entry_data) {
                    if let Ok(entry) = rmp_serde::from_slice::<UndoEntry>(data) {
                        entries.push(entry);
                        max_seq = max_seq.max(seq);
                    }
                }
            }
            if !entries.is_empty() {
                self.apply_remote_entries(entries, max_seq);
            }
        } else {
            if self.base_content_hash != remote_content_hash {
                self.reload_from_disk();
            }
            let new_chain = remote_chain_id.clone();
            let all_entries = Self::load_entries_after(conn, &root_str, &file_str, 0);
            let max_seq = all_entries.last().map(|(s, _)| *s).unwrap_or(0);
            let entries: Vec<UndoEntry> = all_entries.into_iter().map(|(_, e)| e).collect();
            if !entries.is_empty() {
                self.apply_remote_entries(entries, max_seq);
            } else {
                self.last_seen_seq = max_seq;
            }
            self.chain_id = Some(new_chain);
        }
    }

    fn notify_hash_for_path(path: &std::path::Path) -> String {
        use std::hash::Hash;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hasher);
        format!("{:016x}", std::hash::Hasher::finish(&hasher))
    }

    fn touch_notify(&mut self) {
        let Some(ref path) = self.path else { return };
        let Some(dir) = notify_dir() else { return };
        let hash = Self::notify_hash_for_path(path);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(&hash), b"");
        self.self_notified = true;
    }

    fn generate_chain_id() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{}-{}", std::process::id(), ts)
    }

    fn reload_from_disk(&mut self) {
        let Some(ref path) = self.path else { return };
        let Ok(file) = File::open(path) else { return };
        let Ok(rope) = Rope::from_reader(BufReader::new(file)) else { return };
        self.rope = rope;
        self.base_content_hash = Self::hash_rope(&self.rope);
        self.undo_history.clear();
        self.undo_cursor = None;
        self.pending_group = None;
        self.distance_from_save = 0;
        self.save_history_len = 0;
        self.persisted_undo_len = 0;
        self.dirty = false;
    }

    fn load_entries_after(
        conn: &rusqlite::Connection,
        root: &str,
        file: &str,
        after_seq: i64,
    ) -> Vec<(i64, UndoEntry)> {
        conn.prepare(
            "SELECT seq, entry_data FROM undo_entries
             WHERE root_path = ?1 AND file_path = ?2 AND seq > ?3
             ORDER BY seq",
        )
        .and_then(|mut stmt| {
            let rows = stmt.query_map(
                rusqlite::params![root, file, after_seq],
                |row| {
                    let seq: i64 = row.get(0)?;
                    let data: Vec<u8> = row.get(1)?;
                    let entry: UndoEntry = rmp_serde::from_slice(&data).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0, rusqlite::types::Type::Blob, Box::new(e),
                        )
                    })?;
                    Ok((seq, entry))
                },
            )?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default()
    }

    pub fn save(&mut self, ctx: &Context) -> io::Result<()> {
        self.flush_pending();
        if let Some(ref path) = self.path {
            let len = self.rope.len_chars();
            if len == 0 || self.rope.char(len - 1) != '\n' {
                self.rope.insert_char(len, '\n');
            }
            let file = File::create(path)?;
            self.rope.write_to(BufWriter::new(file))?;
            self.dirty = false;
            self.distance_from_save = 0;
            self.save_history_len = self.undo_history.len();
            self.persisted_undo_len = self.save_history_len;
            self.base_content_hash = self.content_hash();
            self.chain_id = None;
            self.last_seen_seq = 0;
            self.disk_modified = false;
            self.disk_deleted = false;

            if let Some(conn) = ctx.db {
                let root_str = ctx.root.to_string_lossy();
                let file_str = path.to_string_lossy();
                let _ = conn.execute(
                    "DELETE FROM buffer_undo_state WHERE root_path = ?1 AND file_path = ?2",
                    rusqlite::params![&*root_str, &*file_str],
                );
            }
            self.touch_notify();

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

    fn char_idx(&self, row: usize, col: usize) -> usize {
        self.rope.line_to_char(row) + col
    }

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
        self.rope.insert_char(idx, ch);
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
            self.rope.remove(start..end);
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
                direction: 1,
            });
            Some("\n".to_string())
        } else {
            None
        }
    }

    // --- Mark / Selection ---

    fn set_mark(&mut self) {
        self.mark = Some((self.cursor_row, self.cursor_col));
    }

    fn clear_mark(&mut self) {
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
    }

    fn selection_range(&self) -> Option<((usize, usize), (usize, usize))> {
        let mark = self.mark?;
        let cursor = (self.cursor_row, self.cursor_col);
        if mark <= cursor {
            Some((mark, cursor))
        } else {
            Some((cursor, mark))
        }
    }

    fn selected_text(&self) -> Option<String> {
        let ((sr, sc), (er, ec)) = self.selection_range()?;
        let start = self.char_idx(sr, sc);
        let end = self.char_idx(er, ec);
        if start == end {
            return None;
        }
        Some(self.rope.slice(start..end).to_string())
    }

    fn kill_region(&mut self) -> Option<String> {
        let ((sr, sc), (er, ec)) = self.selection_range()?;
        let start_idx = self.char_idx(sr, sc);
        let end_idx = self.char_idx(er, ec);
        if start_idx == end_idx {
            self.clear_mark();
            return None;
        }
        let text: String = self.rope.slice(start_idx..end_idx).to_string();
        let cursor_before = (self.cursor_row, self.cursor_col);
        self.rope.remove(start_idx..end_idx);
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

    fn yank_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let cursor_before = (self.cursor_row, self.cursor_col);
        let idx = self.char_idx(self.cursor_row, self.cursor_col);
        self.rope.insert(idx, text);
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

        self.apply_op(&inverse.op);
        self.cursor_row = inverse.cursor_after.0;
        self.cursor_col = inverse.cursor_after.1;
        self.distance_from_save -= entry.direction;
        self.dirty = self.distance_from_save != 0;

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
            direction: -entry.direction,
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
                match (&mut pg.op, &op) {
                    (EditOp::Insert { text: acc, .. }, EditOp::Insert { text: new, .. }) => {
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
                            acc.insert_str(0, new);
                            *acc_idx = *new_idx;
                        } else {
                            acc.push_str(new);
                        }
                    }
                    _ => {
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
            self.distance_from_save += 1;
            self.undo_history.push(UndoEntry {
                op: pg.op,
                cursor_before: pg.cursor_before,
                cursor_after: pg.cursor_after,
                direction: 1,
            });
            self.undo_cursor = None;
        }
    }

    fn push_undo(&mut self, entry: UndoEntry) {
        self.distance_from_save += 1;
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

    /// Adjust scroll so the cursor is visible within `height` visual rows.
    /// Scroll is tracked as (scroll_offset, scroll_sub_line) — a logical line
    /// plus a sub-line offset within it — so scrolling is visual-line granular.
    fn adjust_scroll(&mut self, text_width: usize, height: usize) {
        if height == 0 || text_width == 0 {
            return;
        }

        let total = self.line_count();

        // Clamp scroll_offset / scroll_sub_line to valid range
        if self.scroll_offset >= total {
            self.scroll_offset = total.saturating_sub(1);
            self.scroll_sub_line = 0;
        }
        let scroll_vl = visual_line_count(
            expand_tabs(&self.line(self.scroll_offset)).0.len(),
            text_width,
        );
        if self.scroll_sub_line >= scroll_vl {
            self.scroll_sub_line = scroll_vl.saturating_sub(1);
        }

        // Compute cursor's sub-line within its logical line
        let (cursor_display, cursor_cm) = expand_tabs(&self.line(self.cursor_row));
        let cursor_dc = cursor_cm
            .get(self.cursor_col)
            .copied()
            .unwrap_or_else(|| cursor_cm.last().copied().unwrap_or(0));
        let cursor_chunks = compute_chunks(cursor_display.len(), text_width);
        let cursor_sub = find_sub_line(&cursor_chunks, cursor_dc);

        // Case 1: cursor above viewport
        if self.cursor_row < self.scroll_offset
            || (self.cursor_row == self.scroll_offset && cursor_sub < self.scroll_sub_line)
        {
            self.scroll_offset = self.cursor_row;
            self.scroll_sub_line = cursor_sub;
            return;
        }

        // Case 2: check if cursor is visible
        let mut vrow: usize = 0;

        if self.cursor_row == self.scroll_offset {
            // Same line — just check sub-line distance
            let cursor_vrow = cursor_sub - self.scroll_sub_line;
            if cursor_vrow < height {
                return;
            }
        } else {
            // First logical line: only count sub-lines from scroll_sub_line onward
            vrow += scroll_vl - self.scroll_sub_line;

            // Intermediate lines
            let limit = self.cursor_row.min(self.scroll_offset + height);
            for li in (self.scroll_offset + 1)..limit {
                vrow += visual_line_count(
                    expand_tabs(&self.line(li)).0.len(),
                    text_width,
                );
                if vrow >= height {
                    break;
                }
            }

            if vrow + cursor_sub < height {
                return;
            }
        }

        // Case 3: cursor not visible — place cursor near bottom.
        // We need (height - 1) visual rows above cursor's sub-line.
        let mut remaining = height - 1;

        if cursor_sub <= remaining {
            remaining -= cursor_sub;
        } else {
            // Line itself is taller than viewport at cursor's sub-line
            self.scroll_offset = self.cursor_row;
            self.scroll_sub_line = cursor_sub.saturating_sub(height - 1);
            return;
        }

        let mut new_scroll = self.cursor_row;
        let mut new_sub: usize = 0;

        for li in (0..self.cursor_row).rev() {
            if remaining == 0 {
                break;
            }
            let vl = visual_line_count(
                expand_tabs(&self.line(li)).0.len(),
                text_width,
            );
            if vl <= remaining {
                remaining -= vl;
                new_scroll = li;
                new_sub = 0;
            } else {
                // Partially fits — start from a sub-line within this line
                new_scroll = li;
                new_sub = vl - remaining;
                break;
            }
        }

        self.scroll_offset = new_scroll;
        self.scroll_sub_line = new_sub;
    }

    // --- Undo persistence ---

    fn hash_rope(rope: &Rope) -> u64 {
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

    fn has_unpersisted_undo(&self) -> bool {
        self.pending_group.is_some() || self.undo_history.len() > self.persisted_undo_len
    }

    fn flush_undo_to_db(&mut self, ctx: &Context) {
        let Some(conn) = ctx.db else { return };
        let file_str = match self.path {
            Some(ref p) => p.to_string_lossy().into_owned(),
            None => return,
        };
        let root_str = ctx.root.to_string_lossy();

        self.flush_pending();
        let start = self.persisted_undo_len;
        if start >= self.undo_history.len() {
            return;
        }

        if self.chain_id.is_none() {
            self.chain_id = Some(Self::generate_chain_id());
        }
        let chain_id = self.chain_id.as_ref().unwrap();

        let entries: Vec<Vec<u8>> = self.undo_history[start..]
            .iter()
            .map(|entry| rmp_serde::to_vec(entry).expect("serialize undo entry"))
            .collect();
        self.persisted_undo_len = self.undo_history.len();

        let result: rusqlite::Result<()> = (|| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO buffer_undo_state (root_path, file_path, chain_id, content_hash, undo_cursor, distance_from_save)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(root_path, file_path) DO UPDATE SET
                    chain_id = excluded.chain_id,
                    content_hash = excluded.content_hash,
                    undo_cursor = excluded.undo_cursor,
                    distance_from_save = excluded.distance_from_save",
                rusqlite::params![
                    &*root_str, &*file_str, chain_id,
                    self.base_content_hash as i64,
                    self.undo_cursor.map(|v| v as i64),
                    self.distance_from_save
                ],
            )?;
            for data in &entries {
                tx.execute(
                    "INSERT INTO undo_entries (root_path, file_path, entry_data)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![&*root_str, &*file_str, data],
                )?;
            }
            tx.commit()?;
            Ok(())
        })();

        if let Err(e) = result {
            eprintln!("warning: failed to flush undo entries: {e}");
        }

        if let Ok(max_seq) = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM undo_entries WHERE root_path = ?1 AND file_path = ?2",
            rusqlite::params![&*root_str, &*file_str],
            |row| row.get::<_, i64>(0),
        ) {
            self.last_seen_seq = max_seq;
        }

        self.touch_notify();
    }

    fn mark_externally_saved(&mut self) {
        self.dirty = false;
        self.distance_from_save = 0;
        self.base_content_hash = self.content_hash();
        self.undo_history.clear();
        self.undo_cursor = None;
        self.save_history_len = 0;
        self.persisted_undo_len = 0;
        self.chain_id = None;
        self.last_seen_seq = 0;
    }

    fn apply_remote_entries(&mut self, entries: Vec<UndoEntry>, new_last_seen_seq: i64) {
        self.flush_pending();
        for entry in &entries {
            self.apply_op(&entry.op);
            self.distance_from_save += entry.direction;
        }
        self.undo_history.extend(entries);
        self.persisted_undo_len = self.undo_history.len();
        self.last_seen_seq = new_last_seen_seq;
        if let Some(last) = self.undo_history.last() {
            self.cursor_row = last.cursor_after.0;
            self.cursor_col = last.cursor_after.1;
        }
        self.dirty = self.distance_from_save != 0;
    }

    pub fn restore_undo(
        &mut self,
        entries: Vec<UndoEntry>,
        undo_cursor: Option<usize>,
        distance_from_save: i32,
    ) {
        for entry in &entries {
            self.apply_op(&entry.op);
        }
        self.undo_history = entries;
        self.undo_cursor = undo_cursor;
        self.distance_from_save = distance_from_save;
        self.dirty = distance_from_save != 0;
        self.persisted_undo_len = self.undo_history.len();
        self.save_history_len = 0;
        if let Some(last) = self.undo_history.last() {
            self.cursor_row = last.cursor_after.0;
            self.cursor_col = last.cursor_after.1;
        }
    }

}

// ---------------------------------------------------------------------------
// Component implementation for Buffer
// ---------------------------------------------------------------------------

impl Component for Buffer {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }

    fn panel_claims(&self) -> &[PanelClaim] {
        &[PanelClaim {
            slot: PanelSlot::Main,
            priority: 10,
        }]
    }

    fn tab(&self) -> Option<TabDescriptor> {
        Some(TabDescriptor {
            label: self.filename().to_string(),
            dirty: self.dirty,
            path: self.path.clone(),
            preview: self.preview,
        })
    }

    fn handle_action(&mut self, action: Action, ctx: &mut Context) -> Vec<Effect> {
        // Clear kill accumulator for non-KillLine actions
        if !matches!(action, Action::KillLine) {
            self.kill_accumulator = None;
        }

        match action {
            Action::InsertChar(c) => {
                self.clear_mark();
                self.insert_char(c);
                vec![]
            }
            Action::MoveUp => {
                self.move_up();
                vec![]
            }
            Action::MoveDown => {
                self.move_down();
                vec![]
            }
            Action::MoveLeft => {
                self.move_left();
                vec![]
            }
            Action::MoveRight => {
                self.move_right();
                vec![]
            }
            Action::LineStart => {
                self.move_to_line_start();
                vec![]
            }
            Action::LineEnd => {
                self.move_to_line_end();
                vec![]
            }
            Action::PageUp => {
                self.page_up(ctx.viewport_height);
                vec![]
            }
            Action::PageDown => {
                self.page_down(ctx.viewport_height);
                vec![]
            }
            Action::FileStart => {
                self.move_to_file_start();
                vec![]
            }
            Action::FileEnd => {
                self.move_to_file_end();
                vec![]
            }
            Action::InsertNewline => {
                self.clear_mark();
                self.insert_newline();
                vec![]
            }
            Action::DeleteBackward => {
                self.clear_mark();
                self.delete_char_backward();
                vec![]
            }
            Action::DeleteForward => {
                self.clear_mark();
                self.delete_char_forward();
                vec![]
            }
            Action::InsertTab => {
                self.clear_mark();
                self.insert_char('\t');
                vec![]
            }
            Action::KillLine => {
                if let Some(killed) = self.kill_line() {
                    let acc = self.kill_accumulator.get_or_insert_with(String::new);
                    acc.push_str(&killed);
                    ctx.clipboard.set_text(&acc);
                    vec![]
                } else {
                    vec![]
                }
            }
            Action::Undo => {
                self.undo();
                vec![]
            }
            Action::Save => {
                if self.disk_modified {
                    vec![Effect::ConfirmAction {
                        prompt: format!(
                            "{} changed on disk; save anyway? (yes/no)",
                            self.filename()
                        ),
                        action: Action::SaveForce,
                    }]
                } else {
                    match self.save(ctx) {
                        Ok(()) => {
                            let name = self.filename().to_string();
                            vec![Effect::SetMessage(format!("Saved {name}."))]
                        }
                        Err(e) => vec![Effect::SetMessage(format!("Save failed: {e}"))],
                    }
                }
            }
            Action::SaveForce => match self.save(ctx) {
                Ok(()) => {
                    let name = self.filename().to_string();
                    vec![Effect::SetMessage(format!("Saved {name}."))]
                }
                Err(e) => vec![Effect::SetMessage(format!("Save failed: {e}"))],
            },
            Action::Tick => self.handle_tick(ctx),
            Action::SetMark => {
                self.set_mark();
                vec![Effect::SetMessage("Mark set".into())]
            }
            Action::KillRegion => {
                if let Some(text) = self.kill_region() {
                    ctx.clipboard.set_text(&text);
                    vec![]
                } else {
                    vec![Effect::SetMessage("No region".into())]
                }
            }
            Action::Yank => {
                if let Some(text) = ctx.clipboard.get_text() {
                    self.clear_mark();
                    self.yank_text(&text);
                }
                vec![]
            }
            Action::OpenFileSearch => {
                let selected_text = self.selected_text();
                self.clear_mark();
                vec![Effect::Emit(Event::FileSearchOpened { selected_text })]
            }
            Action::Abort => {
                self.clear_mark();
                vec![]
            }
            _ => vec![],
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> Vec<Effect> {
        match event {
            Event::Resume => {
                self.handle_notification(ctx);
            }
            Event::GoToPosition { path, row, col } => {
                if self.path.as_deref() == Some(path.as_path()) {
                    self.cursor_row = (*row).min(self.line_count().saturating_sub(1));
                    self.cursor_col = (*col).min(self.line_len(self.cursor_row));
                    self.clear_mark();
                }
            }
            Event::PreviewFile { path, row, col, match_len } => {
                if self.path.as_deref() == Some(path.as_path()) {
                    let r = (*row).min(self.line_count().saturating_sub(1));
                    self.cursor_row = r;
                    self.cursor_col = (*col).min(self.line_len(r));
                    self.scroll_offset = r.saturating_sub(ctx.viewport_height / 2);
                    self.highlight_match(*row, *col, *match_len);
                    return vec![Effect::ActivateBuffer(path.clone())];
                }
            }
            Event::ConfirmSearch { path, row, col } => {
                if self.path.as_deref() == Some(path.as_path()) {
                    self.preview = false;
                    let r = (*row).min(self.line_count().saturating_sub(1));
                    self.cursor_row = r;
                    self.cursor_col = (*col).min(self.line_len(r));
                    self.clear_mark();
                    return vec![
                        Effect::ActivateBuffer(path.clone()),
                        Effect::FocusPanel(PanelSlot::Main),
                    ];
                }
            }
            _ => {}
        }
        vec![]
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, ctx: &DrawContext) {
        let height = area.height as usize;
        let gutter_width: usize = 2;
        let text_width = (area.width as usize).saturating_sub(gutter_width);
        self.text_width = text_width;

        self.adjust_scroll(text_width, height);

        let total_lines = self.line_count();
        let gutter_style = ctx.theme.get("editor.gutter").to_style();
        let text_style = ctx.theme.get("editor.text").to_style();
        let sel_style = ctx.theme.get("editor.selection").to_style();
        let sel_range = self.selection_range();

        let mut display_lines: Vec<Line> = Vec::with_capacity(height);
        let mut cursor_pos: Option<(u16, u16)> = None;
        let mut screen_row: usize = 0;
        let mut line_idx = self.scroll_offset;

        while screen_row < height && line_idx < total_lines {
            let raw = self.line(line_idx);
            let (display, char_map) = expand_tabs(&raw);
            let chunks = compute_chunks(display.len(), text_width);

            // Selection display-column range for this line
            let sel_dcols = match sel_range {
                Some(((sr, sc), (er, ec))) if line_idx >= sr && line_idx <= er => {
                    let sd = if line_idx == sr {
                        char_map.get(sc).copied().unwrap_or(display.len())
                    } else {
                        0
                    };
                    let ed = if line_idx == er {
                        char_map.get(ec).copied().unwrap_or(display.len())
                    } else {
                        display.len()
                    };
                    Some((sd, ed))
                }
                _ => None,
            };

            // Cursor display column
            let cursor_dcol = if line_idx == self.cursor_row {
                Some(
                    char_map
                        .get(self.cursor_col)
                        .copied()
                        .unwrap_or_else(|| char_map.last().copied().unwrap_or(0)),
                )
            } else {
                None
            };

            // Skip sub-lines for partial-line scroll on the first visible line
            let skip = if line_idx == self.scroll_offset { self.scroll_sub_line } else { 0 };

            for (chunk_i, &(cs, ce)) in chunks.iter().enumerate() {
                if chunk_i < skip {
                    continue;
                }
                if screen_row >= height {
                    break;
                }
                let is_last = chunk_i == chunks.len() - 1;
                let chunk_text = &display[cs..ce];
                let mut spans: Vec<Span> = Vec::new();

                // Gutter
                spans.push(Span::styled("  ", gutter_style));

                // Content with optional selection highlighting
                if let Some((ss, se)) = sel_dcols {
                    let rel_s = ss.clamp(cs, ce) - cs;
                    let rel_e = se.clamp(cs, ce) - cs;

                    if rel_e > rel_s {
                        if rel_s > 0 {
                            spans.push(Span::styled(
                                chars_to_string(&chunk_text[..rel_s]),
                                text_style,
                            ));
                        }
                        spans.push(Span::styled(
                            chars_to_string(&chunk_text[rel_s..rel_e]),
                            sel_style,
                        ));
                        if rel_e < chunk_text.len() {
                            spans.push(Span::styled(
                                chars_to_string(&chunk_text[rel_e..]),
                                text_style,
                            ));
                        }
                    } else if !chunk_text.is_empty() {
                        spans.push(Span::styled(chars_to_string(chunk_text), text_style));
                    }

                    // Pad selection to line edge on last chunk when selection continues
                    if is_last {
                        if let Some(((_, _), (er, _))) = sel_range {
                            if line_idx < er {
                                let content_len = ce - cs;
                                let pad = text_width.saturating_sub(content_len);
                                if pad > 0 {
                                    spans.push(Span::styled(" ".repeat(pad), sel_style));
                                }
                            }
                        }
                    }
                } else if !chunk_text.is_empty() {
                    spans.push(Span::styled(chars_to_string(chunk_text), text_style));
                }

                // Continuation indicator
                if !is_last {
                    spans.push(Span::styled("\\", gutter_style));
                }

                // Track cursor screen position
                if let Some(dc) = cursor_dcol {
                    if (dc >= cs && dc < ce) || (is_last && dc >= cs) {
                        let cx = gutter_width as u16 + (dc - cs) as u16;
                        cursor_pos = Some((area.x + cx, area.y + screen_row as u16));
                    }
                }

                display_lines.push(Line::from(spans));
                screen_row += 1;
            }

            line_idx += 1;
        }

        // Fill remaining rows with ~
        while screen_row < height {
            display_lines.push(Line::from(vec![Span::styled("~ ", gutter_style)]));
            screen_row += 1;
        }

        let paragraph = Paragraph::new(display_lines).style(text_style);
        frame.render_widget(paragraph, area);

        self.cursor_screen_pos = cursor_pos;
    }

    fn cursor_position(&self) -> Option<(usize, usize)> {
        Some((self.cursor_row, self.cursor_col))
    }

    fn cursor_screen_pos(&self) -> Option<(u16, u16)> {
        self.cursor_screen_pos
    }

    fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    fn set_scroll_offset(&mut self, offset: usize) {
        self.scroll_offset = offset;
    }

    fn status_info(&self) -> Option<(&str, usize, usize)> {
        Some((self.filename(), self.cursor_row + 1, self.cursor_col + 1))
    }

    fn save_session(&self, _ctx: &Context) {}

    fn restore_session(&mut self, ctx: &mut Context) {
        let Some(conn) = ctx.db else { return };
        let Some(ref path) = self.path else { return };
        let root_str = ctx.root.to_string_lossy();
        let file_str = path.to_string_lossy();

        let row: Option<(i64, Option<i64>, i32, String)> = conn
            .query_row(
                "SELECT content_hash, undo_cursor, distance_from_save, chain_id
                 FROM buffer_undo_state WHERE root_path = ?1 AND file_path = ?2",
                rusqlite::params![root_str, file_str],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .ok();

        let Some((stored_hash, undo_cursor_raw, distance_from_save, chain_id)) = row else {
            return;
        };

        if self.content_hash() != stored_hash as u64 {
            return;
        }

        let loaded = Self::load_entries_after(conn, &root_str, &file_str, 0);
        if loaded.is_empty() {
            return;
        }

        let max_seq = loaded.last().unwrap().0;
        let entries: Vec<UndoEntry> = loaded.into_iter().map(|(_, e)| e).collect();

        self.chain_id = Some(chain_id);
        self.last_seen_seq = max_seq;

        self.restore_undo(
            entries,
            undo_cursor_raw.map(|v| v as usize),
            distance_from_save,
        );
    }

    fn needs_flush(&self) -> bool {
        self.has_unpersisted_undo()
    }

    fn flush(&mut self, ctx: &mut Context) {
        self.flush_undo_to_db(ctx);
    }

    fn notify_hash(&self) -> Option<String> {
        self.path.as_ref().map(|p| Self::notify_hash_for_path(p))
    }

}

// ---------------------------------------------------------------------------
// BufferFactory
// ---------------------------------------------------------------------------

pub struct BufferFactory {
    preview_path: Option<PathBuf>,
}

impl BufferFactory {
    pub fn new() -> Self {
        Self { preview_path: None }
    }
}

impl Component for BufferFactory {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }

    fn panel_claims(&self) -> &[PanelClaim] {
        &[]
    }

    fn handle_action(&mut self, _action: Action, _ctx: &mut Context) -> Vec<Effect> {
        vec![]
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> Vec<Effect> {
        let mut effects = Vec::new();
        match event {
            Event::OpenFile(path) => {
                let path_str = path.to_string_lossy();
                match Buffer::from_file_with_waker(&path_str, ctx.waker.clone()) {
                    Ok(buf) => effects.push(Effect::Spawn(Box::new(buf))),
                    Err(e) => effects.push(Effect::SetMessage(format!("Open failed: {e}"))),
                }
            }
            Event::PreviewFile { path, row, col, match_len } => {
                if self.preview_path.as_ref() == Some(path) {
                    return effects; // existing preview buffer handles repositioning
                }
                if self.preview_path.is_some() {
                    effects.push(Effect::KillPreview);
                }
                let path_str = path.to_string_lossy();
                match Buffer::from_file_with_waker(&path_str, ctx.waker.clone()) {
                    Ok(mut buf) => {
                        buf.preview = true;
                        let r = (*row).min(buf.line_count().saturating_sub(1));
                        buf.cursor_row = r;
                        buf.cursor_col = (*col).min(buf.line_len(r));
                        buf.scroll_offset = r.saturating_sub(ctx.viewport_height / 2);
                        buf.highlight_match(*row, *col, *match_len);
                        self.preview_path = Some(path.clone());
                        effects.push(Effect::Spawn(Box::new(buf)));
                    }
                    Err(e) => effects.push(Effect::SetMessage(format!("Preview failed: {e}"))),
                }
            }
            Event::PreviewClosed => {
                if self.preview_path.take().is_some() {
                    effects.push(Effect::KillPreview);
                }
            }
            Event::ConfirmSearch { path, row, col } => {
                if self.preview_path.as_ref() == Some(path) {
                    self.preview_path = None;
                    // Preview buffer promotes itself via Buffer.handle_event
                } else {
                    // No preview for this path — ensure buffer exists
                    self.preview_path = None;
                    effects.push(Effect::Emit(Event::OpenFile(path.clone())));
                    effects.push(Effect::Emit(Event::GoToPosition {
                        path: path.clone(), row: *row, col: *col,
                    }));
                    effects.push(Effect::FocusPanel(PanelSlot::Main));
                }
            }
            _ => {}
        }
        effects
    }

    fn draw(&mut self, _frame: &mut Frame, _area: Rect, _ctx: &DrawContext) {}

    fn save_session(&self, _ctx: &Context) {}

    fn restore_session(&mut self, _ctx: &mut Context) {}

    fn default_theme_toml(&self) -> &'static str {
        r#"
[editor]
text      = "$normal"
gutter    = "$muted"
selection = "$selected"
"#
    }
}
