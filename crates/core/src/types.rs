use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_status::{FileStatus, LineStatus};
use crate::lsp_types::{EditorCodeAction, EditorDiagnostic, EditorInlayHint, EditorTextEdit};

pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// LSP server status for display in the status bar.
#[derive(Clone, Debug, Default)]
pub struct LspStatus {
    pub server_name: String,
    pub busy: bool,
    pub detail: Option<String>,
}

// ---------------------------------------------------------------------------
// Action
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    LineStart,
    LineEnd,
    PageUp,
    PageDown,
    FileStart,
    FileEnd,
    InsertChar(char),
    InsertNewline,
    DeleteBackward,
    DeleteForward,
    InsertTab,
    KillLine,
    Save,
    SaveForce,
    Tick,
    Quit,
    ToggleFocus,
    ToggleSidePanel,
    ExpandDir,
    CollapseDir,
    CollapseAll,
    OpenSelected,
    OpenSelectedBg,
    PrevTab,
    NextTab,
    Undo,
    KillBuffer,
    Abort,
    Suspend,
    SetMark,
    KillRegion,
    Yank,
    OpenFileSearch,
    CloseFileSearch,
    ToggleSearchCase,
    ToggleSearchRegex,
    InBufferSearch,
    FindFile,
    FocusGained,
    FocusLost,
    SaveSession,
    RestoreSession,
    Flush,
    LspGotoDefinition,
    LspRename,
    LspCodeAction,
    LspFormat,
    LspNextDiagnostic,
    LspPrevDiagnostic,
    LspToggleInlayHints,
    JumpBack,
    JumpForward,
    OpenMessages,
}

// ---------------------------------------------------------------------------
// Panel system
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelSlot {
    Main,
    Side,
    StatusBar,
}

#[derive(Debug, Clone)]
pub struct PanelClaim {
    pub slot: PanelSlot,
    pub priority: u32,
}

#[derive(Debug, Clone)]
pub struct TabDescriptor {
    pub label: String,
    pub dirty: bool,
    pub path: Option<PathBuf>,
    pub preview: bool,
    pub read_only: bool,
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

pub trait Clipboard {
    fn get_text(&self) -> Option<String>;
    fn set_text(&self, text: &str);
}

// ---------------------------------------------------------------------------
// Events & Effects
// ---------------------------------------------------------------------------

pub enum Event {
    OpenFile(PathBuf),
    OpenDefinition(PathBuf),
    TabActivated {
        path: Option<PathBuf>,
    },
    Resume,
    FileSearchOpened {
        selected_text: Option<String>,
    },
    GoToPosition {
        path: PathBuf,
        row: usize,
        col: usize,
        scroll_offset: Option<usize>,
    },
    PreviewFile {
        path: PathBuf,
        row: usize,
        col: usize,
        match_len: usize,
    },
    PreviewClosed,
    PreviewPromoted,
    ConfirmSearch {
        path: PathBuf,
        row: usize,
        col: usize,
    },
    FindFileOpened {
        dir: PathBuf,
    },
    FileSaved(PathBuf),
    /// An LSP notification arrived from a language server
    LspNotification {
        server_name: String,
        method: String,
        params: serde_json::Value,
    },
    /// A buffer was closed
    BufferClosed(PathBuf),
    /// LSP: go to definition request
    LspGotoDefinition {
        path: PathBuf,
        row: usize,
        col: usize,
    },
    /// LSP: rename request
    LspRename {
        path: PathBuf,
        row: usize,
        col: usize,
        new_name: String,
    },
    /// LSP: code action request
    LspCodeAction {
        path: PathBuf,
        start_row: usize,
        start_col: usize,
        end_row: usize,
        end_col: usize,
    },
    /// LSP: resolve a selected code action
    LspCodeActionResolve {
        path: PathBuf,
        index: usize,
    },
    /// LSP: format document
    LspFormat {
        path: PathBuf,
    },
    /// LSP: request inlay hints for visible range
    LspInlayHints {
        path: PathBuf,
        start_row: usize,
        end_row: usize,
    },
    /// LSP response: set diagnostics for a file
    SetDiagnostics {
        path: PathBuf,
        diagnostics: Vec<EditorDiagnostic>,
    },
    /// LSP response: apply text edits to a file
    ApplyEdits {
        path: PathBuf,
        edits: Vec<EditorTextEdit>,
    },
    /// LSP response: show code action picker
    ShowCodeActions {
        path: PathBuf,
        actions: Vec<EditorCodeAction>,
    },
    /// LSP response: set inlay hints for a file
    SetInlayHints {
        path: PathBuf,
        hints: Vec<EditorInlayHint>,
    },
    /// Record current position before a navigation jump
    RecordJump {
        path: PathBuf,
        row: usize,
        col: usize,
        scroll_offset: usize,
    },
    /// Navigate back in the jump list (carries current position for save-at-present)
    JumpBack {
        path: PathBuf,
        row: usize,
        col: usize,
        scroll_offset: usize,
    },
    /// Navigate forward in the jump list
    JumpForward,
    /// Open the *Messages* buffer
    OpenMessages,
}

pub enum Effect {
    Emit(Event),
    Spawn(Box<dyn super::Component>),
    SetMessage(String),
    FocusPanel(PanelSlot),
    ConfirmAction {
        prompt: String,
        action: Action,
    },
    ActivateBuffer(PathBuf),
    KillPreview,
    SetFileStatuses {
        statuses: HashMap<PathBuf, HashSet<FileStatus>>,
        branch: Option<String>,
    },
    SetLineStatuses {
        path: PathBuf,
        statuses: Vec<LineStatus>,
    },
    Quit,
    PromptRename {
        prompt: String,
        initial: String,
        path: PathBuf,
        row: usize,
        col: usize,
    },
    SetLspStatus(LspStatus),
    ShowPicker {
        title: String,
        items: Vec<String>,
        source_path: PathBuf,
    },
}
