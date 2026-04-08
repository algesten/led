mod alert;
mod config;
mod doc;
pub mod git;
pub mod keys;
mod language;
mod path;
pub mod rx;
pub mod theme;
mod watch;
pub mod wrap;

mod versioned;

pub use alert::{Alert, AlertExt};
pub use config::Startup;
pub use doc::{Doc, EditOp, InertDoc, TextDoc, UndoEntry, UndoHistory, apply_op_to_doc};
pub use language::{LanguageId, LspContextId};
pub use path::{CanonPath, UserPath};
pub use versioned::Versioned;
pub use watch::{FileWatcher, Registration, WatchEvent, WatchEventKind, WatchMode};

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static CHANGE_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn next_change_seq() -> ChangeSeq {
    ChangeSeq(CHANGE_SEQ.fetch_add(1, Ordering::Relaxed))
}

static SYNTAX_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn next_syntax_seq() -> SyntaxSeq {
    SyntaxSeq(SYNTAX_SEQ.fetch_add(1, Ordering::Relaxed))
}

/// Process-unique identifier used to tag undo entries with their
/// originating instance. Generated lazily on first call from time + pid,
/// then frozen for the lifetime of the process.
///
/// Two led instances editing the same file via SQLite sync each have
/// their own id; this is how a buffer can tell whether the edits in
/// its undo chain were typed locally in this process or arrived via
/// sync from somewhere else.
pub fn instance_id() -> u64 {
    static INSTANCE_ID: OnceLock<u64> = OnceLock::new();
    *INSTANCE_ID.get_or_init(|| {
        use std::hash::{DefaultHasher, Hash, Hasher};
        use std::time::SystemTime;
        let mut h = DefaultHasher::new();
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .hash(&mut h);
        std::process::id().hash(&mut h);
        h.finish()
    })
}

thread_local! {
    static LINE_BUF: Cell<String> = const { Cell::new(String::new()) };
}

/// Borrow the thread-local line buffer for the duration of `f`.
/// The buffer retains its allocation across calls — after the first use
/// grows it, subsequent calls reuse that capacity with zero allocation.
pub fn with_line_buf<R>(f: impl FnOnce(&mut String) -> R) -> R {
    LINE_BUF.with(|cell| {
        let mut buf = cell.take();
        let result = f(&mut buf);
        cell.set(buf);
        result
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferId(pub u64);

// ── Domain newtypes ──

macro_rules! newtype_usize {
    ($($(#[$meta:meta])* $name:ident),+ $(,)?) => {$(
        $(#[$meta])*
        #[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash,
                 serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub usize);
        impl std::ops::Deref for $name {
            type Target = usize;
            fn deref(&self) -> &usize { &self.0 }
        }
        impl From<usize> for $name {
            fn from(v: usize) -> Self { Self(v) }
        }
        impl std::ops::Add for $name {
            type Output = Self;
            fn add(self, rhs: Self) -> Self { Self(self.0 + rhs.0) }
        }
        impl std::ops::Add<usize> for $name {
            type Output = Self;
            fn add(self, rhs: usize) -> Self { Self(self.0 + rhs) }
        }
        impl std::ops::Sub for $name {
            type Output = Self;
            fn sub(self, rhs: Self) -> Self { Self(self.0 - rhs.0) }
        }
        impl std::ops::Sub<usize> for $name {
            type Output = Self;
            fn sub(self, rhs: usize) -> Self { Self(self.0 - rhs) }
        }
        impl std::ops::AddAssign<usize> for $name {
            fn add_assign(&mut self, rhs: usize) { self.0 += rhs; }
        }
        impl std::ops::SubAssign<usize> for $name {
            fn sub_assign(&mut self, rhs: usize) { self.0 -= rhs; }
        }
    )+};
}

macro_rules! newtype_u64 {
    ($($(#[$meta:meta])* $name:ident),+ $(,)?) => {$(
        $(#[$meta])*
        #[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash,
                 serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);
        impl std::ops::Deref for $name {
            type Target = u64;
            fn deref(&self) -> &u64 { &self.0 }
        }
        impl From<u64> for $name {
            fn from(v: u64) -> Self { Self(v) }
        }
        impl std::ops::Add for $name {
            type Output = Self;
            fn add(self, rhs: Self) -> Self { Self(self.0 + rhs.0) }
        }
        impl std::ops::Add<u64> for $name {
            type Output = Self;
            fn add(self, rhs: u64) -> Self { Self(self.0 + rhs) }
        }
        impl std::ops::Sub for $name {
            type Output = Self;
            fn sub(self, rhs: Self) -> Self { Self(self.0 - rhs.0) }
        }
        impl std::ops::Sub<u64> for $name {
            type Output = Self;
            fn sub(self, rhs: u64) -> Self { Self(self.0 - rhs) }
        }
        impl std::ops::AddAssign<u64> for $name {
            fn add_assign(&mut self, rhs: u64) { self.0 += rhs; }
        }
        impl std::ops::SubAssign<u64> for $name {
            fn sub_assign(&mut self, rhs: u64) { self.0 -= rhs; }
        }
    )+};
}

newtype_usize! {
    /// Line number in a document (0-based).
    Row,
    /// Character column within a line (0-based).
    Col,
    /// Absolute character offset in a document.
    CharOffset,
    /// Visual sub-line index within a wrapped line.
    SubLine,
}

newtype_u64! {
    /// Monotonic document version (increments on every insert/remove).
    DocVersion,
    /// Save generation counter (increments on every save).
    SaveSeq,
    /// Monotonic change sequence (increments on every observable buffer change).
    ChangeSeq,
    /// Hash of content as persisted on disk.
    PersistedContentHash,
    /// Hash of current in-memory content (ephemeral, computed from rope).
    EphemeralContentHash,
    /// Monotonic syntax-reparse sequence.
    SyntaxSeq,
    /// Monotonic force-redraw sequence.
    RedrawSeq,
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
    SaveAs,
    SaveForce,
    SaveNoFormat,
    SaveAll,
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
    ToggleSearchReplace,
    ReplaceAll,

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
    NextIssue,
    PrevIssue,
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

    // Macros
    KbdMacroStart,
    KbdMacroEnd,
    KbdMacroExecute,

    // Lifecycle
    Quit,
    Suspend,

    // Test / headless
    Wait(u64),
    Resize(u16, u16),
}
