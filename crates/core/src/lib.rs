mod alert;
mod config;
mod doc;
pub mod git;
pub mod keys;
pub mod rx;
pub mod theme;
mod watch;
pub mod wrap;

mod versioned;

pub use alert::{Alert, AlertExt};
pub use config::Startup;
pub use doc::{Doc, EditOp, TextDoc, UndoEntry, UndoHistory};
pub use versioned::Versioned;
pub use watch::{FileWatcher, Registration, WatchEvent, WatchEventKind, WatchMode};

use std::sync::atomic::{AtomicU64, Ordering};

static CHANGE_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn next_change_seq() -> u64 {
    CHANGE_SEQ.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeStamp {
    pub seq: u64,
    pub origin: Origin,
    pub content_hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DocId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelSlot {
    #[default]
    Main,
    Side,
    StatusBar,
    Overlay,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    // Movement
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

    // Insert/Delete
    InsertChar(char),
    InsertCloseBracket(char),
    InsertNewline,
    DeleteBackward,
    DeleteForward,
    InsertTab,
    KillLine,

    // File
    Save,
    SaveAs,
    SaveForce,
    KillBuffer,

    // Navigation
    PrevTab,
    NextTab,
    JumpBack,
    JumpForward,
    Outline,
    MatchBracket,

    // Search
    InBufferSearch,
    OpenFileSearch,
    CloseFileSearch,
    ToggleSearchCase,
    ToggleSearchRegex,

    // Find
    FindFile,

    // Edit
    Undo,
    Redo,
    SetMark,
    KillRegion,
    Yank,
    SortImports,

    // LSP
    LspGotoDefinition,
    LspRename,
    LspCodeAction,
    LspFormat,
    LspNextDiagnostic,
    LspPrevDiagnostic,
    LspToggleInlayHints,

    // UI
    ToggleFocus,
    ToggleSidePanel,
    ExpandDir,
    CollapseDir,
    CollapseAll,
    OpenSelected,
    OpenSelectedBg,
    OpenMessages,
    Abort,

    // Lifecycle
    Quit,
    Suspend,

    // Test / headless
    Wait(u64),
    Resize(u16, u16),
}
