use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use led_config_file::ConfigFile;
use led_core::keys::{Keymap, Keys};
use led_core::theme::Theme;
use led_core::{
    ChangeSeq, CharOffset, Col, ContentHash, Doc, DocId, DocVersion, EditOp, InertDoc, PanelSlot,
    Row, Startup, SubLine, TabOrder, UndoHistory, Versioned,
};
pub use led_workspace::Workspace;
pub use led_workspace::{SessionBuffer, SessionRestorePhase};

pub mod file_search;

// ── BufferStatus ──

/// Per-file annotations that must stay in sync with content.
/// Lives inside `BufferState` for open buffers, in a global map for unopened files.
#[derive(Debug, Clone, Default)]
pub struct BufferStatus {
    diagnostics: Vec<led_lsp::Diagnostic>,
    inlay_hints: Vec<led_lsp::InlayHint>,
    git_line_statuses: Vec<led_core::git::LineStatus>,
}

impl BufferStatus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn diagnostics(&self) -> &[led_lsp::Diagnostic] {
        &self.diagnostics
    }

    pub fn inlay_hints(&self) -> &[led_lsp::InlayHint] {
        &self.inlay_hints
    }

    pub fn git_line_statuses(&self) -> &[led_core::git::LineStatus] {
        &self.git_line_statuses
    }

    fn set_diagnostics(&mut self, diags: Vec<led_lsp::Diagnostic>) {
        self.diagnostics = diags;
    }

    fn set_inlay_hints(&mut self, hints: Vec<led_lsp::InlayHint>) {
        self.inlay_hints = hints;
    }

    fn clear_inlay_hints(&mut self) {
        self.inlay_hints.clear();
    }

    fn set_git_line_statuses(&mut self, statuses: Vec<led_core::git::LineStatus>) {
        self.git_line_statuses = statuses;
    }

    /// Shift line positions after a structural edit.
    fn shift_lines(&mut self, edit_row: usize, delta: isize) {
        self.diagnostics.retain_mut(|d| {
            // Remove diagnostics on deleted lines
            if delta < 0 {
                let deleted_start = edit_row + 1;
                let deleted_end = edit_row + (-delta) as usize;
                if d.start_row >= deleted_start && d.end_row <= deleted_end {
                    return false;
                }
            }
            // Shift diagnostics below the edit
            if d.start_row > edit_row {
                d.start_row = (d.start_row as isize + delta).max(0) as usize;
            }
            if d.end_row > edit_row {
                d.end_row = (d.end_row as isize + delta).max(0) as usize;
            }
            true
        });
        // Git line statuses become stale on structural edits — clear them.
        // The driver will re-fetch after the next save.
        self.git_line_statuses.clear();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreviewRequest {
    pub path: PathBuf,
    pub row: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JumpPosition {
    pub path: PathBuf,
    pub row: usize,
    pub col: usize,
    pub scroll_offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dimensions {
    // ── Inputs ──
    pub viewport_width: u16,
    pub viewport_height: u16,
    pub show_side_panel: bool,

    // ── Configurable base values ──
    pub side_panel_width: u16,
    pub min_editor_width: u16,
    pub status_bar_height: u16,
    pub tab_bar_height: u16,
    pub gutter_width: u16,
    pub scroll_margin: usize,
    pub tab_stop: usize,
    pub ruler_column: Option<usize>,
}

impl Dimensions {
    pub fn new(viewport_width: u16, viewport_height: u16, show_side_panel: bool) -> Self {
        Self {
            viewport_width,
            viewport_height,
            show_side_panel,
            side_panel_width: 25,
            min_editor_width: 25,
            status_bar_height: 1,
            tab_bar_height: 1,
            gutter_width: 2,
            scroll_margin: 3,
            tab_stop: 4,
            ruler_column: Some(110),
        }
    }

    /// Does the side panel fit?
    pub fn side_panel_visible(&self) -> bool {
        self.show_side_panel && self.viewport_width > self.side_panel_width + self.min_editor_width
    }

    /// Actual side panel width (0 if hidden)
    pub fn side_width(&self) -> u16 {
        if self.side_panel_visible() {
            self.side_panel_width
        } else {
            0
        }
    }

    /// Width available for the editor area (everything right of side panel)
    pub fn editor_width(&self) -> u16 {
        self.viewport_width.saturating_sub(self.side_width())
    }

    /// Height of the buffer content area (viewport minus status bar and tab bar)
    pub fn buffer_height(&self) -> usize {
        (self.viewport_height as usize)
            .saturating_sub(self.status_bar_height as usize)
            .saturating_sub(self.tab_bar_height as usize)
    }

    /// Width of the text content area (editor minus gutter)
    pub fn text_width(&self) -> usize {
        (self.editor_width() as usize).saturating_sub(self.gutter_width as usize)
    }
}

#[derive(Debug, Clone)]
pub struct UndoFlush {
    pub file_path: PathBuf,
    pub chain_id: String,
    pub content_hash: u64,
    pub undo_cursor: usize,
    pub distance_from_save: i32,
    pub entries: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    Insert,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SaveState {
    #[default]
    Clean,
    Modified,
    Saving,
}

/// Why a buffer's content last changed.
///
/// Carried on every `Mut::BufferUpdate` so the compiler enforces that
/// callers always declare intent. Stored on `BufferState` by the reducer
/// so derived streams can branch on it (e.g. LSP didSave for external changes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChangeReason {
    /// Buffer just opened — initial content load from disk.
    #[default]
    Init,
    /// User edit, yank, undo, or cross-instance sync.
    Edit,
    /// Content reloaded from disk (e.g. external `git checkout .`).
    /// The file is already saved — no user-initiated save will follow.
    ExternalFileChange,
}

// ── Syntax highlight types ──

#[derive(Debug, Clone, PartialEq)]
pub struct HighlightSpan {
    pub char_start: usize,
    pub char_end: usize,
    pub capture_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BracketPair {
    pub open_line: usize,
    pub open_col: usize,
    pub close_line: usize,
    pub close_col: usize,
    pub color_index: Option<usize>,
}

impl BracketPair {
    /// Find the matching bracket for the cursor from cached pairs.
    pub fn find_match(
        pairs: &[BracketPair],
        cursor_row: usize,
        cursor_col: usize,
    ) -> Option<(usize, usize)> {
        for bp in pairs {
            if bp.open_line == cursor_row && bp.open_col == cursor_col {
                return Some((bp.close_line, bp.close_col));
            }
            if bp.close_line == cursor_row && bp.close_col == cursor_col {
                return Some((bp.open_line, bp.open_col));
            }
        }
        None
    }
}

// ── Incremental search state ──

#[derive(Debug, Clone)]
pub struct ISearchState {
    pub query: String,
    pub origin: (usize, usize),
    pub origin_scroll: usize,
    pub origin_sub_line: usize,
    pub failed: bool,
    pub matches: Vec<(usize, usize, usize)>, // (row, col, char_len)
    pub match_idx: Option<usize>,
}

/// What syntax work this buffer needs from the syntax driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyntaxRequest {
    /// Full reparse needed (materialization, undo, redo, reload, save).
    Full,
    /// Incremental update with captured edit ops.
    Partial { edit_ops: Vec<EditOp> },
}

/// Whether a buffer's content is loaded from disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaterializationState {
    /// Path known, no content loaded. Doc is InertDoc.
    NotMaterialized,
    /// Open requested to docstore, waiting for response.
    Requested,
    /// Content loaded from disk. Doc is TextDoc.
    Materialized,
}

#[derive(Clone)]
pub struct BufferState {
    // Identity
    doc_id: DocId,
    path: Option<PathBuf>,

    // Content (InertDoc for non-materialized buffers)
    doc: Arc<dyn Doc>,
    materialization: MaterializationState,

    // Editing state (owned by buffer, not doc)
    version: DocVersion,
    undo: UndoHistory,

    // Cursor
    cursor_row: Row,
    cursor_col: Col,
    cursor_col_affinity: Col,
    mark: Option<(Row, Col)>,

    // Scroll
    scroll_row: Row,
    scroll_sub_line: SubLine,

    // Edit tracking
    last_edit_kind: Option<EditKind>,
    save_state: SaveState,

    // Undo persistence
    persisted_undo_len: usize,
    chain_id: Option<String>,
    last_seen_seq: i64,
    content_hash: ContentHash,
    change_seq: ChangeSeq,
    change_reason: ChangeReason,

    // Incremental search
    pub isearch: Option<ISearchState>,
    pub last_search: Option<String>,

    // Syntax highlighting
    pending_syntax_request: Option<SyntaxRequest>,
    pending_syntax_seq: u64,
    syntax_highlights: Rc<Vec<(usize, HighlightSpan)>>,
    bracket_pairs: Rc<Vec<BracketPair>>,
    matching_bracket: Option<(usize, usize)>,

    // Indent
    pending_indent_row: Option<usize>,
    pending_tab_fallback: bool,
    reindent_chars: Arc<[char]>,

    // LSP
    completion_triggers: Vec<String>,

    // UI
    tab_order: TabOrder,
    is_preview: bool,
    last_used: Instant,

    // Per-file annotations (diagnostics, inlay hints, git line status)
    status: BufferStatus,

    // Guards against double-shifting annotations when edit() and an outer
    // helper both call sync_annotations for the same doc transition.
    annotations_synced_ver: DocVersion,
}

impl BufferState {
    /// Create an unmaterialized buffer for the given path.
    /// Content is loaded later via `materialize()`.
    pub fn new(path: PathBuf) -> Self {
        Self {
            doc_id: DocId(0),
            doc: Arc::new(InertDoc),
            materialization: MaterializationState::NotMaterialized,
            version: DocVersion(0),
            undo: UndoHistory::default(),
            path: Some(path),
            cursor_row: Row(0),
            cursor_col: Col(0),
            cursor_col_affinity: Col(0),
            scroll_row: Row(0),
            scroll_sub_line: SubLine(0),
            tab_order: TabOrder(0),
            mark: None,
            last_edit_kind: None,
            save_state: SaveState::Clean,
            persisted_undo_len: 0,
            chain_id: None,
            last_seen_seq: 0,
            content_hash: ContentHash(0),
            change_seq: ChangeSeq(0),
            change_reason: ChangeReason::Init,
            isearch: None,
            last_search: None,
            pending_syntax_request: None,
            pending_syntax_seq: 0,
            syntax_highlights: Rc::new(Vec::new()),
            bracket_pairs: Rc::new(Vec::new()),
            matching_bracket: None,
            pending_indent_row: None,
            pending_tab_fallback: false,
            reindent_chars: Arc::from([]),
            completion_triggers: Vec::new(),
            is_preview: false,
            last_used: Instant::now(),
            status: BufferStatus::new(),
            annotations_synced_ver: DocVersion(0),
        }
    }

    // ── Identity ──

    pub fn doc_id(&self) -> DocId {
        self.doc_id
    }
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn path_buf(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }
    pub fn is_preview(&self) -> bool {
        self.is_preview
    }
    pub fn set_preview(&mut self, preview: bool) {
        self.is_preview = preview;
    }

    // ── Editing state ──

    /// Monotonic version counter. Incremented on every content edit, undo, redo.
    pub fn version(&self) -> DocVersion {
        self.version
    }

    /// Whether the buffer has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.undo.has_pending() || self.undo.distance_from_save() != 0
    }

    /// Access the undo history (read-only, for persistence).
    pub fn undo_history(&self) -> &UndoHistory {
        &self.undo
    }

    /// Number of committed undo entries (excludes pending).
    pub fn undo_history_len(&self) -> usize {
        self.undo.entry_count()
    }

    /// Return the edit ops accumulated in the current (unflushed) pending group.
    pub fn pending_edit_ops(&self) -> Vec<EditOp> {
        self.undo.pending_edit_ops()
    }

    // ── Document ──

    /// Whether this buffer has materialized content.
    pub fn is_materialized(&self) -> bool {
        self.materialization == MaterializationState::Materialized
    }

    pub fn materialization(&self) -> MaterializationState {
        self.materialization
    }

    /// Get the document (InertDoc for non-materialized buffers).
    pub fn doc(&self) -> &Arc<dyn Doc> {
        &self.doc
    }

    /// Load or replace the document content.
    ///
    /// `clear_annotations`: when true, clears syntax highlights,
    /// brackets, and diagnostics (use for reloads where content
    /// changed externally).  When false, preserves annotations
    /// (use for first materialization — diagnostics may have
    /// arrived while the buffer was unmaterialized).
    pub fn materialize(&mut self, doc_id: DocId, doc: Arc<dyn Doc>, clear_annotations: bool) {
        self.doc_id = doc_id;
        self.content_hash = doc.content_hash();
        self.doc = doc;
        self.materialization = MaterializationState::Materialized;
        if clear_annotations {
            self.syntax_highlights = Rc::new(Vec::new());
            self.bracket_pairs = Rc::new(Vec::new());
            self.matching_bracket = None;
            self.status = BufferStatus::new();
        }
        self.set_syntax_full();
    }

    /// Dematerialize: replace doc with InertDoc, discard undo.
    pub fn dematerialize(&mut self) {
        self.doc = Arc::new(InertDoc);
        self.undo = UndoHistory::default();
        self.materialization = MaterializationState::NotMaterialized;
        self.pending_syntax_request = None;
    }

    /// Mark that materialization has been requested.
    pub fn mark_materialization_requested(&mut self) {
        self.materialization = MaterializationState::Requested;
    }

    // ── Syntax request ──

    pub fn pending_syntax_request(&self) -> Option<&SyntaxRequest> {
        self.pending_syntax_request.as_ref()
    }

    pub fn pending_syntax_seq(&self) -> u64 {
        self.pending_syntax_seq
    }

    fn set_syntax_full(&mut self) {
        self.pending_syntax_request = Some(SyntaxRequest::Full);
        self.pending_syntax_seq += 1;
    }

    fn append_syntax_edit(&mut self, op: EditOp) {
        match &mut self.pending_syntax_request {
            Some(SyntaxRequest::Full) => {
                // Full subsumes Partial — keep Full but bump seq
                // so derived re-fires with the updated doc.
                self.pending_syntax_seq += 1;
            }
            Some(SyntaxRequest::Partial { edit_ops }) => {
                edit_ops.push(op);
                self.pending_syntax_seq += 1;
            }
            None => {
                self.pending_syntax_request = Some(SyntaxRequest::Partial { edit_ops: vec![op] });
                self.pending_syntax_seq += 1;
            }
        }
    }

    /// Edit the document at the cursor row. Shifts annotations and marks
    /// modified automatically. The closure returns `(new_doc, R)` where `R`
    /// is forwarded to the caller (typically cursor position info).
    pub fn edit<R>(&mut self, f: impl FnOnce(&Arc<dyn Doc>) -> (Arc<dyn Doc>, R)) -> R {
        let doc = &self.doc;
        let old_lines = doc.line_count();
        let old_ver = self.version;
        let edit_row = *self.cursor_row;
        let (new_doc, result) = f(doc);
        self.version = self.version + 1;
        self.doc = new_doc;
        self.sync_annotations(edit_row, old_lines, old_ver);
        result
    }

    /// Edit the document at a specific row. Shifts annotations and marks
    /// modified automatically. The closure returns `(new_doc, edit_ops, R)`.
    pub fn edit_at<R>(
        &mut self,
        edit_row: usize,
        f: impl FnOnce(&Arc<dyn Doc>) -> (Arc<dyn Doc>, Vec<EditOp>, R),
    ) -> R {
        let doc = &self.doc;
        let old_lines = doc.line_count();
        let old_ver = self.version;
        let (new_doc, ops, result) = f(doc);
        self.version = self.version + 1;
        self.doc = new_doc;
        for op in ops {
            self.append_syntax_edit(op);
        }
        self.sync_annotations(edit_row, old_lines, old_ver);
        result
    }

    // ── Primitive text mutations ──

    /// Insert text at a character offset. Records undo op, shifts annotations.
    pub fn insert_text(&mut self, char_idx: CharOffset, text: &str) {
        let edit_row = *self.cursor_row;
        let op = EditOp {
            offset: char_idx,
            old_text: String::new(),
            new_text: text.to_string(),
        };
        self.undo.push_op(op.clone(), char_idx);
        self.edit_at(edit_row, |doc| {
            let new_doc = doc.insert(char_idx, text);
            (new_doc, vec![op], ())
        });
        self.last_edit_kind = Some(EditKind::Insert);
        self.touch();
    }

    /// Remove text between two character offsets. Records undo op, shifts annotations.
    pub fn remove_text(&mut self, start: CharOffset, end: CharOffset) {
        let edit_row = *self.cursor_row;
        let old_text = self.doc.slice(start, end);
        let op = EditOp {
            offset: start,
            old_text,
            new_text: String::new(),
        };
        self.undo.push_op(op.clone(), start);
        self.edit_at(edit_row, |doc| {
            let new_doc = doc.remove(start, end);
            (new_doc, vec![op], ())
        });
        self.last_edit_kind = Some(EditKind::Delete);
        self.touch();
    }

    // ── Save lifecycle ──

    /// Save completed: docstore confirmed save. Updates doc (undo state
    /// Apply pre-save cleanup: strip trailing whitespace, ensure final newline.
    /// Edits are recorded in undo so they can be undone.
    pub fn apply_save_cleanup(&mut self) {
        // Collect edits first to avoid borrow conflict
        let doc = &self.doc;
        let line_count = doc.line_count();
        let mut removals: Vec<(CharOffset, CharOffset)> = Vec::new();

        for line_idx in (0..line_count).rev() {
            let row = Row(line_idx);
            let line = doc.line(row);
            let trimmed = line.trim_end();
            if trimmed.len() < line.len() {
                let line_start = doc.line_to_char(row).0;
                let start = CharOffset(line_start + trimmed.chars().count());
                let end = CharOffset(line_start + line.chars().count());
                removals.push((start, end));
            }
        }

        let needs_final_newline = {
            let last_row = Row(doc.line_count().saturating_sub(1));
            let last = doc.line(last_row);
            !last.is_empty()
        };

        // Apply removals (already in reverse order)
        for (start, end) in removals {
            self.remove_text(start, end);
        }

        // Ensure final newline
        if needs_final_newline {
            let doc = &self.doc;
            let last_row = Row(doc.line_count().saturating_sub(1));
            let len = doc.line_to_char(last_row).0 + doc.line_len(last_row);
            self.insert_text(CharOffset(len), "\n");
        }
    }

    /// Save completed: docstore confirmed save. Updates doc (undo state
    /// flushed), marks clean, resets persistence, bumps change seq.
    /// Preserves annotations (content unchanged).
    pub fn save_completed(&mut self, doc: Arc<dyn Doc>) {
        self.doc = doc;
        self.save_state = SaveState::Clean;
        self.undo.flush_pending();
        self.undo.reset_distance_from_save();
        self.persisted_undo_len = self.undo.entry_count();
        self.chain_id = None;
        self.last_seen_seq = 0;
        self.content_hash = self.doc().content_hash();
        self.change_seq = ChangeSeq(led_core::next_change_seq());
        self.set_syntax_full();
    }

    /// Save-as completed: docstore confirmed save to new path.
    /// Same as save_completed but also updates path.
    pub fn save_as_completed(&mut self, doc: Arc<dyn Doc>, new_path: PathBuf) {
        self.path = Some(new_path);
        self.save_completed(doc);
    }

    // ── External changes ──

    /// File changed on disk with different content. Replaces doc,
    /// clears annotations, clamps cursor.
    pub fn reload_from_disk(&mut self, doc: Arc<dyn Doc>) {
        self.materialize(self.doc_id, doc, true);
        self.change_seq = ChangeSeq(led_core::next_change_seq());
        // Clamp cursor to new document bounds
        let max_row = Row(self.doc().line_count().saturating_sub(1));
        self.cursor_row = self.cursor_row.min(max_row);
        let max_col = Col(self.doc().line_len(self.cursor_row));
        self.cursor_col = self.cursor_col.min(max_col);
        self.cursor_col_affinity = self.cursor_col;
        self.close_group_on_move();
    }

    /// File was saved externally (detected by sync). Marks doc clean
    /// without changing content. Preserves annotations.
    pub fn mark_externally_saved(&mut self) {
        self.last_seen_seq = 0;
        self.chain_id = None;
        self.persisted_undo_len = self.undo.entry_count();
        self.change_seq = ChangeSeq(led_core::next_change_seq());
        if self.is_dirty() && self.save_state == SaveState::Clean {
            self.undo.reset_distance_from_save();
        }
    }

    // ── Workspace sync ──

    /// Apply a single remote undo entry: update doc content and record in undo history.
    pub fn apply_remote_entry(&mut self, doc: Arc<dyn Doc>, entry: led_core::UndoEntry) {
        self.doc = doc;
        self.undo.push_remote_entry(entry);
        self.version = self.version + 1;
    }

    /// Replay remote undo entries completed. Clears annotations, updates persistence.
    /// Call after applying entries via `apply_remote_entry`.
    pub fn apply_sync_replay(&mut self, last_seen_seq: i64) {
        // Annotations are stale after remote edits — clear them.
        self.syntax_highlights = Rc::new(Vec::new());
        self.bracket_pairs = Rc::new(Vec::new());
        self.matching_bracket = None;
        self.status = BufferStatus::new();
        self.last_seen_seq = last_seen_seq;
        self.persisted_undo_len = self.undo.entry_count();
        self.content_hash = self.doc().content_hash();
        self.change_seq = ChangeSeq(led_core::next_change_seq());
    }

    /// Reload from persisted state with new chain. Clears annotations, updates persistence.
    /// Call after applying entries via `apply_remote_entry`.
    pub fn apply_sync_reload(&mut self, chain_id: String, last_seen_seq: i64) {
        // Annotations are stale after remote edits — clear them.
        self.syntax_highlights = Rc::new(Vec::new());
        self.bracket_pairs = Rc::new(Vec::new());
        self.matching_bracket = None;
        self.status = BufferStatus::new();
        self.chain_id = Some(chain_id);
        self.last_seen_seq = last_seen_seq;
        self.persisted_undo_len = self.undo.entry_count();
        self.content_hash = self.doc().content_hash();
        self.change_seq = ChangeSeq(led_core::next_change_seq());
    }

    // ── Undo flush ──

    /// Undo entries flushed to workspace (pending confirmation).
    pub fn undo_flush_started(&mut self, chain_id: String, undo_cursor: usize) {
        self.chain_id = Some(chain_id);
        self.persisted_undo_len = undo_cursor;
        self.change_seq = ChangeSeq(led_core::next_change_seq());
    }

    /// Undo flush confirmed by workspace.
    pub fn undo_flush_confirmed(&mut self, chain_id: String, last_seen_seq: i64) {
        self.chain_id = Some(chain_id);
        self.last_seen_seq = last_seen_seq;
    }

    // ── Undo group management ──

    pub fn close_undo_group(&mut self) {
        self.undo.flush_pending();
    }

    pub fn begin_undo_group(&mut self, cursor_char: CharOffset) {
        self.undo.begin_group(cursor_char);
    }

    pub fn undo(&mut self) -> Option<CharOffset> {
        self.undo.flush_pending();

        if self.undo.entries_from(0).is_empty() {
            return None;
        }

        // Start undo chain if not already in one
        if self.undo.undo_cursor().is_none() {
            self.undo.start_undo_chain();
        }

        let cursor = self.undo.undo_cursor()?;
        if cursor == 0 {
            return None;
        }

        // Collect entries to undo (avoids borrow conflict)
        let entries = self.undo.entries_from(0);
        let mut pos = cursor - 1;
        let mut ops: Vec<(EditOp, i32, CharOffset, CharOffset)> = Vec::new(); // (inv_op, direction, inv_cb, inv_ca)
        loop {
            let entry = &entries[pos];
            let inv_op = EditOp {
                offset: entry.op.offset,
                old_text: entry.op.new_text.clone(),
                new_text: entry.op.old_text.clone(),
            };
            ops.push((
                inv_op,
                entry.direction,
                entry.cursor_after,
                entry.cursor_before,
            ));
            if entry.direction != 0 || pos == 0 {
                break;
            }
            pos -= 1;
        }

        // Apply collected inverse ops
        let mut doc = self.doc.clone();
        let mut restore_cursor = CharOffset(0);
        for (inv_op, direction, inv_cb, inv_ca) in ops {
            if !inv_op.old_text.is_empty() {
                let end = CharOffset(inv_op.offset.0 + inv_op.old_text.chars().count());
                doc = doc.remove(inv_op.offset, end);
            }
            if !inv_op.new_text.is_empty() {
                doc = doc.insert(inv_op.offset, &inv_op.new_text);
            }
            restore_cursor = inv_ca;
            self.undo.push_undo_inverse(
                led_core::UndoEntry {
                    op: inv_op,
                    cursor_before: inv_cb,
                    cursor_after: inv_ca,
                    direction: -direction,
                },
                direction,
            );
        }

        self.undo.set_undo_cursor(Some(pos));
        self.version = self.version + 1;
        self.doc = doc;
        self.set_syntax_full();
        Some(restore_cursor)
    }

    pub fn redo(&mut self) -> Option<CharOffset> {
        let (cursor, chain_base) = match (self.undo.undo_cursor(), self.undo.undo_chain_base()) {
            (Some(c), Some(b)) => (c, b),
            _ => return None,
        };
        if cursor >= chain_base {
            return None;
        }

        // Collect ops to replay (avoids borrow conflict)
        let entries = self.undo.entries_from(0);
        let mut ops: Vec<(EditOp, i32, CharOffset)> = Vec::new(); // (op, direction, cursor_after)
        let mut pos = cursor;
        loop {
            if pos >= chain_base {
                break;
            }
            let entry = &entries[pos];
            ops.push((entry.op.clone(), entry.direction, entry.cursor_after));
            pos += 1;
            if entry.direction != 0 && pos < chain_base {
                if entries[pos].direction != 0 {
                    break;
                }
            }
        }

        // Apply collected ops
        let mut doc = self.doc.clone();
        let mut restore_cursor = CharOffset(0);
        for (op, direction, cursor_after) in &ops {
            if !op.old_text.is_empty() {
                let end = CharOffset(op.offset.0 + op.old_text.chars().count());
                doc = doc.remove(op.offset, end);
            }
            if !op.new_text.is_empty() {
                doc = doc.insert(op.offset, &op.new_text);
            }
            self.undo.apply_redo_entry(*direction);
            restore_cursor = *cursor_after;
        }

        if pos >= chain_base {
            self.undo.set_undo_cursor(None);
        } else {
            self.undo.set_undo_cursor(Some(pos));
        }

        self.version = self.version + 1;
        self.doc = doc;
        self.set_syntax_full();
        Some(restore_cursor)
    }

    // ── Cursor ──

    pub fn cursor_row(&self) -> Row {
        self.cursor_row
    }
    pub fn cursor_col(&self) -> Col {
        self.cursor_col
    }
    pub fn cursor_col_affinity(&self) -> Col {
        self.cursor_col_affinity
    }
    pub fn set_cursor(&mut self, row: Row, col: Col, affinity: Col) {
        let max_row = Row(self.doc().line_count().saturating_sub(1));
        self.cursor_row = row.min(max_row);
        let max_col = Col(self.doc().line_len(self.cursor_row));
        self.cursor_col = col.min(max_col);
        self.cursor_col_affinity = affinity;
    }
    pub fn set_cursor_row(&mut self, row: Row) {
        let max_row = Row(self.doc().line_count().saturating_sub(1));
        self.cursor_row = row.min(max_row);
    }

    // ── Mark ──

    pub fn mark(&self) -> Option<(Row, Col)> {
        self.mark
    }
    pub fn set_mark(&mut self) {
        self.mark = Some((self.cursor_row, self.cursor_col));
    }
    pub fn set_mark_at(&mut self, row: Row, col: Col) {
        self.mark = Some((row, col));
    }
    pub fn clear_mark(&mut self) {
        self.mark = None;
    }

    // ── Scroll ──

    pub fn scroll_row(&self) -> Row {
        self.scroll_row
    }
    pub fn scroll_sub_line(&self) -> SubLine {
        self.scroll_sub_line
    }
    pub fn set_scroll(&mut self, row: Row, sub_line: SubLine) {
        self.scroll_row = row.min(Row(self.doc().line_count().saturating_sub(1)));
        self.scroll_sub_line = sub_line;
    }

    // ── Save state ──

    pub fn save_state(&self) -> SaveState {
        self.save_state
    }
    pub fn mark_saving(&mut self) {
        self.save_state = SaveState::Saving;
    }

    // ── Edit kind tracking ──

    pub fn last_edit_kind(&self) -> Option<EditKind> {
        self.last_edit_kind
    }
    pub fn close_group_on_move(&mut self) {
        if self.last_edit_kind.is_some() {
            self.close_undo_group();
            self.last_edit_kind = None;
        }
    }

    // ── Annotations (offer pattern) ──

    /// Accept diagnostics if they match the current document content.
    /// Unmaterialized buffers always accept — no content to go stale against.
    pub fn offer_diagnostics(
        &mut self,
        diags: Vec<led_lsp::Diagnostic>,
        content_hash: ContentHash,
    ) -> bool {
        if !self.is_materialized() {
            // Non-materialized buffer — always accept
            self.status.set_diagnostics(diags);
            return true;
        }
        let my_hash = self.doc.content_hash();
        if content_hash == my_hash {
            self.status.set_diagnostics(diags);
            true
        } else {
            log::debug!(
                "offer_diagnostics rejected: incoming={:?} buffer={:?} path={:?} n_diags={}",
                content_hash,
                my_hash,
                self.path,
                diags.len()
            );
            false
        }
    }

    /// Accept syntax highlights only if doc version matches.
    pub fn offer_syntax(
        &mut self,
        highlights: Vec<(usize, HighlightSpan)>,
        bracket_pairs: Vec<BracketPair>,
        version: DocVersion,
    ) -> bool {
        if self.version == version {
            self.syntax_highlights = Rc::new(highlights);
            self.bracket_pairs = Rc::new(bracket_pairs);
            self.update_matching_bracket();
            true
        } else {
            false
        }
    }

    pub fn set_inlay_hints(&mut self, hints: Vec<led_lsp::InlayHint>) {
        self.status.set_inlay_hints(hints);
    }
    pub fn clear_inlay_hints(&mut self) {
        self.status.clear_inlay_hints();
    }
    pub fn set_git_line_statuses(&mut self, statuses: Vec<led_core::git::LineStatus>) {
        self.status.set_git_line_statuses(statuses);
    }

    // ── Reading annotations (for display) ──

    pub fn syntax_highlights(&self) -> &Rc<Vec<(usize, HighlightSpan)>> {
        &self.syntax_highlights
    }
    pub fn bracket_pairs(&self) -> &Rc<Vec<BracketPair>> {
        &self.bracket_pairs
    }
    pub fn matching_bracket(&self) -> Option<(usize, usize)> {
        self.matching_bracket
    }
    pub fn update_matching_bracket(&mut self) {
        self.matching_bracket =
            BracketPair::find_match(&self.bracket_pairs, *self.cursor_row, *self.cursor_col);
    }
    pub fn status(&self) -> &BufferStatus {
        &self.status
    }

    // ── Persistence & sync (read-only for external code) ──

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }
    pub fn change_seq(&self) -> ChangeSeq {
        self.change_seq
    }
    pub fn change_reason(&self) -> ChangeReason {
        self.change_reason
    }
    pub fn set_change_reason(&mut self, reason: ChangeReason) {
        self.change_reason = reason;
    }
    pub fn chain_id(&self) -> Option<&str> {
        self.chain_id.as_deref()
    }
    pub fn last_seen_seq(&self) -> i64 {
        self.last_seen_seq
    }
    pub fn persisted_undo_len(&self) -> usize {
        self.persisted_undo_len
    }

    /// Restore session state during buffer open. Sets persistence fields
    /// that were saved from a previous session.
    pub fn restore_session(
        &mut self,
        persisted_undo_len: usize,
        chain_id: Option<String>,
        last_seen_seq: i64,
        content_hash: ContentHash,
        distance_from_save: i32,
    ) {
        self.persisted_undo_len = persisted_undo_len;
        self.chain_id = chain_id;
        self.last_seen_seq = last_seen_seq;
        self.content_hash = content_hash;
        self.undo.set_distance_from_save(distance_from_save);
    }

    // ── Tab management & activity ──

    pub fn tab_order(&self) -> TabOrder {
        self.tab_order
    }
    pub fn set_tab_order(&mut self, order: TabOrder) {
        self.tab_order = order;
    }
    pub fn last_used(&self) -> Instant {
        self.last_used
    }
    pub fn touch(&mut self) {
        self.last_used = Instant::now();
    }

    // ── LSP configuration ──

    pub fn completion_triggers(&self) -> &[String] {
        &self.completion_triggers
    }
    pub fn set_completion_triggers(&mut self, triggers: Vec<String>) {
        self.completion_triggers = triggers;
    }
    pub fn reindent_chars(&self) -> &[char] {
        &self.reindent_chars
    }
    pub fn set_reindent_chars(&mut self, chars: Arc<[char]>) {
        self.reindent_chars = chars;
    }
    pub fn pending_indent_row(&self) -> Option<usize> {
        self.pending_indent_row
    }
    pub fn pending_tab_fallback(&self) -> bool {
        self.pending_tab_fallback
    }

    /// Request auto-indent for a given row.  When `tab_fallback` is true,
    /// a literal tab-stop will be inserted if the language server returns
    /// no indent change.
    pub fn request_indent(&mut self, row: Option<usize>, tab_fallback: bool) {
        self.pending_indent_row = row;
        self.pending_tab_fallback = tab_fallback;
        if row.is_some() {
            // Ensure a syntax cycle fires so the driver can compute
            // the indent.  Bump seq even if a request already exists
            // so derived re-evaluates with the new indent_row.
            if self.pending_syntax_request.is_none() {
                self.pending_syntax_request = Some(SyntaxRequest::Full);
            }
            self.pending_syntax_seq += 1;
        }
    }

    /// Shift cached annotations (syntax highlights, diagnostics, git line
    /// statuses) after a document edit.  This is the public entry-point used
    /// by callers that perform multi-step edits outside `edit()`/`edit_at()`.
    pub fn shift_annotations(&mut self, edit_row: usize, old_lines: usize, old_ver: DocVersion) {
        self.sync_annotations(edit_row, old_lines, old_ver);
    }

    // ── Private ──

    fn sync_annotations(&mut self, edit_row: usize, old_lines: usize, old_ver: DocVersion) {
        let cur_ver = self.version;
        if cur_ver == old_ver {
            return;
        }
        // Mark modified on any doc change, regardless of line count delta.
        self.mark_modified_if_dirty();
        // Already synced for this doc version (e.g. edit() + with_buf both call us).
        if cur_ver == self.annotations_synced_ver {
            return;
        }
        let new_lines = self.doc().line_count();
        let delta = new_lines as isize - old_lines as isize;
        if delta == 0 {
            self.annotations_synced_ver = cur_ver;
            return;
        }
        // Shift syntax highlights
        let shifted: Vec<_> = self
            .syntax_highlights
            .iter()
            .filter_map(|(line, span)| {
                if *line <= edit_row {
                    Some((*line, span.clone()))
                } else {
                    let new_line = (*line as isize + delta) as usize;
                    if new_line < new_lines {
                        Some((new_line, span.clone()))
                    } else {
                        None
                    }
                }
            })
            .collect();
        self.syntax_highlights = Rc::new(shifted);
        // Shift diagnostics + clear git line statuses
        self.status.shift_lines(edit_row, delta);
        // Mark modified if dirty
        self.mark_modified_if_dirty();
        self.annotations_synced_ver = cur_ver;
    }

    pub fn mark_modified_if_dirty(&mut self) {
        if self.is_dirty() && self.save_state == SaveState::Clean {
            self.save_state = SaveState::Modified;
        }
    }
}

