use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::file_status::{FileStatus, LineStatus};

pub type Waker = Arc<dyn Fn() + Send + Sync>;

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
    },
    PreviewFile {
        path: PathBuf,
        row: usize,
        col: usize,
        match_len: usize,
    },
    PreviewClosed,
    ConfirmSearch {
        path: PathBuf,
        row: usize,
        col: usize,
    },
    FindFileOpened { dir: PathBuf },
    FileSaved(PathBuf),
}

pub enum Effect {
    Emit(Event),
    Spawn(Box<dyn super::Component>),
    SetMessage(String),
    FocusPanel(PanelSlot),
    ConfirmAction { prompt: String, action: Action },
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
}
