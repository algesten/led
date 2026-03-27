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
use led_core::{BufferId, Doc, DocId, PanelSlot, Startup, Versioned};
pub use led_workspace::Workspace;
pub use led_workspace::{SessionBuffer, SessionRestorePhase};

pub mod file_search;

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

#[derive(Clone)]
pub struct BufferState {
    pub id: BufferId,
    pub doc_id: DocId,
    pub doc: Arc<dyn Doc>,
    pub path: Option<PathBuf>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub cursor_col_affinity: usize,
    pub scroll_row: usize,
    pub scroll_sub_line: usize,
    pub tab_order: usize,
    pub mark: Option<(usize, usize)>,
    pub last_edit_kind: Option<EditKind>,
    pub save_state: SaveState,
    // Undo persistence
    pub persisted_undo_len: usize,
    pub chain_id: Option<String>,
    pub last_seen_seq: i64,
    pub content_hash: u64,
    /// Monotonically increasing stamp, bumped on every meaningful modification
    /// (flush, save, sync apply). Used to detect self-echoes at the model level.
    pub change_seq: u64,
    /// Why the buffer content last changed. Set by the reducer on every
    /// `Mut::BufferUpdate` (compiler-enforced) and after `Mut::Action`.
    /// Read by derived to decide e.g. whether the LSP needs a didSave.
    pub change_reason: ChangeReason,
    // Incremental search
    pub isearch: Option<ISearchState>,
    pub last_search: Option<String>,
    // Syntax highlighting
    pub syntax_highlights: Rc<Vec<(usize, HighlightSpan)>>,
    pub bracket_pairs: Rc<Vec<BracketPair>>,
    pub matching_bracket: Option<(usize, usize)>,
    pub pending_indent_row: Option<usize>,
    pub pending_tab_fallback: bool,
    /// Characters that trigger re-indentation when typed, declared by the syntax highlighter.
    pub reindent_chars: Arc<[char]>,
    pub completion_triggers: Vec<String>,
    pub is_preview: bool,
    pub last_used: Instant,
}

impl BufferState {
    pub fn new(id: BufferId, doc_id: DocId, doc: Arc<dyn Doc>, path: Option<PathBuf>) -> Self {
        let content_hash = doc.content_hash();
        Self {
            id,
            doc_id,
            doc,
            path,
            cursor_row: 0,
            cursor_col: 0,
            cursor_col_affinity: 0,
            scroll_row: 0,
            scroll_sub_line: 0,
            tab_order: 0,
            mark: None,
            last_edit_kind: None,
            save_state: SaveState::Clean,
            persisted_undo_len: 0,
            chain_id: None,
            last_seen_seq: 0,
            content_hash,
            change_seq: 0,
            change_reason: ChangeReason::Init,
            isearch: None,
            last_search: None,
            syntax_highlights: Rc::new(Vec::new()),
            bracket_pairs: Rc::new(Vec::new()),
            matching_bracket: None,
            pending_indent_row: None,
            pending_tab_fallback: false,
            reindent_chars: Arc::from([]),
            completion_triggers: Vec::new(),
            is_preview: false,
            last_used: Instant::now(),
        }
    }
}

impl fmt::Debug for BufferState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BufferState")
            .field("id", &self.id)
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

// ── Git ──

#[derive(Debug, Clone, Default)]
pub struct GitState {
    pub branch: Option<String>,
    pub file_statuses: HashMap<PathBuf, HashSet<led_core::git::FileStatus>>,
    pub line_statuses: HashMap<PathBuf, Vec<led_core::git::LineStatus>>,
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
    // Annotations (per-file, for rendering)
    pub diagnostics: HashMap<PathBuf, Vec<led_lsp::Diagnostic>>,
    pub inlay_hints: HashMap<PathBuf, Vec<led_lsp::InlayHint>>,
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
    pub buffer: Option<BufferId>,
    pub pre_preview_buffer: Option<BufferId>,
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
    pub buffers: Rc<HashMap<BufferId, Rc<BufferState>>>,
    pub active_buffer: Option<BufferId>,
    pub next_buffer_id: u64,
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
    pub notify_hash_to_buffer: HashMap<String, BufferId>,

    // Confirmation prompts
    pub confirm_kill: bool,

    // Kill ring & clipboard
    pub kill_ring: KillRingState,

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

    pub fn buffers_mut(&mut self) -> &mut HashMap<BufferId, Rc<BufferState>> {
        Rc::make_mut(&mut self.buffers)
    }

    /// Get a mutable reference to a single buffer via copy-on-write.
    pub fn buf_mut(&mut self, id: BufferId) -> Option<&mut BufferState> {
        Rc::make_mut(&mut self.buffers)
            .get_mut(&id)
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
