mod color_hint;
mod component;
mod editing;
mod search;
pub(crate) mod syntax;
mod undo;
mod watcher;
mod wrap;

pub use component::BufferFactory;

use led_core::{DocStore, PanelClaim, PanelSlot, TextDoc};
use std::fs::File;
use std::hash::Hasher;
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use led_core::Waker;
use led_core::lsp_types::{EditorDiagnostic, EditorInlayHint};
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
// Incremental search state
// ---------------------------------------------------------------------------

pub struct ISearchState {
    pub query: String,
    pub origin: (usize, usize),
    pub origin_scroll: usize,
    pub origin_sub_line: usize,
    pub failed: bool,
    pub matches: Vec<(usize, usize, usize)>, // (row, col, char_len)
    pub match_idx: Option<usize>,
}

// ---------------------------------------------------------------------------
// Buffer
// ---------------------------------------------------------------------------

pub fn notify_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("led/notify"))
}

pub struct Buffer {
    pub local_doc: Option<TextDoc>,
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
    pub(crate) preview_highlight: bool,
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
    pub read_only: bool,
    pub(crate) syntax: Option<syntax::SyntaxState>,
    pub(crate) pending_syntax: Arc<Mutex<Option<syntax::SyntaxState>>>,
    pub(crate) syntax_ready: Arc<AtomicBool>,
    pub(crate) syntax_cancel: Arc<AtomicBool>,
    pub isearch: Option<ISearchState>,
    pub(crate) last_search: Option<String>,
    pub(crate) diagnostics: Vec<EditorDiagnostic>,
    pub(crate) inlay_hints: Vec<EditorInlayHint>,
    pub(crate) inlay_hints_enabled: bool,
    pub(crate) last_hint_range: Option<(usize, usize)>,
    claims: Vec<PanelClaim>,
    claims_with_status: Vec<PanelClaim>,
}

impl Buffer {
    // --- Constructors ---

    pub fn empty() -> Self {
        let doc = TextDoc::new();
        let base_content_hash = Self::hash_rope(doc.rope());
        Self {
            local_doc: Some(doc),
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
            preview_highlight: false,
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
            read_only: false,
            syntax: None,
            pending_syntax: Arc::new(Mutex::new(None)),
            syntax_ready: Arc::new(AtomicBool::new(false)),
            syntax_cancel: Arc::new(AtomicBool::new(false)),
            isearch: None,
            last_search: None,
            diagnostics: Vec::new(),
            inlay_hints: Vec::new(),
            inlay_hints_enabled: false,
            last_hint_range: None,
            claims: vec![
                PanelClaim {
                    slot: PanelSlot::Main,
                    priority: 10,
                },
                PanelClaim {
                    slot: PanelSlot::StatusBar,
                    priority: 10,
                },
            ],
            claims_with_status: vec![
                PanelClaim {
                    slot: PanelSlot::Main,
                    priority: 10,
                },
                PanelClaim {
                    slot: PanelSlot::StatusBar,
                    priority: 20,
                },
            ],
        }
    }

    pub fn from_file(path: &str, docs: &mut DocStore) -> io::Result<Self> {
        Self::from_file_with_waker(path, None, docs)
    }