impl fmt::Debug for BufferState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BufferState")
            .field("doc_id", &self.doc_id)
            .field("path", &self.path)
            .field("cursor_row", &self.cursor_row)
            .field("cursor_col", &self.cursor_col)
            .field("scroll_row", &self.scroll_row)
            .field("tab_order", &self.tab_order)
            .field("last_used", &self.last_used)
            .finish()
    }
}

// ── File browser ──

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory { expanded: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Default)]
pub struct FileBrowserState {
    pub root: Option<PathBuf>,
    pub dir_contents: HashMap<PathBuf, Vec<led_fs::DirEntry>>,
    pub expanded_dirs: HashSet<PathBuf>,
    pub entries: Rc<Vec<TreeEntry>>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub pending_reveal: Option<PathBuf>,
}

impl FileBrowserState {
    /// Rebuild the flat `entries` list from `dir_contents` and `expanded_dirs`.
    /// Pure — no I/O.
    pub fn rebuild_entries(&mut self) {
        let entries = Rc::make_mut(&mut self.entries);
        entries.clear();
        let Some(ref root) = self.root else { return };
        let root = root.clone();
        walk_tree(&root, 0, &self.dir_contents, &self.expanded_dirs, entries);
    }

    /// Expand ancestors of `path`, rebuild entries, and try to select it.
    /// Sets `pending_reveal` for retry when dir listings arrive asynchronously.
    /// Returns newly expanded directories that need listing.
    pub fn reveal(&mut self, path: &Path) -> Vec<PathBuf> {
        self.pending_reveal = Some(path.to_path_buf());

        let Some(ref root) = self.root else {
            return vec![];
        };
        let root = root.clone();

        // Expand ancestor directories from parent up to (but not including) root
        let mut newly_expanded = Vec::new();
        let mut ancestor = path.parent();
        while let Some(dir) = ancestor {
            if dir == root || !dir.starts_with(&root) {
                break;
            }
            if self.expanded_dirs.insert(dir.to_path_buf()) {
                newly_expanded.push(dir.to_path_buf());
            }
            ancestor = dir.parent();
        }

        self.rebuild_entries();
        self.complete_pending_reveal();

        newly_expanded
    }

