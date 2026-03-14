use std::io;

mod alert;
mod config;
mod doc;
pub mod keys;
pub mod rx;
pub mod theme;
mod watch;

pub use alert::{Alert, AlertExt};
pub use config::Startup;
pub use doc::{Doc, TextDoc};
pub use watch::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferId(pub u64);

pub trait WriteContent: Send + Sync + 'static {
    fn write_to(&self, writer: &mut dyn io::Write) -> io::Result<()>;
}

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
    InsertNewline,
    DeleteBackward,
    DeleteForward,
    InsertTab,
    KillLine,

    // File
    Save,
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
}
