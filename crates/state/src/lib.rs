use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use led_config_file::ConfigFile;
use led_core::keys::{Keymap, Keys};
use led_core::theme::Theme;
use led_core::{
    CanonPath, ChangeSeq, CharOffset, Col, Doc, DocVersion, EditOp, InertDoc, PanelSlot,
    PersistedContentHash, RedrawSeq, Row, Startup, SubLine, SyntaxSeq, UndoHistory, UserPath,
    Versioned,
};
pub use led_workspace::SessionBuffer;
pub use led_workspace::Workspace;

// ── Phase ──

/// Application lifecycle phase.
///
/// ```text
/// Init ──┬── Resuming ──┐
///        └──────────────┼── Running ⇄ Suspended
///                       │
///        (any phase) ───┴── Exiting ── (process exit)
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for workspace, config, session load.
    #[default]
    Init,
    /// Session found, buffers being opened from DB.
    Resuming,
    /// Fully operational. Focus is resolved on entry.
    Running,
    /// Terminal suspended (SIGTSTP). Returns to Running.
    Suspended,
    /// Quit requested. Session being saved.
    Exiting,
}

pub mod file_search;

// ── Resume ──

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeState {
    Pending,
    Opened,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeEntry {
    pub path: CanonPath,
    pub state: ResumeState,
}

// ── Tab ──

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    path: CanonPath,
    preview: Option<PreviewTab>,
    pending_cursor: Option<(Row, Col, Row)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewTab {
    previous_tab: CanonPath,
}

impl Tab {
    pub fn new(path: CanonPath) -> Self {
        Self {
            path,
            preview: None,
            pending_cursor: None,
        }
    }

    pub fn path(&self) -> &CanonPath {
        &self.path
    }

    pub fn is_preview(&self) -> bool {
        self.preview.is_some()
    }

    pub fn previous_tab(&self) -> Option<&CanonPath> {
        self.preview.as_ref().map(|p| &p.previous_tab)
    }

    pub fn set_preview(&mut self, previous_tab: CanonPath) {
        self.preview = Some(PreviewTab { previous_tab });
    }

    pub fn unpreview(&mut self) {
        self.preview = None;
    }

    pub fn set_cursor(&mut self, row: Row, col: Col, scroll_row: Row) {
        self.pending_cursor = Some((row, col, scroll_row));
    }

    pub fn has_pending_cursor(&self) -> bool {
        self.pending_cursor.is_some()
    }

    pub fn take_cursor(&mut self) -> Option<(Row, Col, Row)> {
        self.pending_cursor.take()
    }
}

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

    /// Clear diagnostics that touch a row whose content changed (e.g. text killed).
    fn clear_diagnostics_on_row(&mut self, row: usize) {
        let row = Row(row);
        self.diagnostics
            .retain(|d| !(d.start_row <= row && d.end_row >= row));
    }

    /// Shift line positions after a structural edit.
    fn shift_lines(&mut self, edit_row: usize, delta: isize) {
        let edit_row = Row(edit_row);
        self.diagnostics.retain_mut(|d| {
            // Remove diagnostics on the edit row and deleted lines.
            // The edit row's content changes during a structural edit
            // (e.g. line join), so its diagnostics are stale.
            if delta < 0 {
                let deleted_end = edit_row + (-delta) as usize;
                if d.start_row >= edit_row && d.end_row <= deleted_end {
                    return false;
                }
            }
            // Shift diagnostics below the edit
            if d.start_row > edit_row {
                d.start_row = Row((d.start_row.0 as isize + delta).max(0) as usize);
            }
            if d.end_row > edit_row {
                d.end_row = Row((d.end_row.0 as isize + delta).max(0) as usize);
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
    pub path: CanonPath,
    pub row: Row,
    pub col: Col,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JumpPosition {
    pub path: CanonPath,
    pub row: Row,
    pub col: Col,
    pub scroll_offset: Row,
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
    pub file_path: CanonPath,
    pub chain_id: String,
    pub content_hash: PersistedContentHash,
    pub undo_cursor: usize,
    pub distance_from_save: i32,
    pub entries: Vec<led_core::UndoEntry>,
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
/// Set by buffer operations themselves. Derived streams branch on this
/// to decide whether to send `didSave` to the LSP (LocalSave or
/// ExternalFileChange both indicate the content matches disk).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChangeReason {
    /// Buffer just constructed — no content loaded yet.
    #[default]
    Init,
    /// User edit, yank, undo, redo, format edits, or cross-instance sync.
    Edit,
    /// Local save completed (docstore confirmed write).
    LocalSave,
    /// Content reloaded from disk (e.g. external `git checkout .`).
    ExternalFileChange,
}

// ── Syntax highlight types ──

#[derive(Debug, Clone, PartialEq)]
pub struct HighlightSpan {
    pub char_start: Col,
    pub char_end: Col,
    pub capture_name: Rc<str>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BracketPair {
    pub open_line: Row,
    pub open_col: Col,
    pub close_line: Row,
    pub close_col: Col,
    pub color_index: Option<usize>,
}

impl BracketPair {
    /// Find the matching bracket for the cursor from cached pairs.
    pub fn find_match(
        pairs: &[BracketPair],
        cursor_row: Row,
        cursor_col: Col,
    ) -> Option<(Row, Col)> {
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
    pub origin: (Row, Col),
    pub origin_scroll: Row,
    pub origin_sub_line: SubLine,
    pub failed: bool,
    pub matches: Vec<(Row, Col, usize)>, // (row, col, char_len)
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
    Unmaterialized,
    /// Open requested to docstore, waiting for response.
    Requested,
    /// Content loaded from disk. Doc is TextDoc.
    Materialized,
}

#[derive(Clone)]
pub struct BufferState {
    // Identity
    path: Option<CanonPath>,

    // Content (InertDoc for non-materialized buffers)
    doc: Arc<dyn Doc>,
    materialization: Cell<MaterializationState>,

    // Editing state (owned by buffer, not doc)
    version: DocVersion,
    /// Snapshot of `version` at the last save (or external reload).
    /// Used as a dedupe key for "the saved content changed."
    saved_version: DocVersion,
    undo: UndoHistory,

    // Cursor (interior mutable — allows set_cursor through &self)
    cursor_row: Cell<Row>,
    cursor_col: Cell<Col>,
    cursor_col_affinity: Cell<Col>,
    mark: Option<(Row, Col)>,

    // Scroll (interior mutable — allows set_scroll through &self)
    scroll_row: Cell<Row>,
    scroll_sub_line: Cell<SubLine>,

    // Edit tracking
    last_edit_kind: Option<EditKind>,
    save_state: SaveState,

    // Undo persistence
    persisted_undo_len: usize,
    chain_id: Option<String>,
    last_seen_seq: i64,
    content_hash: PersistedContentHash,
    change_seq: ChangeSeq,
    change_reason: ChangeReason,

    // Incremental search
    pub isearch: Option<ISearchState>,
    pub last_search: Option<String>,

    // Syntax highlighting
    pending_syntax_request: Option<SyntaxRequest>,
    pending_syntax_seq: SyntaxSeq,
    syntax_highlights: Rc<Vec<(Row, HighlightSpan)>>,
    bracket_pairs: Rc<Vec<BracketPair>>,
    matching_bracket: Option<(Row, Col)>,

    // Indent
    pending_indent_row: Option<Row>,
    pending_tab_fallback: bool,
    reindent_chars: Arc<[char]>,

    // LSP
    completion_triggers: Vec<String>,

    // Materialization
    create_if_missing: bool,

    // UI
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
    pub fn new(path: CanonPath) -> Self {
        Self {
            doc: Arc::new(InertDoc),
            materialization: Cell::new(MaterializationState::Unmaterialized),
            version: DocVersion(0),
            saved_version: DocVersion(0),
            undo: UndoHistory::default(),
            path: Some(path),
            cursor_row: Cell::new(Row(0)),
            cursor_col: Cell::new(Col(0)),
            cursor_col_affinity: Cell::new(Col(0)),
            scroll_row: Cell::new(Row(0)),
            scroll_sub_line: Cell::new(SubLine(0)),
            mark: None,
            last_edit_kind: None,
            save_state: SaveState::Clean,
            persisted_undo_len: 0,
            chain_id: None,
            last_seen_seq: 0,
            content_hash: PersistedContentHash(0),
            change_seq: ChangeSeq(0),
            change_reason: ChangeReason::Init,
            isearch: None,
            last_search: None,
            pending_syntax_request: None,
            pending_syntax_seq: SyntaxSeq(0),
            syntax_highlights: Rc::new(Vec::new()),
            bracket_pairs: Rc::new(Vec::new()),
            matching_bracket: None,
            pending_indent_row: None,
            pending_tab_fallback: false,
            reindent_chars: Arc::from([]),
            completion_triggers: Vec::new(),
            create_if_missing: false,

            last_used: Instant::now(),
            status: BufferStatus::new(),
            annotations_synced_ver: DocVersion(0),
        }
    }

    // ── Identity ──

    pub fn path(&self) -> Option<&CanonPath> {
        self.path.as_ref()
    }

    // ── Editing state ──

    /// Monotonic version counter. Incremented on every content edit, undo, redo.
    pub fn version(&self) -> DocVersion {
        self.version
    }

    /// Snapshot of `version` at the last save (or external reload).
    /// Only changes when the on-disk content changes.
    pub fn saved_version(&self) -> DocVersion {
        self.saved_version
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
        self.materialization.get() == MaterializationState::Materialized
    }

    /// Whether this buffer is in the initial unmaterialized state.
    pub fn is_unmaterialized(&self) -> bool {
        self.materialization.get() == MaterializationState::Unmaterialized
    }

    pub fn materialization(&self) -> MaterializationState {
        self.materialization.get()
    }

    /// Mark as requested via interior mutability. Called from derived
    /// streams to prevent duplicate Open emissions.
    pub fn mark_requested(&self) {
        self.materialization.set(MaterializationState::Requested);
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
    pub fn materialize(&mut self, doc: Arc<dyn Doc>, clear_annotations: bool) {
        self.content_hash = PersistedContentHash(doc.content_hash().0);
        self.doc = doc;
        self.materialization.set(MaterializationState::Materialized);
        self.change_reason = ChangeReason::Init;
        if clear_annotations {
            self.syntax_highlights = Rc::new(Vec::new());
            self.bracket_pairs = Rc::new(Vec::new());
            self.matching_bracket = None;
            self.status = BufferStatus::new();
        }
        self.set_syntax_full();
    }

    /// Dematerialize: replace doc with InertDoc, discard undo.
    /// Increments version so any in-flight Open response will mismatch
    /// and be treated as a fresh materialization.
    pub fn dematerialize(&mut self) {
        self.doc = Arc::new(InertDoc);
        self.undo = UndoHistory::default();
        self.version = self.version + 1;
        self.materialization
            .set(MaterializationState::Unmaterialized);
        self.pending_syntax_request = None;
    }

    pub fn create_if_missing(&self) -> bool {
        self.create_if_missing
    }

    pub fn set_create_if_missing(&mut self, v: bool) {
        self.create_if_missing = v;
    }

    // ── Syntax request ──

    pub fn pending_syntax_request(&self) -> Option<&SyntaxRequest> {
        self.pending_syntax_request.as_ref()
    }

    pub fn pending_syntax_seq(&self) -> SyntaxSeq {
        self.pending_syntax_seq
    }

    fn set_syntax_full(&mut self) {
        self.pending_syntax_request = Some(SyntaxRequest::Full);
        self.pending_syntax_seq = led_core::next_syntax_seq();
    }

    fn append_syntax_edit(&mut self, op: EditOp) {
        match &mut self.pending_syntax_request {
            Some(SyntaxRequest::Full) => {
                // Full subsumes Partial — keep Full but bump seq
                // so derived re-fires with the updated doc.
                self.pending_syntax_seq = led_core::next_syntax_seq();
            }
            Some(SyntaxRequest::Partial { edit_ops }) => {
                edit_ops.push(op);
                self.pending_syntax_seq = led_core::next_syntax_seq();
            }
            None => {
                self.pending_syntax_request = Some(SyntaxRequest::Partial { edit_ops: vec![op] });
                self.pending_syntax_seq = led_core::next_syntax_seq();
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
        let edit_row = self.cursor_row.get();
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
        edit_row: Row,
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
        let edit_row = self.cursor_row.get();
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
        self.change_reason = ChangeReason::Edit;
        self.touch();
    }

    /// Remove text between two character offsets. Records undo op, shifts annotations.
    pub fn remove_text(&mut self, start: CharOffset, end: CharOffset) {
        let edit_row = self.cursor_row.get();
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
        self.change_reason = ChangeReason::Edit;
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

        led_core::with_line_buf(|line_buf| {
            for line_idx in (0..line_count).rev() {
                let row = Row(line_idx);
                doc.line(row, line_buf);
                // Strip line endings first — only trim actual whitespace.
                let content_len = line_buf.trim_end_matches(&['\n', '\r'][..]).len();
                line_buf.truncate(content_len);
                let trimmed = line_buf.trim_end();
                if trimmed.len() < line_buf.len() {
                    let line_start = doc.line_to_char(row).0;
                    let start = CharOffset(line_start + trimmed.chars().count());
                    let end = CharOffset(line_start + line_buf.chars().count());
                    removals.push((start, end));
                }
            }
        });

        let needs_final_newline = {
            let last_row = Row(doc.line_count().saturating_sub(1));
            doc.line_len(last_row) > 0
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
        self.content_hash = PersistedContentHash(self.doc().content_hash().0);
        self.change_seq = led_core::next_change_seq();
        self.change_reason = ChangeReason::LocalSave;
        self.version = self.version + 1;
        self.saved_version = self.version;
        self.set_syntax_full();
    }

    /// Save-as completed: docstore confirmed save to new path.
    /// Same as save_completed but also updates path.
    pub fn save_as_completed(&mut self, doc: Arc<dyn Doc>, new_path: CanonPath) {
        self.path = Some(new_path);
        self.save_completed(doc);
    }

    // ── External changes ──

    /// File changed on disk with different content. Replaces doc,
    /// clears annotations, clamps cursor.
    pub fn reload_from_disk(&mut self, doc: Arc<dyn Doc>) {
        self.content_hash = PersistedContentHash(doc.content_hash().0);
        self.doc = doc;
        self.version = self.version + 1;
        self.saved_version = self.version;
        self.change_seq = led_core::next_change_seq();
        self.change_reason = ChangeReason::ExternalFileChange;
        // Request full syntax reparse. Keep old highlights visible until new ones arrive.
        self.set_syntax_full();
        // Clamp cursor to new document bounds.
        let max_row = Row(self.doc().line_count().saturating_sub(1));
        self.cursor_row.set(self.cursor_row.get().min(max_row));
        let max_col = Col(self.doc().line_len(self.cursor_row.get()));
        self.cursor_col.set(self.cursor_col.get().min(max_col));
        self.cursor_col_affinity.set(self.cursor_col.get());
        self.close_group_on_move();
    }

    /// File was saved externally (detected by sync). Marks doc clean
    /// without changing content. Bumps version so LSP is notified.
    pub fn mark_externally_saved(&mut self) {
        self.version = self.version + 1;
        self.saved_version = self.version;
        self.last_seen_seq = 0;
        self.chain_id = None;
        self.persisted_undo_len = self.undo.entry_count();
        self.change_seq = led_core::next_change_seq();
        self.change_reason = ChangeReason::ExternalFileChange;
        if self.is_dirty() && self.save_state == SaveState::Clean {
            self.undo.reset_distance_from_save();
        }
    }

    // ── Workspace sync ──

    /// Apply a batch of remote undo entries to this buffer.
    ///
    /// Validates seq monotonicity and chain anchor before applying. The
    /// entries' positions were computed against `content_hash` on
    /// local; if our buffer is at a different anchor we refuse to
    /// apply (returns `false`) so the caller can leave the entries in
    /// SQLite for retry on the next sync round.
    ///
    /// Handles both same-chain (anchor unchanged) and chain-switch
    /// cases uniformly: caller passes the chain_id and anchor that
    /// SQLite holds, and the post-apply state mirrors them.
    pub fn try_apply_sync(
        &mut self,
        chain_id: String,
        content_hash: PersistedContentHash,
        entries: &[led_core::UndoEntry],
        new_last_seen_seq: i64,
    ) -> bool {
        if new_last_seen_seq <= self.last_seen_seq {
            log::trace!(
                "sync: skipping apply, already seen ({} <= {})",
                new_last_seen_seq,
                self.last_seen_seq,
            );
            return false;
        }
        if self.content_hash != content_hash {
            log::trace!(
                "sync: chain anchor mismatch, ours={:#x} theirs={:#x}, refusing apply",
                self.content_hash.0,
                content_hash.0,
            );
            return false;
        }
        self.apply_persisted_entries(entries);
        // Annotations are stale after remote edits — clear them.
        self.syntax_highlights = Rc::new(Vec::new());
        self.bracket_pairs = Rc::new(Vec::new());
        self.matching_bracket = None;
        self.status = BufferStatus::new();
        self.chain_id = Some(chain_id);
        self.last_seen_seq = new_last_seen_seq;
        self.persisted_undo_len = self.undo.entry_count();
        self.change_seq = led_core::next_change_seq();
        self.change_reason = ChangeReason::Edit;
        self.set_syntax_full();
        true
    }

    /// Apply a batch of typed undo entries to the doc + undo history.
    ///
    /// No validation. Used by session restore (where the buffer was
    /// just opened against the chain anchor and the entries are
    /// trusted) and internally by `try_apply_sync`.
    pub fn apply_persisted_entries(&mut self, entries: &[led_core::UndoEntry]) {
        self.close_undo_group();
        for entry in entries {
            let doc = led_core::apply_op_to_doc(&self.doc, &entry.op);
            self.doc = doc;
            self.undo.push_remote_entry(entry.clone());
            self.version = self.version + 1;
            self.change_reason = ChangeReason::Edit;
        }
    }

    // ── Undo flush ──

    /// Undo entries flushed to workspace (pending confirmation).
    pub fn undo_flush_started(&mut self, chain_id: String, undo_cursor: usize) {
        self.chain_id = Some(chain_id);
        self.persisted_undo_len = undo_cursor;
        self.change_seq = led_core::next_change_seq();
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
                    content_hash: None,
                },
                direction,
            );
        }

        self.undo.set_undo_cursor(Some(pos));
        self.version = self.version + 1;
        self.doc = doc;
        self.change_reason = ChangeReason::Edit;
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
        self.change_reason = ChangeReason::Edit;
        self.set_syntax_full();
        Some(restore_cursor)
    }

    // ── Cursor ──

    pub fn cursor_row(&self) -> Row {
        self.cursor_row.get()
    }
    pub fn cursor_col(&self) -> Col {
        self.cursor_col.get()
    }
    pub fn cursor_col_affinity(&self) -> Col {
        self.cursor_col_affinity.get()
    }
    pub fn set_cursor(&self, row: Row, col: Col, affinity: Col) {
        let max_row = Row(self.doc().line_count().saturating_sub(1));
        self.cursor_row.set(row.min(max_row));
        let max_col = Col(self.doc().line_len(self.cursor_row.get()));
        self.cursor_col.set(col.min(max_col));
        self.cursor_col_affinity.set(affinity);
    }
    pub fn set_cursor_row(&mut self, row: Row) {
        let max_row = Row(self.doc().line_count().saturating_sub(1));
        self.cursor_row.set(row.min(max_row));
    }

    // ── Mark ──

    pub fn mark(&self) -> Option<(Row, Col)> {
        self.mark
    }
    pub fn set_mark(&mut self) {
        self.mark = Some((self.cursor_row.get(), self.cursor_col.get()));
    }
    pub fn set_mark_at(&mut self, row: Row, col: Col) {
        self.mark = Some((row, col));
    }
    pub fn clear_mark(&mut self) {
        self.mark = None;
    }

    // ── Scroll ──

    pub fn scroll_row(&self) -> Row {
        self.scroll_row.get()
    }
    pub fn scroll_sub_line(&self) -> SubLine {
        self.scroll_sub_line.get()
    }
    pub fn set_scroll(&self, row: Row, sub_line: SubLine) {
        self.scroll_row
            .set(row.min(Row(self.doc().line_count().saturating_sub(1))));
        self.scroll_sub_line.set(sub_line);
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

    /// Record a save-point marker in the undo chain for diagnostic replay.
    pub fn record_diag_save_point(&mut self) {
        let hash = PersistedContentHash(self.doc.content_hash().0);
        log::trace!(
            "diag: record_save_point hash={:#x} path={:?} undo_len={}",
            hash.0,
            self.path,
            self.undo.entry_count(),
        );
        self.undo.insert_save_point(hash);
    }

    /// Accept diagnostics, replaying edits if the content_hash is stale.
    /// Unmaterialized buffers always accept — no content to go stale against.
    pub fn offer_diagnostics(
        &mut self,
        diags: Vec<led_lsp::Diagnostic>,
        content_hash: PersistedContentHash,
    ) -> bool {
        if !self.is_materialized() {
            log::trace!(
                "diag: offer_diagnostics unmaterialized, accepting {} diags, path={:?}",
                diags.len(),
                self.path,
            );
            self.status.set_diagnostics(diags);
            return true;
        }
        let my_hash = self.doc.content_hash();
        // Fast path: exact match (common case, also handles undo-back-to-save)
        if content_hash.0 == my_hash.0 {
            log::trace!(
                "diag: offer_diagnostics FAST path, hash={:#x}, {} diags, path={:?}",
                content_hash.0,
                diags.len(),
                self.path,
            );
            self.status.set_diagnostics(diags);
            return true;
        }
        // Replay path: find save-point marker, reconstruct and transform
        if let Some(save_idx) = self.undo.find_save_point(content_hash) {
            let n_entries = self.undo.entry_count() - save_idx - 1;
            log::trace!(
                "diag: offer_diagnostics REPLAY path, req_hash={:#x} cur_hash={:#x}, {} diags, {} entries to replay, path={:?}",
                content_hash.0,
                my_hash.0,
                diags.len(),
                n_entries,
                self.path,
            );
            let transformed = self.replay_diagnostics(diags, save_idx);
            self.status.set_diagnostics(transformed);
            return true;
        }
        log::debug!(
            "offer_diagnostics rejected: incoming={:?} buffer={:?} path={:?} n_diags={}",
            content_hash,
            self.doc.content_hash(),
            self.path,
            diags.len()
        );
        false
    }

    /// Reconstruct save-time doc, walk forward through edits, transform
    /// diagnostic positions (clear edited rows, shift for structural changes).
    fn replay_diagnostics(
        &mut self,
        mut diags: Vec<led_lsp::Diagnostic>,
        save_idx: usize,
    ) -> Vec<led_lsp::Diagnostic> {
        self.undo.flush_pending();
        let entries = self.undo.entries_from(save_idx + 1);

        // Reconstruct save-time doc by walking backward from current doc
        let mut doc = self.doc.clone();
        for entry in entries.iter().rev() {
            if entry.op.is_noop() {
                continue;
            }
            let inv = EditOp {
                offset: entry.op.offset,
                old_text: entry.op.new_text.clone(),
                new_text: entry.op.old_text.clone(),
            };
            doc = led_core::apply_op_to_doc(&doc, &inv);
        }

        // Walk forward, transforming diagnostics at each step
        for entry in entries {
            if entry.op.is_noop() {
                continue;
            }
            let edit_row = doc.char_to_line(entry.op.offset);
            let old_newlines = entry.op.old_text.chars().filter(|&c| c == '\n').count();
            let new_newlines = entry.op.new_text.chars().filter(|&c| c == '\n').count();
            let delta = new_newlines as isize - old_newlines as isize;

            if delta == 0 {
                // Content edit on this row — clear diagnostics touching it
                diags.retain(|d| !(d.start_row <= edit_row && d.end_row >= edit_row));
            } else {
                // Structural edit — shift/remove diagnostics
                diags.retain_mut(|d| {
                    if delta < 0 {
                        let deleted_end = Row(*edit_row + (-delta) as usize);
                        if d.start_row >= edit_row && d.end_row <= deleted_end {
                            return false;
                        }
                    }
                    if d.start_row > edit_row {
                        d.start_row = Row((d.start_row.0 as isize + delta).max(0) as usize);
                    }
                    if d.end_row > edit_row {
                        d.end_row = Row((d.end_row.0 as isize + delta).max(0) as usize);
                    }
                    true
                });
            }

            // Advance replay doc
            doc = led_core::apply_op_to_doc(&doc, &entry.op);
        }

        diags
    }

    /// Accept syntax highlights only if doc version matches.
    pub fn offer_syntax(
        &mut self,
        highlights: Rc<Vec<(Row, HighlightSpan)>>,
        bracket_pairs: Vec<BracketPair>,
        version: DocVersion,
    ) -> bool {
        if self.version == version {
            self.syntax_highlights = highlights;
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

    pub fn syntax_highlights(&self) -> &Rc<Vec<(Row, HighlightSpan)>> {
        &self.syntax_highlights
    }
    pub fn bracket_pairs(&self) -> &Rc<Vec<BracketPair>> {
        &self.bracket_pairs
    }
    pub fn matching_bracket(&self) -> Option<(Row, Col)> {
        self.matching_bracket
    }
    pub fn update_matching_bracket(&mut self) {
        self.matching_bracket = BracketPair::find_match(
            &self.bracket_pairs,
            self.cursor_row.get(),
            self.cursor_col.get(),
        );
    }
    pub fn status(&self) -> &BufferStatus {
        &self.status
    }

    // ── Persistence & sync (read-only for external code) ──

    pub fn content_hash(&self) -> PersistedContentHash {
        self.content_hash
    }
    pub fn change_seq(&self) -> ChangeSeq {
        self.change_seq
    }
    pub fn change_reason(&self) -> ChangeReason {
        self.change_reason
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
        content_hash: PersistedContentHash,
        distance_from_save: i32,
    ) {
        self.persisted_undo_len = persisted_undo_len;
        self.chain_id = chain_id;
        self.last_seen_seq = last_seen_seq;
        self.content_hash = content_hash;
        self.undo.set_distance_from_save(distance_from_save);
    }

    // ── Activity tracking ──

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
    pub fn pending_indent_row(&self) -> Option<Row> {
        self.pending_indent_row
    }
    pub fn pending_tab_fallback(&self) -> bool {
        self.pending_tab_fallback
    }

    /// Request auto-indent for a given row.  When `tab_fallback` is true,
    /// a literal tab-stop will be inserted if the language server returns
    /// no indent change.
    pub fn request_indent(&mut self, row: Option<Row>, tab_fallback: bool) {
        self.pending_indent_row = row;
        self.pending_tab_fallback = tab_fallback;
        if row.is_some() {
            // Ensure a syntax cycle fires so the driver can compute
            // the indent.  Bump seq even if a request already exists
            // so derived re-evaluates with the new indent_row.
            if self.pending_syntax_request.is_none() {
                self.pending_syntax_request = Some(SyntaxRequest::Full);
            }
            self.pending_syntax_seq = led_core::next_syntax_seq();
        }
    }

    /// Shift cached annotations (syntax highlights, diagnostics, git line
    /// statuses) after a document edit.  This is the public entry-point used
    /// by callers that perform multi-step edits outside `edit()`/`edit_at()`.
    pub fn shift_annotations(&mut self, edit_row: Row, old_lines: usize, old_ver: DocVersion) {
        self.sync_annotations(edit_row, old_lines, old_ver);
    }

    // ── Private ──

    fn sync_annotations(&mut self, edit_row: Row, old_lines: usize, old_ver: DocVersion) {
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
            // No structural change, but the edit row's content changed.
            // Clear stale diagnostics on that row.
            self.status.clear_diagnostics_on_row(*edit_row);
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
                    let new_line = (line.0 as isize + delta) as usize;
                    if new_line < new_lines {
                        Some((Row(new_line), span.clone()))
                    } else {
                        None
                    }
                }
            })
            .collect();
        self.syntax_highlights = Rc::new(shifted);
        // Shift diagnostics + clear git line statuses
        self.status.shift_lines(*edit_row, delta);
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
            .field("path", &self.path)
            .field("cursor_row", &self.cursor_row)
            .field("cursor_col", &self.cursor_col)
            .field("scroll_row", &self.scroll_row)
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
    pub path: CanonPath,
    pub name: String,
    pub depth: usize,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Default)]
pub struct FileBrowserState {
    pub root: Option<CanonPath>,
    pub dir_contents: HashMap<CanonPath, Vec<led_fs::DirEntry>>,
    pub expanded_dirs: HashSet<CanonPath>,
    pub entries: Rc<Vec<TreeEntry>>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub pending_reveal: Option<CanonPath>,
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
    pub fn reveal(&mut self, path: &CanonPath) -> Vec<CanonPath> {
        self.pending_reveal = Some(path.clone());

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
            if self.expanded_dirs.insert(dir.clone()) {
                newly_expanded.push(dir.clone());
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
    dir: &CanonPath,
    depth: usize,
    dir_contents: &HashMap<CanonPath, Vec<led_fs::DirEntry>>,
    expanded_dirs: &HashSet<CanonPath>,
    entries: &mut Vec<TreeEntry>,
) {
    let Some(contents) = dir_contents.get(dir) else {
        return;
    };
    for entry in contents {
        let path = UserPath::new(dir.as_path().join(&entry.name)).canonicalize();
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
    pub file_statuses: HashMap<CanonPath, HashSet<led_core::git::FileStatus>>,
    pub pending_file_scan: Versioned<()>,
    pub scan_seq: Versioned<()>,
}

// ── Session ──

#[derive(Debug, Clone, Default)]
pub struct SessionState {
    pub positions: HashMap<CanonPath, SessionBuffer>,
    pub active_tab_order: Option<usize>,
    pub pending_opens: Versioned<Vec<CanonPath>>,
    pub resume: Vec<ResumeEntry>,
    pub saved: bool,
    pub watchers_ready: bool,
}

// ── Jump list ──

#[derive(Debug, Clone, Default)]
pub struct JumpListState {
    pub entries: VecDeque<JumpPosition>,
    pub index: usize,
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

// ── App state ──

#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub startup: Arc<Startup>,
    pub workspace: Option<Rc<Workspace>>,
    pub config_keys: Option<ConfigFile<Keys>>,
    pub config_theme: Option<ConfigFile<Theme>>,
    pub keymap: Option<Rc<Keymap>>,
    pub phase: Phase,
    pub focus: PanelSlot,
    pub show_side_panel: bool,
    pub dims: Option<Dimensions>,
    pub force_redraw: RedrawSeq,
    pub alerts: AlertState,
    pub buffers: Rc<HashMap<CanonPath, Rc<BufferState>>>,
    pub tabs: VecDeque<Tab>,
    pub active_tab: Option<CanonPath>,
    pub save_request: Versioned<()>,
    pub save_all_request: Versioned<()>,
    pub save_done: Versioned<()>,
    pub browser: Rc<FileBrowserState>,
    pub pending_open: Versioned<Option<CanonPath>>,
    pub pending_lists: Versioned<Vec<CanonPath>>,

    // Session persistence
    pub session: SessionState,
    pub pending_undo_flush: Versioned<Option<UndoFlush>>,
    pub pending_undo_clear: Versioned<CanonPath>,
    pub pending_sync_check: Versioned<CanonPath>,
    pub notify_hash_to_buffer: HashMap<String, CanonPath>,

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
    pub pending_find_file_list: Versioned<Option<(CanonPath, String, bool)>>,
    pub pending_save_as: Versioned<Option<CanonPath>>,

    // File search
    pub file_search: Option<file_search::FileSearchState>,
    pub pending_file_search: Versioned<Option<file_search::FileSearchRequest>>,
    pub pending_file_replace: Versioned<Option<file_search::FileSearchReplaceRequest>>,
    pub pending_replace_opens: Versioned<Vec<CanonPath>>,
    pub pending_replace_all: Option<file_search::PendingReplaceAll>,

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

    pub fn buffers_mut(&mut self) -> &mut HashMap<CanonPath, Rc<BufferState>> {
        Rc::make_mut(&mut self.buffers)
    }

    /// Get a mutable reference to a single buffer via copy-on-write.
    pub fn buf_mut(&mut self, path: &CanonPath) -> Option<&mut BufferState> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(start_row: usize, end_row: usize) -> led_lsp::Diagnostic {
        led_lsp::Diagnostic {
            start_row: led_core::Row(start_row),
            start_col: led_core::Col(0),
            end_row: led_core::Row(end_row),
            end_col: led_core::Col(0),
            severity: led_lsp::DiagnosticSeverity::Warning,
            message: String::new(),
            source: None,
            code: None,
        }
    }

    #[test]
    fn clear_diagnostics_on_row_removes_matching() {
        // When text is killed on a line (delta=0), the diagnostic
        // on that row should be cleared.
        let mut status = BufferStatus::new();
        status.set_diagnostics(vec![diag(5, 5), diag(8, 8)]);
        status.clear_diagnostics_on_row(5);
        assert_eq!(status.diagnostics().len(), 1);
        assert_eq!(status.diagnostics()[0].start_row, led_core::Row(8));
    }

    #[test]
    fn clear_diagnostics_on_row_removes_spanning() {
        // A diagnostic that spans across the edit row should also be cleared
        // (e.g. hint diagnostic covering rows 4-6, edit on row 5).
        let mut status = BufferStatus::new();
        status.set_diagnostics(vec![diag(4, 6), diag(8, 8)]);
        status.clear_diagnostics_on_row(5);
        assert_eq!(status.diagnostics().len(), 1);
        assert_eq!(status.diagnostics()[0].start_row, led_core::Row(8));
    }

    #[test]
    fn clear_diagnostics_on_row_keeps_others() {
        let mut status = BufferStatus::new();
        status.set_diagnostics(vec![diag(3, 3), diag(7, 7)]);
        status.clear_diagnostics_on_row(5);
        assert_eq!(status.diagnostics().len(), 2);
    }

    #[test]
    fn shift_lines_removes_diagnostic_on_edit_row() {
        // ctrl-k join: diagnostic on the edit row should be cleared
        // because the line's content changed.
        let mut status = BufferStatus::new();
        status.set_diagnostics(vec![diag(5, 5)]);
        status.shift_lines(5, -1);
        assert!(status.diagnostics().is_empty());
    }

    #[test]
    fn shift_lines_removes_diagnostic_on_deleted_line_below() {
        let mut status = BufferStatus::new();
        status.set_diagnostics(vec![diag(6, 6)]);
        status.shift_lines(5, -1);
        assert!(status.diagnostics().is_empty());
    }

    #[test]
    fn shift_lines_shifts_diagnostic_below_deleted_range() {
        let mut status = BufferStatus::new();
        status.set_diagnostics(vec![diag(10, 10)]);
        status.shift_lines(5, -2);
        assert_eq!(status.diagnostics()[0].start_row, led_core::Row(8));
        assert_eq!(status.diagnostics()[0].end_row, led_core::Row(8));
    }

    #[test]
    fn shift_lines_keeps_diagnostic_above_edit_row() {
        let mut status = BufferStatus::new();
        status.set_diagnostics(vec![diag(3, 3)]);
        status.shift_lines(5, -1);
        assert_eq!(status.diagnostics()[0].start_row, led_core::Row(3));
    }

    #[test]
    fn shift_lines_insert_does_not_remove_diagnostic_on_edit_row() {
        // Inserting a line should not clear diagnostics on the edit row.
        let mut status = BufferStatus::new();
        status.set_diagnostics(vec![diag(5, 5)]);
        status.shift_lines(5, 1);
        assert_eq!(status.diagnostics()[0].start_row, led_core::Row(5));
    }

    #[test]
    fn shift_lines_insert_shifts_diagnostic_below() {
        let mut status = BufferStatus::new();
        status.set_diagnostics(vec![diag(8, 8)]);
        status.shift_lines(5, 1);
        assert_eq!(status.diagnostics()[0].start_row, led_core::Row(9));
    }

    #[test]
    fn shift_lines_multi_line_delete() {
        // Delete 3 lines starting below edit_row.
        // Diagnostic on edit_row: removed (content changed).
        // Diagnostic on deleted lines: removed.
        // Diagnostic below: shifted.
        let mut status = BufferStatus::new();
        status.set_diagnostics(vec![diag(5, 5), diag(7, 7), diag(10, 10)]);
        status.shift_lines(5, -3);
        assert_eq!(status.diagnostics().len(), 1);
        assert_eq!(status.diagnostics()[0].start_row, led_core::Row(7)); // was 10, shifted by -3
    }

    // ── Diagnostic replay tests ──

    fn diag_msg(start_row: usize, end_row: usize, msg: &str) -> led_lsp::Diagnostic {
        led_lsp::Diagnostic {
            start_row: led_core::Row(start_row),
            start_col: led_core::Col(0),
            end_row: led_core::Row(end_row),
            end_col: led_core::Col(5),
            severity: led_lsp::DiagnosticSeverity::Error,
            message: msg.to_string(),
            source: None,
            code: None,
        }
    }

    fn make_buf(content: &str) -> BufferState {
        let path = led_core::UserPath::new("/tmp/test.rs").canonicalize();
        let mut buf = BufferState::new(path);
        let doc: Arc<dyn led_core::Doc> = Arc::new(
            led_core::TextDoc::from_reader(std::io::Cursor::new(content.as_bytes())).unwrap(),
        );
        buf.materialize(doc, false);
        buf
    }

    #[test]
    fn offer_diagnostics_exact_match() {
        let mut buf = make_buf("hello\nworld\n");
        let hash = PersistedContentHash(buf.doc().content_hash().0);
        let diags = vec![diag_msg(1, 1, "error on world")];
        assert!(buf.offer_diagnostics(diags, hash));
        assert_eq!(buf.status().diagnostics().len(), 1);
    }

    #[test]
    fn offer_diagnostics_rejects_unknown_hash() {
        let mut buf = make_buf("hello\nworld\n");
        let diags = vec![diag_msg(1, 1, "error")];
        assert!(!buf.offer_diagnostics(diags, PersistedContentHash(999)));
        assert!(buf.status().diagnostics().is_empty());
    }

    #[test]
    fn offer_diagnostics_replay_clears_edited_row() {
        let mut buf = make_buf("hello\nworld\n");
        let save_hash = PersistedContentHash(buf.doc().content_hash().0);
        buf.record_diag_save_point();

        // Insert char on row 1 (offset 6 = start of "world")
        buf.insert_text(led_core::CharOffset(6), "x");

        let diags = vec![
            diag_msg(0, 0, "error on hello"),
            diag_msg(1, 1, "error on world"),
        ];
        assert!(buf.offer_diagnostics(diags, save_hash));
        let result = buf.status().diagnostics();
        assert_eq!(result.len(), 1);
        assert!(result[0].message.contains("hello"));
    }

    #[test]
    fn offer_diagnostics_replay_shifts_after_newline() {
        let mut buf = make_buf("aaa\nbbb\nccc\n");
        let save_hash = PersistedContentHash(buf.doc().content_hash().0);
        buf.record_diag_save_point();

        // Insert newline at end of row 0 (offset 3, between "aaa" and "\n")
        buf.insert_text(led_core::CharOffset(3), "\n");

        let diags = vec![diag_msg(2, 2, "error on ccc")];
        assert!(buf.offer_diagnostics(diags, save_hash));
        let result = buf.status().diagnostics();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].start_row, led_core::Row(3));
    }

    #[test]
    fn offer_diagnostics_replay_removes_deleted_line() {
        let mut buf = make_buf("aaa\nbbb\nccc\nddd\n");
        let save_hash = PersistedContentHash(buf.doc().content_hash().0);
        buf.record_diag_save_point();

        // Remove "bbb\n" (offset 4..8)
        buf.remove_text(led_core::CharOffset(4), led_core::CharOffset(8));

        let diags = vec![
            diag_msg(0, 0, "error on aaa"),
            diag_msg(1, 1, "error on bbb"),
            diag_msg(3, 3, "error on ddd"),
        ];
        assert!(buf.offer_diagnostics(diags, save_hash));
        let result = buf.status().diagnostics();
        // "aaa" at row 0: kept. "bbb" at row 1: removed (in deleted range).
        // "ddd" at row 3: shifted by -1 to row 2.
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].start_row, led_core::Row(0));
        assert_eq!(result[1].start_row, led_core::Row(2));
    }

    #[test]
    fn offer_diagnostics_fast_path_after_undo_to_save() {
        let mut buf = make_buf("hello\n");
        let save_hash = PersistedContentHash(buf.doc().content_hash().0);
        buf.record_diag_save_point();

        buf.insert_text(led_core::CharOffset(0), "x");
        buf.undo();

        assert_eq!(buf.doc().content_hash().0, save_hash.0);
        let diags = vec![diag_msg(0, 0, "error")];
        assert!(buf.offer_diagnostics(diags, save_hash));
        assert_eq!(buf.status().diagnostics().len(), 1);
    }

    #[test]
    fn replay_multi_step_insert_then_newline() {
        // Save, insert char on row 0, then insert newline on row 0.
        // Diagnostics should: clear row 0 (edited), shift row 1+ by +1 (newline).
        // "aaa\nbbb\nccc\n"
        let mut buf = make_buf("aaa\nbbb\nccc\n");
        let save_hash = PersistedContentHash(buf.doc().content_hash().0);
        buf.record_diag_save_point();

        // Step 1: insert "x" at offset 0 (row 0) — "xaaa\nbbb\nccc\n"
        buf.insert_text(led_core::CharOffset(0), "x");
        // Close undo group so next edit is separate
        buf.close_undo_group();

        // Step 2: insert newline at offset 2 (row 0) — "xa\naa\nbbb\nccc\n"
        buf.insert_text(led_core::CharOffset(2), "\n");

        let diags = vec![
            diag_msg(0, 0, "error on aaa"),
            diag_msg(1, 1, "error on bbb"),
            diag_msg(2, 2, "error on ccc"),
        ];
        assert!(buf.offer_diagnostics(diags, save_hash));
        let result = buf.status().diagnostics();
        // Row 0 ("aaa"): cleared by step 1 (char edit on row 0)
        // Row 1 ("bbb"): after step 1 still row 1, after step 2 shifts to row 2
        // Row 2 ("ccc"): after step 1 still row 2, after step 2 shifts to row 3
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].start_row, led_core::Row(2));
        assert_eq!(result[1].start_row, led_core::Row(3));
    }

    #[test]
    fn replay_multi_step_newline_then_edit_below() {
        // Save, insert newline at row 1, then edit the new row 3 (was row 2).
        // "aaa\nbbb\nccc\n"
        let mut buf = make_buf("aaa\nbbb\nccc\n");
        let save_hash = PersistedContentHash(buf.doc().content_hash().0);
        buf.record_diag_save_point();

        // Step 1: insert newline at offset 7 (end of "bbb") — "aaa\nbbb\n\nccc\n"
        buf.insert_text(led_core::CharOffset(7), "\n");
        buf.close_undo_group();

        // Step 2: insert "x" at offset 9 (start of "ccc", now row 3) — "aaa\nbbb\n\nxccc\n"
        buf.insert_text(led_core::CharOffset(9), "x");

        let diags = vec![
            diag_msg(0, 0, "error on aaa"),
            diag_msg(1, 1, "error on bbb"),
            diag_msg(2, 2, "error on ccc"),
        ];
        assert!(buf.offer_diagnostics(diags, save_hash));
        let result = buf.status().diagnostics();
        // Row 0 ("aaa"): untouched → stays at 0
        // Row 1 ("bbb"): step 1 inserts newline at row 1, but diagnostic is ON row 1
        //   not BELOW it, so it stays at row 1
        // Row 2 ("ccc"): step 1 shifts it from row 2 → row 3, step 2 edits row 3 → cleared
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].start_row, led_core::Row(0));
        assert_eq!(result[1].start_row, led_core::Row(1));
    }

    #[test]
    fn replay_multi_step_delete_then_insert() {
        // Save, delete a line, then insert a newline elsewhere.
        // "aaa\nbbb\nccc\nddd\n"
        let mut buf = make_buf("aaa\nbbb\nccc\nddd\n");
        let save_hash = PersistedContentHash(buf.doc().content_hash().0);
        buf.record_diag_save_point();

        // Step 1: remove "bbb\n" (offset 4..8) — "aaa\nccc\nddd\n"
        buf.remove_text(led_core::CharOffset(4), led_core::CharOffset(8));
        buf.close_undo_group();

        // Step 2: insert newline at offset 3 (end of "aaa") — "aaa\n\nccc\nddd\n"
        buf.insert_text(led_core::CharOffset(3), "\n");

        let diags = vec![
            diag_msg(0, 0, "error on aaa"),
            diag_msg(3, 3, "error on ddd"),
        ];
        assert!(buf.offer_diagnostics(diags, save_hash));
        let result = buf.status().diagnostics();
        // Row 0 ("aaa"): step 1 doesn't touch it. Step 2: newline at row 0,
        //   diagnostic is ON row 0, not shifted (shift only applies to rows > edit_row)
        // Row 3 ("ddd"): step 1 (delta=-1, edit_row=1): 3 > 1, shifted to 2.
        //   step 2 (delta=+1, edit_row=0): 2 > 0, shifted to 3.
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].start_row, led_core::Row(0));
        assert_eq!(result[1].start_row, led_core::Row(3));
    }

    #[test]
    fn replay_many_char_edits_on_same_row() {
        // Save, type several characters on row 1. Only row 1 diagnostic
        // should be cleared; others untouched.
        let mut buf = make_buf("aaa\nbbb\nccc\n");
        let save_hash = PersistedContentHash(buf.doc().content_hash().0);
        buf.record_diag_save_point();

        // Type "xyz" at start of row 1 (offset 4)
        buf.insert_text(led_core::CharOffset(4), "x");
        buf.insert_text(led_core::CharOffset(5), "y");
        buf.insert_text(led_core::CharOffset(6), "z");

        let diags = vec![
            diag_msg(0, 0, "error on aaa"),
            diag_msg(1, 1, "error on bbb"),
            diag_msg(2, 2, "error on ccc"),
        ];
        assert!(buf.offer_diagnostics(diags, save_hash));
        let result = buf.status().diagnostics();
        // Row 1 cleared (edited), rows 0 and 2 untouched
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].start_row, led_core::Row(0));
        assert_eq!(result[1].start_row, led_core::Row(2));
    }

    // ── Sync apply tests ──

    fn doc_from(content: &str) -> Arc<dyn led_core::Doc> {
        Arc::new(led_core::TextDoc::from_reader(std::io::Cursor::new(content.as_bytes())).unwrap())
    }

    /// Hash of the rope built from `content` (live/ephemeral hash).
    fn rope_hash(content: &str) -> u64 {
        doc_from(content).content_hash().0
    }

    fn insert_entry(offset: usize, text: &str) -> led_core::UndoEntry {
        led_core::UndoEntry {
            op: led_core::EditOp {
                offset: led_core::CharOffset(offset),
                old_text: String::new(),
                new_text: text.to_string(),
            },
            cursor_before: led_core::CharOffset(offset),
            cursor_after: led_core::CharOffset(offset + text.chars().count()),
            direction: 1,
            content_hash: None,
        }
    }

    /// Buffer in a known sync state: materialized at `content`, on chain
    /// `chain_id` with the matching anchor and a non-zero `last_seen_seq`.
    fn synced_buf(content: &str, chain_id: &str, last_seen_seq: i64) -> BufferState {
        let mut buf = make_buf(content);
        let anchor = PersistedContentHash(buf.doc().content_hash().0);
        buf.restore_session(0, Some(chain_id.to_string()), last_seen_seq, anchor, 0);
        buf
    }

    #[test]
    fn try_apply_sync_rejects_old_seq() {
        let mut buf = synced_buf("hello", "chain-x", 5);
        let anchor = buf.content_hash();
        let before_doc_hash = buf.doc().content_hash().0;

        // new_last_seen_seq <= current → refuse
        let applied = buf.try_apply_sync("chain-x".to_string(), anchor, &[insert_entry(5, "!")], 5);
        assert!(!applied);
        // Buffer untouched
        assert_eq!(buf.last_seen_seq(), 5);
        assert_eq!(buf.doc().content_hash().0, before_doc_hash);
        assert_eq!(buf.chain_id(), Some("chain-x"));
    }

    #[test]
    fn try_apply_sync_rejects_anchor_mismatch() {
        let mut buf = synced_buf("hello", "chain-x", 5);
        let before_doc_hash = buf.doc().content_hash().0;
        let bogus_anchor = PersistedContentHash(0xdead_beef);

        let applied = buf.try_apply_sync(
            "chain-x".to_string(),
            bogus_anchor,
            &[insert_entry(5, "!")],
            6,
        );
        assert!(!applied);
        // Buffer untouched
        assert_eq!(buf.last_seen_seq(), 5);
        assert_eq!(buf.doc().content_hash().0, before_doc_hash);
    }

    #[test]
    fn try_apply_sync_succeeds_on_same_chain() {
        let mut buf = synced_buf("hello", "chain-x", 5);
        let anchor = buf.content_hash();

        // Insert " world" at end (offset 5) → "hello world"
        let applied = buf.try_apply_sync(
            "chain-x".to_string(),
            anchor,
            &[insert_entry(5, " world")],
            7,
        );
        assert!(applied);
        assert_eq!(buf.doc().content_hash().0, rope_hash("hello world"));
        assert_eq!(buf.chain_id(), Some("chain-x"));
        assert_eq!(buf.last_seen_seq(), 7);
        // Anchor (PersistedContentHash) unchanged — sync replay does not
        // move it, so subsequent rounds can verify against it.
        assert_eq!(buf.content_hash(), anchor);
    }

    #[test]
    fn try_apply_sync_handles_chain_switch() {
        // Buffer is on chain X. A new chain Y arrives with the SAME
        // anchor (e.g. local saved+immediately edited; the new chain's
        // anchor matches the disk state the remote already has).
        let mut buf = synced_buf("hello", "chain-x", 5);
        let anchor = buf.content_hash();

        let applied = buf.try_apply_sync("chain-y".to_string(), anchor, &[insert_entry(5, "!")], 8);
        assert!(applied);
        assert_eq!(buf.chain_id(), Some("chain-y"));
        assert_eq!(buf.doc().content_hash().0, rope_hash("hello!"));
        assert_eq!(buf.last_seen_seq(), 8);
    }

    #[test]
    fn apply_persisted_entries_skips_validation() {
        // Used by session restore — no anchor check, no seq bookkeeping.
        let mut buf = make_buf("hello");
        let anchor_before = buf.content_hash();
        let seq_before = buf.last_seen_seq();

        buf.apply_persisted_entries(&[insert_entry(5, "!")]);

        assert_eq!(buf.doc().content_hash().0, rope_hash("hello!"));
        // No anchor or seq bookkeeping — those are caller's responsibility.
        assert_eq!(buf.content_hash(), anchor_before);
        assert_eq!(buf.last_seen_seq(), seq_before);
    }

    #[test]
    fn reload_then_sync_succeeds() {
        // Order A: docstore reload arrives BEFORE the queued sync entries.
        // After reload, the buffer is at the new chain's anchor and the
        // sync apply succeeds on the first try.
        let mut buf = synced_buf("hello", "chain-x", 5);

        // Local saved a new state to disk: "hello world".
        let new_disk = doc_from("hello world");
        let new_anchor = PersistedContentHash(new_disk.content_hash().0);
        buf.reload_from_disk(new_disk);
        assert_eq!(buf.content_hash(), new_anchor);

        // Sync entries arrive: a new chain Y rooted at the new disk state,
        // appending "!" at the end.
        let applied = buf.try_apply_sync(
            "chain-y".to_string(),
            new_anchor,
            &[insert_entry(11, "!")],
            10,
        );
        assert!(applied);
        assert_eq!(buf.doc().content_hash().0, rope_hash("hello world!"));
        assert_eq!(buf.chain_id(), Some("chain-y"));
        assert_eq!(buf.last_seen_seq(), 10);
    }

    #[test]
    fn sync_before_reload_refuses_then_recovers() {
        // Order B: sync entries arrive BEFORE the docstore reload. They
        // were generated against the new chain's anchor, but the buffer
        // is still at the old anchor — refuse. After reload moves the
        // buffer to the new anchor, retrying the same sync apply succeeds.
        let mut buf = synced_buf("hello", "chain-x", 5);
        let stale_anchor = buf.content_hash();

        // Local saved "hello world" — that hash is the new chain anchor.
        let new_disk = doc_from("hello world");
        let new_anchor = PersistedContentHash(new_disk.content_hash().0);

        // Sync entries arrive first. Buffer is still at the old anchor;
        // applying these would corrupt the rope, so we refuse.
        let entries = vec![insert_entry(11, "!")];
        let applied = buf.try_apply_sync("chain-y".to_string(), new_anchor, &entries, 10);
        assert!(!applied);
        // Buffer is unchanged.
        assert_eq!(buf.doc().content_hash().0, rope_hash("hello"));
        assert_eq!(buf.content_hash(), stale_anchor);
        assert_eq!(buf.chain_id(), Some("chain-x"));
        assert_eq!(buf.last_seen_seq(), 5);

        // Now the docstore reload lands.
        buf.reload_from_disk(new_disk);
        assert_eq!(buf.content_hash(), new_anchor);

        // Retry the same sync entries — now anchors match, apply succeeds.
        let applied = buf.try_apply_sync("chain-y".to_string(), new_anchor, &entries, 10);
        assert!(applied);
        assert_eq!(buf.doc().content_hash().0, rope_hash("hello world!"));
        assert_eq!(buf.chain_id(), Some("chain-y"));
        assert_eq!(buf.last_seen_seq(), 10);
    }
}