    /// If `pending_reveal` is set, search entries for a match and select it.
    /// Clears `pending_reveal` on success.
    pub fn complete_pending_reveal(&mut self) {
        let Some(ref path) = self.pending_reveal else {
            return;
        };
        if let Some(idx) = self.entries.iter().position(|e| e.path == *path) {
            self.selected = idx;
            self.pending_reveal = None;
        }
    }
}

fn walk_tree(
    dir: &PathBuf,
    depth: usize,
    dir_contents: &HashMap<PathBuf, Vec<led_fs::DirEntry>>,
    expanded_dirs: &HashSet<PathBuf>,
    entries: &mut Vec<TreeEntry>,
) {
    let Some(contents) = dir_contents.get(dir) else {
        return;
    };
    for entry in contents {
        let path = dir.join(&entry.name);
        let expanded = entry.is_dir && expanded_dirs.contains(&path);
        let kind = if entry.is_dir {
            EntryKind::Directory { expanded }
        } else {
            EntryKind::File
        };
        entries.push(TreeEntry {
            path: path.clone(),
            name: entry.name.clone(),
            depth,
            kind,
        });
        if expanded {
            walk_tree(&path, depth + 1, dir_contents, expanded_dirs, entries);
        }
    }
}

// ── Find file ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindFileMode {
    Open,
    SaveAs,
}

