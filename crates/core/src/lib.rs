use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

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
}

// ---------------------------------------------------------------------------
// Panel system
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelSlot {
    Main,
    Side,
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
}

// ---------------------------------------------------------------------------
// Events & Effects
// ---------------------------------------------------------------------------

pub enum Event {
    OpenFile(PathBuf),
    TabActivated { path: Option<PathBuf> },
    Resume,
}

pub enum Effect {
    Emit(Event),
    Spawn(Box<dyn Component>),
    SetMessage(String),
    FocusPanel(PanelSlot),
    SetClipboard(Arc<String>),
    ConfirmAction { prompt: String, action: Action },
    Quit,
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

pub struct Context<'a> {
    pub db: Option<&'a Connection>,
    pub root: &'a std::path::Path,
    pub viewport_height: usize,
    pub yank_fn: Option<&'a mut dyn FnMut() -> Option<String>>,
    pub waker: Option<Waker>,
}

impl<'a> Context<'a> {
    pub fn yank(&mut self) -> Option<String> {
        self.yank_fn.as_mut().and_then(|f| f())
    }
}

pub struct DrawContext<'a> {
    pub theme: &'a Theme,
    pub focused: bool,
}

// ---------------------------------------------------------------------------
// Theme / ElementStyle
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ElementStyle {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub reversed: bool,
}

impl ElementStyle {
    pub fn to_style(&self) -> Style {
        let mut s = Style::default().fg(self.fg).bg(self.bg);
        if self.bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.reversed {
            s = s.add_modifier(Modifier::REVERSED);
        }
        s
    }
}

pub const BLANK_STYLE: ElementStyle = ElementStyle {
    fg: Color::Reset,
    bg: Color::Reset,
    bold: false,
    reversed: false,
};

#[derive(Clone)]
pub struct Theme {
    styles: HashMap<String, ElementStyle>,
}

impl Theme {
    pub fn new() -> Self {
        Self { styles: HashMap::new() }
    }

    pub fn get(&self, key: &str) -> ElementStyle {
        self.styles.get(key).cloned().unwrap_or(BLANK_STYLE)
    }

    pub fn set(&mut self, key: String, style: ElementStyle) {
        self.styles.insert(key, style);
    }
}

// ---------------------------------------------------------------------------
// Component trait
// ---------------------------------------------------------------------------

pub trait Component: std::any::Any {
    fn as_any(&self) -> &dyn std::any::Any;
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;

    fn panel_claims(&self) -> &[PanelClaim];

    fn tab(&self) -> Option<TabDescriptor> {
        None
    }

    fn handle_action(&mut self, action: Action, ctx: &mut Context) -> Vec<Effect>;

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> Vec<Effect>;

    fn draw(&mut self, frame: &mut Frame, area: Rect, ctx: &DrawContext);

    /// Cursor position (row, col) within the component — used by the shell for cursor placement.
    fn cursor_position(&self) -> Option<(usize, usize)> {
        None
    }

    /// Absolute screen position (x, y) for the cursor — computed during draw().
    fn cursor_screen_pos(&self) -> Option<(u16, u16)> {
        None
    }

    /// Scroll offset — used by the shell for scroll computation.
    fn scroll_offset(&self) -> usize {
        0
    }

    /// Set scroll offset after shell computes it.
    fn set_scroll_offset(&mut self, _offset: usize) {}

    /// Status bar info: (label, line, col) — used by the shell for status bar rendering.
    fn status_info(&self) -> Option<(&str, usize, usize)> {
        None
    }

    fn save_session(&self, ctx: &Context);

    fn restore_session(&mut self, ctx: &mut Context);

    fn needs_flush(&self) -> bool {
        false
    }

    fn flush(&mut self, _ctx: &mut Context) {}

    fn notify_hash(&self) -> Option<String> {
        None
    }

    fn handle_notification(&mut self, _ctx: &mut Context) {}

    /// TOML fragment for this component's default theme styles.
    fn default_theme_toml(&self) -> &'static str {
        ""
    }
}
