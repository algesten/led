use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use rusqlite::Connection;

use crate::{Action, Clipboard, Effect, Event, PanelClaim, TabDescriptor, Waker};

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

pub struct Context<'a> {
    pub db: Option<&'a Connection>,
    pub root: &'a std::path::Path,
    pub viewport_height: usize,
    pub clipboard: &'a dyn Clipboard,
    pub waker: Option<Waker>,
    pub kv: HashMap<String, String>,
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
        Self {
            styles: HashMap::new(),
        }
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

    fn focus_changed(&mut self, _focused: bool, _ctx: &mut Context) -> Vec<Effect> {
        vec![]
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

    fn ensure_schema(&self, _ctx: &Context) {}

    fn save_session(&self, ctx: &mut Context);

    fn restore_session(&mut self, ctx: &mut Context);

    fn needs_flush(&self) -> bool {
        false
    }

    fn flush(&mut self, _ctx: &mut Context) {}

    fn notify_hash(&self) -> Option<String> {
        None
    }

    fn handle_notification(&mut self, _ctx: &mut Context) {}

    /// Keymap context name used when this component has focus.
    fn context_name(&self) -> Option<&str> {
        None
    }
}