#[derive(Debug, Clone)]
pub struct FindFileState {
    pub mode: FindFileMode,
    pub input: String,
    pub cursor: usize,                           // byte position in input
    pub base_input: String,                      // input before arrow selection; dir prefix source
    pub completions: Vec<led_fs::FindFileEntry>, // from driver, already filtered+sorted
    pub selected: Option<usize>,
    pub show_side: bool,
}

// ── Alerts ──

#[derive(Debug, Clone, Default)]
pub struct AlertState {
    pub info: Option<String>,
    pub warn: Option<String>,
}

impl AlertState {
    pub fn has_alert(&self) -> bool {
        self.info.is_some() || self.warn.is_some()
    }

    pub fn clear(&mut self) {
        self.info = None;
        self.warn = None;
    }
}

// ── Keyboard macros ──

#[derive(Debug, Clone, Default)]
pub struct KbdMacroState {
    pub recording: bool,
    pub current: Vec<led_core::Action>,
    pub last: Option<Vec<led_core::Action>>,
    pub playback_depth: usize,
    pub execute_count: Option<usize>,
}

// ── Git ──

#[derive(Debug, Clone, Default)]
pub struct GitState {
    pub branch: Option<String>,
    pub file_statuses: HashMap<PathBuf, HashSet<led_core::git::FileStatus>>,
    pub pending_file_scan: Versioned<()>,
    pub pending_line_scan: Versioned<Option<PathBuf>>,
    pub scan_seq: Versioned<()>,
}

