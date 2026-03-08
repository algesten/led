use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use rusqlite::Connection;

use crate::{
    Action, Clipboard, Effect, Event, FileStatusStore, LspStatus, PanelClaim, PanelSlot,
    TabDescriptor, Waker,
};

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
    pub cursor_pos: Option<(u16, u16)>,
    pub slot: PanelSlot,
    pub file_statuses: &'a FileStatusStore,
    pub lsp_status: Option<&'a LspStatus>,
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

/// A Component is a self-contained UI unit managed by the Shell orchestrator.
///
/// # Architecture rules
///
/// The Shell MUST NOT downcast components or call any methods beyond this trait.
/// All communication flows through three channels:
///
///   Action  (shell → component)  — user input and lifecycle signals
///   Event   (broadcast)          — inter-component notifications via Effect::Emit
///   Effect  (component → shell)  — requests back to the shell (spawn, focus, message, etc.)
///
/// To add new behaviour, use these channels — never add shell code that operates
/// on a specific component type. If a component needs to expose state to the shell
/// (e.g. dirty flag), do it through `tab()` metadata or a new Effect variant.
///
/// # Lifecycle actions
///
/// The shell dispatches these Action variants to components. Implementations of
/// `handle_action` must handle them without disrupting component-internal modes
/// (e.g. do not exit an interactive search when Tick arrives):
///
///   Tick            — periodic timer, used for polling async results
///   FocusGained     — this component's panel just received focus
///   FocusLost       — this component's panel just lost focus
///   SaveSession     — persist state to ctx.kv
///   RestoreSession  — restore state from ctx.kv / ctx.db
///   Flush           — write pending changes to the database
///
/// # Panel claims
///
/// Components declare which panel slots they can draw in via `panel_claims()`.
/// The shell picks the highest-priority claimer for each slot. A single component
/// can claim multiple slots (e.g. Main + StatusBar) and `draw()` will be called
/// once per slot with `ctx.slot` indicating which one.
///
/// Claims may be dynamic (e.g. toggling a status bar overlay), but the returned
/// slice must not allocate — store pre-built slices on the struct.
///
/// # Drawing
///
/// `draw()` receives a `DrawContext` with a mutable `cursor_pos` field. If the
/// component wants the terminal cursor placed, write to `ctx.cursor_pos`. The
/// shell reads this after draw and positions the cursor accordingly.
pub trait Component {
    fn panel_claims(&self) -> &[PanelClaim];

    fn tab(&self) -> Option<TabDescriptor> {
        None
    }

    fn handle_action(&mut self, action: Action, ctx: &mut Context) -> Vec<Effect>;

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> Vec<Effect>;

    fn draw(&mut self, frame: &mut Frame, area: Rect, ctx: &mut DrawContext);

    /// Keymap context name used when this component has focus.
    fn context_name(&self) -> Option<&str> {
        None
    }

    /// Current cursor position (row, col, scroll_offset) for jump-list recording.
    fn cursor_position(&self) -> Option<(usize, usize, usize)> {
        None
    }
}
