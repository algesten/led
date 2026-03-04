use std::fs::File;
use std::hash::Hasher;
use std::io::{self, BufReader, BufWriter};
use std::path::PathBuf;
use std::time::Instant;

use led_core::{
    Action, Component, Context, DrawContext, Effect, Event, PanelClaim, PanelSlot, TabDescriptor,
};
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
    distance_from_save: i32,
    save_history_len: usize,
    persisted_undo_len: usize,
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
            distance_from_save: 0,
            save_history_len: 0,
            persisted_undo_len: 0,
        }
    }

    pub fn from_file(path: &str) -> io::Result<Self> {
        let file = File::open(path)?;
        let rope = Rope::from_reader(BufReader::new(file))?;
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
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
        })
    }

    pub fn save(&mut self) -> io::Result<()> {
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

    pub fn kill_line(&mut self) {
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
                    text,
                },
                cursor_before,
                cursor_after,
                direction: 1,
            });
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

    // --- Undo persistence ---

    pub fn content_hash(&self) -> u64 {
        let mut hasher = XxHash64::with_seed(0);
        for chunk in self.rope.chunks() {
            hasher.write(chunk.as_bytes());
        }
        hasher.finish()
    }

    pub fn has_unpersisted_undo(&self) -> bool {
        self.pending_group.is_some() || self.undo_history.len() > self.persisted_undo_len
    }

    pub fn drain_unpersisted_undo(&mut self) -> Vec<(usize, Vec<u8>)> {
        self.flush_pending();
        let start = self.persisted_undo_len;
        let mut result = Vec::new();
        for (i, entry) in self.undo_history[start..].iter().enumerate() {
            let bytes = rmp_serde::to_vec(entry).expect("serialize undo entry");
            result.push((start + i, bytes));
        }
        self.persisted_undo_len = self.undo_history.len();
        result
    }

    pub fn undo_metadata(&self) -> (Option<usize>, i32) {
        (self.undo_cursor, self.distance_from_save)
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

    /// Whether this buffer was saved (and undo should be cleared from DB).
    /// Returns the path if save happened, then clears the flag.
    pub fn take_saved_path(&mut self) -> Option<PathBuf> {
        // We track this via save_history_len matching persisted_undo_len after save
        None // Shell tracks saved_paths separately
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
        })
    }

    fn handle_action(&mut self, action: Action, ctx: &mut Context) -> Vec<Effect> {
        match action {
            Action::InsertChar(c) => {
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
                self.insert_newline();
                vec![]
            }
            Action::DeleteBackward => {
                self.delete_char_backward();
                vec![]
            }
            Action::DeleteForward => {
                self.delete_char_forward();
                vec![]
            }
            Action::InsertTab => {
                self.insert_char('\t');
                vec![]
            }
            Action::KillLine => {
                self.kill_line();
                vec![]
            }
            Action::Undo => {
                self.undo();
                vec![]
            }
            Action::Save => match self.save() {
                Ok(()) => {
                    let name = self.filename().to_string();
                    let mut effects = vec![Effect::SetMessage(format!("Saved {name}."))];
                    if let Some(ref path) = self.path {
                        effects.push(Effect::SavedFile(path.clone()));
                    }
                    effects
                }
                Err(e) => vec![Effect::SetMessage(format!("Save failed: {e}"))],
            },
            _ => vec![],
        }
    }

    fn handle_event(&mut self, _event: &Event, _ctx: &mut Context) -> Vec<Effect> {
        vec![]
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, ctx: &DrawContext) {
        let height = area.height as usize;
        let total_lines = self.line_count();
        let gutter_style = ctx.theme.get("editor.gutter").to_style();
        let text_style = ctx.theme.get("editor.text").to_style();

        let mut display_lines = Vec::with_capacity(height);

        for i in 0..height {
            let line_idx = self.scroll_offset + i;
            if line_idx < total_lines {
                let gutter = Span::styled("  ", gutter_style);
                let text =
                    Span::styled(self.line(line_idx).replace('\t', "    "), text_style);
                display_lines.push(Line::from(vec![gutter, text]));
            } else {
                let gutter = Span::styled("~ ", gutter_style);
                display_lines.push(Line::from(vec![gutter]));
            }
        }

        let paragraph = Paragraph::new(display_lines).style(text_style);
        frame.render_widget(paragraph, area);
    }

    fn cursor_position(&self) -> Option<(usize, usize)> {
        Some((self.cursor_row, self.cursor_col))
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

    fn save_session(&self, _ctx: &Context) {
        // Session persistence handled by shell
    }

    fn restore_session(&mut self, _ctx: &mut Context) {
        // Session persistence handled by shell
    }

    fn needs_flush(&self) -> bool {
        self.has_unpersisted_undo()
    }

    fn flush(&mut self, _ctx: &mut Context) {
        self.flush_pending();
    }

}

// ---------------------------------------------------------------------------
// BufferFactory
// ---------------------------------------------------------------------------

pub struct BufferFactory;

impl Component for BufferFactory {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }

    fn panel_claims(&self) -> &[PanelClaim] {
        &[]
    }

    fn handle_action(&mut self, _action: Action, _ctx: &mut Context) -> Vec<Effect> {
        vec![]
    }

    fn handle_event(&mut self, event: &Event, _ctx: &mut Context) -> Vec<Effect> {
        match event {
            Event::OpenFile(path) => {
                let path_str = path.to_string_lossy();
                match Buffer::from_file(&path_str) {
                    Ok(buf) => vec![Effect::Spawn(Box::new(buf))],
                    Err(e) => vec![Effect::SetMessage(format!("Open failed: {e}"))],
                }
            }
            _ => vec![],
        }
    }

    fn draw(&mut self, _frame: &mut Frame, _area: Rect, _ctx: &DrawContext) {}

    fn save_session(&self, _ctx: &Context) {}

    fn restore_session(&mut self, _ctx: &mut Context) {}

    fn default_theme_toml(&self) -> &'static str {
        r#"
[editor]
text   = "$normal"
gutter = "$muted"
"#
    }
}