// ── Session ──

#[derive(Debug, Clone, Default)]
pub struct SessionState {
    pub restore_phase: SessionRestorePhase,
    pub restored_focus: Option<PanelSlot>,
    pub positions: HashMap<PathBuf, SessionBuffer>,
    pub active_tab_order: Option<usize>,
    pub pending_opens: Versioned<Vec<PathBuf>>,
    pub saved: bool,
    pub watchers_ready: bool,
}

// ── Jump list ──

#[derive(Debug, Clone, Default)]
pub struct JumpListState {
    pub entries: VecDeque<JumpPosition>,
    pub index: usize,
    pub pending_position: Option<JumpPosition>,
}

// ── Kill ring ──

#[derive(Debug, Clone, Default)]
pub struct KillRingState {
    pub content: String,
    pub accumulator: Option<String>,
    pub pending_yank: Versioned<()>,
}

impl KillRingState {
    pub fn accumulate(&mut self, text: &str) {
        self.accumulator
            .get_or_insert_with(String::new)
            .push_str(text);
        self.content = self.accumulator.clone().unwrap();
    }

    pub fn set(&mut self, text: String) {
        self.content = text;
    }

    pub fn break_accumulation(&mut self) {
        self.accumulator = None;
    }
}

// ── LSP ──

#[derive(Debug, Clone, Default)]
pub struct LspState {
    pub inlay_hints_enabled: bool,