    pub fn from_file_with_waker(
        path: &str,
        waker: Option<Waker>,
        docs: &mut DocStore,
    ) -> io::Result<Self> {
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
        let doc = TextDoc::from_reader(BufReader::new(file))?;
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
        let base_content_hash = Self::hash_rope(doc.rope());
        let changed = Arc::new(AtomicBool::new(false));
        let _watcher = Self::create_watcher(&canonical, &changed, waker.as_ref());
        let pending_syntax: Arc<Mutex<Option<syntax::SyntaxState>>> = Arc::new(Mutex::new(None));
        let syntax_ready = Arc::new(AtomicBool::new(false));
        let syntax_cancel = Arc::new(AtomicBool::new(false));

        // Clone rope for background syntax parsing before inserting doc into store
        let rope_for_syntax = doc.rope().clone();
        docs.insert(canonical.clone(), doc);

        // Parse syntax on a background thread
        {
            let path = canonical.clone();
            let ready = syntax_ready.clone();
            let pending = pending_syntax.clone();
            let cancel = syntax_cancel.clone();
            let waker = waker.clone();
            tokio::task::spawn_blocking(move || {
                if cancel.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                if let Some(state) =
                    syntax::SyntaxState::from_path_and_rope(&path, &rope_for_syntax)
                {
                    if cancel.load(std::sync::atomic::Ordering::Acquire) {
                        return;
                    }
                    *pending.lock().unwrap() = Some(state);
                    ready.store(true, std::sync::atomic::Ordering::Release);
                    if let Some(w) = waker.as_ref() {
                        w();
                    }
                }
            });
        }

        Ok(Self {
            local_doc: None,
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
            preview_highlight: false,
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
            read_only: false,
            syntax: None,
            pending_syntax,
            syntax_ready,
            syntax_cancel,
            isearch: None,
            last_search: None,
            diagnostics: Vec::new(),
            inlay_hints: Vec::new(),
            inlay_hints_enabled: false,
            last_hint_range: None,
            claims: vec![
                PanelClaim {
                    slot: PanelSlot::Main,
                    priority: 10,
                },
                PanelClaim {
                    slot: PanelSlot::StatusBar,
                    priority: 10,
                },
            ],
            claims_with_status: vec![
                PanelClaim {
                    slot: PanelSlot::Main,
                    priority: 10,
                },
                PanelClaim {
                    slot: PanelSlot::StatusBar,
                    priority: 20,
                },
            ],
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
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.syntax_cancel
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

impl Buffer {
    // --- Accessors ---

    pub fn filename(&self) -> &str {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("[scratch]")
    }

    // --- Syntax ---

    /// Notify syntax state of an insert. Call BEFORE mutating the rope.
    /// Returns an opaque edit to pass to `apply_syntax_edit` after mutation.
    pub(crate) fn syntax_edit_insert(
        &self,
        doc: &TextDoc,
        char_idx: usize,
        text: &str,
    ) -> Option<tree_sitter::InputEdit> {
        if self.syntax.is_some() {
            Some(syntax::edit_for_insert(doc.rope(), char_idx, text))
        } else {
            None
        }
    }

    /// Notify syntax state of a remove. Call BEFORE mutating the rope.
    pub(crate) fn syntax_edit_remove(
        &self,
        doc: &TextDoc,
        char_start: usize,
        char_end: usize,
    ) -> Option<tree_sitter::InputEdit> {
        if self.syntax.is_some() {
            Some(syntax::edit_for_remove(doc.rope(), char_start, char_end))
        } else {
            None
        }
    }

    /// Apply a previously computed edit to the syntax tree. Call AFTER mutating the rope.
    pub(crate) fn apply_syntax_edit(
        &mut self,
        doc: &TextDoc,
        edit: Option<tree_sitter::InputEdit>,
    ) {
        if let (Some(edit), Some(s)) = (edit, &mut self.syntax) {
            s.apply_edit(&edit, doc.rope());
        }
    }

    /// Compute a syntax edit for an EditOp. Call BEFORE applying the op.
    pub(crate) fn syntax_edit_for_op(
        &self,
        doc: &TextDoc,
        op: &EditOp,
    ) -> Option<tree_sitter::InputEdit> {
        match op {
            EditOp::Insert { char_idx, text } => self.syntax_edit_insert(doc, *char_idx, text),
            EditOp::Remove { char_idx, text } => {
                let end = *char_idx + text.chars().count();
                self.syntax_edit_remove(doc, *char_idx, end)
            }
        }
    }

    // --- Helpers ---

    pub(crate) fn clamp_cursor_col(&mut self, doc: &TextDoc) {
        let len = doc.line_len(self.cursor_row);
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

    /// Resolve the doc for this buffer: from DocStore for file-backed, from local_doc otherwise.
    pub(crate) fn take_doc(&mut self, docs: &mut DocStore) -> TextDoc {
        if let Some(ref path) = self.path {
            docs.remove(path).expect("doc not in store")
        } else {
            self.local_doc.take().expect("no local doc")
        }
    }

    /// Put the doc back after use.
    pub(crate) fn put_doc(&mut self, docs: &mut DocStore, doc: TextDoc) {
        if let Some(ref path) = self.path {
            docs.insert(path.clone(), doc);
        } else {
            self.local_doc = Some(doc);
        }
    }
}
