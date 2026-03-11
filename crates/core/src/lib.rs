use tokio_stream::Stream;

mod alert;
mod config;
mod ext;
mod fanout;
pub mod keys;
pub mod theme;
mod watch;

pub use alert::{Alert, AlertExt};
pub use config::Startup;
pub use ext::StreamOpsExt;
pub use ext::{Combine, Dedupe, Flatten, Merge, Reduce, SampleCombine};
pub use fanout::{FanoutStream, FanoutStreamExt, LatestStream};
pub use watch::watch;

pub trait AStream<T>: Stream<Item = T> + Send + 'static {}
impl<S, T> AStream<T> for S where S: Stream<Item = T> + Send + 'static {}

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