    // Popups
    pub completion: Option<CompletionState>,
    pub code_actions: Option<CodeActionPickerState>,
    pub rename: Option<RenameState>,

    // Status bar — two indicators
    pub server_name: String,
    pub busy: bool,
    pub progress: Option<LspProgress>,
    pub spinner_tick: u32,

    // Single pending request
    pub pending_request: Versioned<Option<LspRequest>>,

    // Format-on-save: trigger save after format completes
    pub pending_save_after_format: bool,
}

#[derive(Debug, Clone)]
pub enum LspRequest {
    GotoDefinition,
    Format,
    CodeAction,
    Complete,
    Rename { new_name: String },
    CodeActionSelect { index: usize },
    CompleteAccept { index: usize },
}

#[derive(Debug, Clone)]
pub struct CompletionState {
    pub items: Vec<led_lsp::CompletionItem>,
    pub prefix_start_col: usize,
    pub selected: usize,
    pub scroll_offset: usize,
}

#[derive(Debug, Clone)]
pub struct CodeActionPickerState {
    pub actions: Vec<String>,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct RenameState {
    pub input: String,
    pub cursor: usize,
}

#[derive(Debug, Clone)]
pub struct LspProgress {
    pub title: String,
    pub message: Option<String>,
}

// ── Preview ──

#[derive(Debug, Clone, Default)]
pub struct PreviewState {
    pub buffer: Option<PathBuf>,
    pub pre_preview_buffer: Option<PathBuf>,
    pub pending: Versioned<Option<PreviewRequest>>,
}

// ── App state ──

#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub startup: Arc<Startup>,
    pub workspace: Option<Rc<Workspace>>,
    pub config_keys: Option<ConfigFile<Keys>>,
    pub config_theme: Option<ConfigFile<Theme>>,
    pub keymap: Option<Rc<Keymap>>,
    pub focus: PanelSlot,
    pub show_side_panel: bool,
    pub dims: Option<Dimensions>,
    pub quit: bool,
    pub suspend: bool,
    pub force_redraw: u64,
    pub alerts: AlertState,
    pub buffers: Rc<HashMap<PathBuf, Rc<BufferState>>>,
    pub active_buffer: Option<PathBuf>,
    pub save_request: Versioned<()>,
    pub save_all_request: Versioned<()>,
    pub save_done: Versioned<()>,
    pub browser: Rc<FileBrowserState>,
    pub pending_open: Versioned<Option<PathBuf>>,
    pub pending_lists: Versioned<Vec<PathBuf>>,

