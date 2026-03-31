mod alert;
mod config;
mod doc;
pub mod git;
pub mod keys;
mod language;
pub mod rx;
pub mod theme;
mod watch;
pub mod wrap;

mod versioned;

pub use alert::{Alert, AlertExt};
pub use config::Startup;
pub use doc::{Doc, EditOp, InertDoc, TextDoc, UndoEntry, UndoHistory, apply_op_to_doc};
pub use language::{LanguageId, LspContextId};
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
    /// Tab display order (0-based).
    TabOrder,
}

newtype_u64! {
    /// Monotonic document version (increments on every insert/remove).
    DocVersion,
    /// Save generation counter (increments on every save).
    SaveSeq,
    /// Monotonic change sequence (increments on every observable buffer change).
    ChangeSeq,
    /// Content hash for comparing on-disk vs in-memory state.
    ContentHash,
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