    // Session persistence
    pub session: SessionState,
    pub pending_undo_flush: Versioned<Option<UndoFlush>>,
    pub pending_undo_clear: Versioned<PathBuf>,
    pub pending_sync_check: Versioned<PathBuf>,
    pub notify_hash_to_buffer: HashMap<String, PathBuf>,

    // Confirmation prompts
    pub confirm_kill: bool,

    // Kill ring & clipboard
    pub kill_ring: KillRingState,

    // Keyboard macros
    pub kbd_macro: KbdMacroState,

    // Jump list
    pub jump: JumpListState,

    // Find file / Save as
    pub find_file: Option<FindFileState>,
    pub pending_find_file_list: Versioned<Option<(PathBuf, String, bool)>>,
    pub pending_save_as: Versioned<Option<PathBuf>>,

    // File search
    pub file_search: Option<file_search::FileSearchState>,
    pub pending_file_search: Versioned<Option<file_search::FileSearchRequest>>,
    pub pending_file_replace: Versioned<Option<file_search::FileSearchReplaceRequest>>,
    pub pending_replace_opens: Versioned<Vec<std::path::PathBuf>>,
    pub pending_replace_all: Option<file_search::PendingReplaceAll>,

    // Preview buffer
    pub preview: PreviewState,

    // Git
    pub git: Rc<GitState>,

    // LSP
    pub lsp: Rc<LspState>,
}

impl AppState {
    pub fn new(startup: Startup) -> Self {
        Self {
            startup: Arc::new(startup),
            show_side_panel: true,
            ..Default::default()
        }
    }

    pub fn buffers_mut(&mut self) -> &mut HashMap<PathBuf, Rc<BufferState>> {
        Rc::make_mut(&mut self.buffers)
    }

    /// Get a mutable reference to a single buffer via copy-on-write.
    pub fn buf_mut(&mut self, path: &Path) -> Option<&mut BufferState> {
        Rc::make_mut(&mut self.buffers)
            .get_mut(path)
            .map(|rc| Rc::make_mut(rc))
    }

    pub fn browser_mut(&mut self) -> &mut FileBrowserState {
        Rc::make_mut(&mut self.browser)
    }

    pub fn git_mut(&mut self) -> &mut GitState {
        Rc::make_mut(&mut self.git)
    }

    pub fn lsp_mut(&mut self) -> &mut LspState {
        Rc::make_mut(&mut self.lsp)
    }
}
