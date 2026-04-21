//! Sync core of the terminal driver — strictly isolated.
//!
//! Knows only about its own atom ([`Terminal`]), the mirror types the
//! async reader translates crossterm events into ([`TermEvent`] /
//! [`KeyEvent`] / ...), the render view-models [`paint`] consumes
//! ([`Frame`], [`TabBarModel`], [`BodyModel`]), and the sync
//! [`TerminalInputDriver::process`] API.
//!
//! **Nothing** here references other drivers, `state-tabs`, or the
//! runtime. The memo that builds a `Frame` from multiple drivers'
//! atoms lives in `led-runtime`.

use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

mod theme;
pub use theme::{Attrs, Color, Style, Theme};

// ── Mirror types — the ABI boundary ────────────────────────────────────

/// Viewport size in columns × rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dims {
    pub cols: u16,
    pub rows: u16,
}

/// Screen-coordinate rectangle. Inclusive `x` / `y`, exclusive
/// `x + cols` / `y + rows`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

/// Pre-computed layout for a tick: where each chrome region goes.
/// Painter consumes this; no painter code touches `dims` directly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub dims: Dims,
    pub side_area: Option<Rect>,
    pub side_border_x: Option<u16>,
    pub editor_area: Rect,
    pub tab_bar: Rect,
    pub status_bar: Rect,
}

impl Layout {
    /// Produce a layout for the given terminal dims. The side panel
    /// gets a 25-col budget total (24 cols of content + 1 col
    /// border at the right edge), matching legacy goldens.
    /// `browser_visible` + sufficient terminal width are required
    /// for the panel to show.
    pub fn compute(dims: Dims, browser_visible: bool) -> Self {
        const SIDE_TOTAL: u16 = 25; // content + border
        const SIDE_CONTENT: u16 = SIDE_TOTAL - 1;
        const MIN_EDITOR_WIDTH: u16 = 25;
        let body_rows = dims.rows.saturating_sub(2);

        let side_visible = browser_visible
            && dims.cols > SIDE_TOTAL
            && dims.cols.saturating_sub(SIDE_TOTAL) >= MIN_EDITOR_WIDTH;
        let (side_area, side_border_x, editor_x) = if side_visible {
            (
                Some(Rect {
                    x: 0,
                    y: 0,
                    cols: SIDE_CONTENT,
                    rows: body_rows,
                }),
                Some(SIDE_CONTENT),
                SIDE_TOTAL,
            )
        } else {
            (None, None, 0)
        };
        let editor_cols = dims.cols.saturating_sub(editor_x);
        let editor_area = Rect {
            x: editor_x,
            y: 0,
            cols: editor_cols,
            rows: body_rows,
        };
        let tab_bar = Rect {
            x: editor_x,
            y: body_rows,
            cols: editor_cols,
            rows: 1,
        };
        let status_bar = Rect {
            x: 0,
            y: dims.rows.saturating_sub(1),
            cols: dims.cols,
            rows: 1,
        };
        Self {
            dims,
            side_area,
            side_border_x,
            editor_area,
            tab_bar,
            status_bar,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermEvent {
    Key(KeyEvent),
    Resize(Dims),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Esc,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct KeyModifiers(u8);

impl KeyModifiers {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(0b001);
    pub const CONTROL: Self = Self(0b010);
    pub const ALT: Self = Self(0b100);

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for KeyModifiers {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

// ── Atom ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Terminal {
    /// Current viewport. `None` until the first resize event lands.
    pub dims: Option<Dims>,

    /// Scratch queue the input side appends to and dispatch drains.
    pub pending: VecDeque<TermEvent>,
}

// ── Render view-models ─────────────────────────────────────────────────
//
// These are the shapes `paint` consumes. Building them — from whichever
// driver atoms contribute — is the runtime's job; this crate only owns
// the *types*.

/// Tab-bar labels. `labels` is wrapped in `Arc` so cache-hit clones of
/// [`TabBarModel`] (and its containing [`Frame`]) are a pointer copy
/// rather than a deep clone of every label per tick.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TabBarModel {
    pub labels: Arc<Vec<String>>,
    pub active: Option<usize>,
}

/// Body view. All owned-string fields use `Arc<str>` / `Arc<Vec<String>>`
/// so drv cache-hit clones (which happen on every idle tick through
/// `render_frame`) never deep-copy the content.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum BodyModel {
    #[default]
    Empty,
    Pending {
        path_display: Arc<str>,
    },
    Error {
        path_display: Arc<str>,
        message: Arc<str>,
    },
    Content {
        lines: Arc<Vec<String>>,
        /// Body-relative cursor position `(row, col)`. `None` when the
        /// cursor is outside the visible scroll window (defensive — the
        /// runtime's scroll invariant should keep it in view).
        cursor: Option<(u16, u16)>,
    },
}

/// Side-panel row. `chevron` is `None` for files, `Some(true)` for
/// expanded dirs (▽), `Some(false)` for collapsed dirs (▷). `depth`
/// indent and `selected` highlighting are resolved in the painter.
///
/// `match_range` is `(char_start, char_end)` inside `name` — set on
/// file-search hit rows so the painter can highlight the matched
/// substring with `theme.search_match`. Skipped when the row is
/// selected (selection style wins end-to-end).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidePanelRow {
    pub depth: u16,
    pub chevron: Option<bool>,
    pub name: Arc<str>,
    pub selected: bool,
    pub match_range: Option<(u16, u16)>,
}

/// Which kind of content the side panel is displaying.
///
/// - `Browser` = file-tree view (chevron column, 2-col indent).
/// - `Completions` = find-file list (no chevron, no indent).
/// - `FileSearch` = project-wide search overlay; the header row
///   (row 0) is repainted by the driver with per-toggle styling
///   based on these flags so users see which of `Aa` / `.*` / `=>`
///   are active.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SidePanelMode {
    #[default]
    Browser,
    Completions,
    FileSearch {
        case_sensitive: bool,
        use_regex: bool,
        replace_mode: bool,
    },
}

/// Side-panel slice of the render frame. Pre-sliced to the visible
/// window (`rows` entries long; caller has already done the scroll
/// clamp). Wrapped in `Arc` so cache-hit clones of [`Frame`] are a
/// pointer copy.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SidePanelModel {
    pub rows: Arc<Vec<SidePanelRow>>,
    pub focused: bool,
    pub mode: SidePanelMode,
}

/// Bottom-row status bar. `left` is written from col 0; `right` is
/// written right-aligned; the gap is cleared. `is_warn` asks the
/// painter to use the warn (red-bg / white-fg / bold) style for the
/// whole row.
///
/// Both strings are `Arc<str>` so cache-hit clones are a pointer
/// copy even when nothing on the status bar changed.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct StatusBarModel {
    pub left: Arc<str>,
    pub right: Arc<str>,
    pub is_warn: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Frame {
    pub tab_bar: TabBarModel,
    pub body: BodyModel,
    pub status_bar: StatusBarModel,
    pub side_panel: Option<SidePanelModel>,
    pub layout: Layout,
    /// Absolute terminal cursor position as `(col, row)` — matches
    /// crossterm's `cursor::MoveTo` argument order. `None` hides the
    /// cursor (no active content / cursor scrolled away).
    pub cursor: Option<(u16, u16)>,
    pub dims: Dims,
}

// ── Trace ──────────────────────────────────────────────────────────────

/// `--golden-trace` hook for terminal events emitted by the driver.
pub trait Trace: Send + Sync {
    fn key_in(&self, ev: &KeyEvent);
    fn resize(&self, dims: Dims);
    /// Called by [`TerminalOutputDriver::execute`] before a paint.
    /// (Post-course-correct #3: the trace emission moved inside the
    /// driver.)
    fn render_tick(&self);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn key_in(&self, _: &KeyEvent) {}
    fn resize(&self, _: Dims) {}
    fn render_tick(&self) {}
}

// ── Sync driver API ────────────────────────────────────────────────────

/// The main-loop-facing half of the input driver.
///
/// Constructed with the receiving end of a `TermEvent` channel. The
/// sender is owned by the async worker (desktop: a crossterm reader
/// thread in `*-native`; mobile: a UI-thread bridge).
pub struct TerminalInputDriver {
    rx: Receiver<TermEvent>,
    trace: Arc<dyn Trace>,
}

impl TerminalInputDriver {
    pub fn new(rx: Receiver<TermEvent>, trace: Arc<dyn Trace>) -> Self {
        Self { rx, trace }
    }

    /// Drain events into the `Terminal` atom. Resize additionally
    /// applies directly to `dims` (pure state, no dispatch needed);
    /// both variants land in `pending` for dispatch.
    pub fn process(&self, term: &mut Terminal) {
        while let Ok(ev) = self.rx.try_recv() {
            match &ev {
                TermEvent::Key(k) => self.trace.key_in(k),
                TermEvent::Resize(d) => {
                    self.trace.resize(*d);
                    term.dims = Some(*d);
                }
            }
            term.pending.push_back(ev);
        }
    }
}

#[cfg(test)]
mod tests {
    //! Strictly self-contained: Terminal atom + its sync driver only.

    use super::*;
    use std::sync::mpsc;

    #[test]
    fn input_driver_applies_resize_and_queues_key() {
        let (tx, rx) = mpsc::channel::<TermEvent>();
        let driver = TerminalInputDriver::new(rx, Arc::new(NoopTrace));

        let mut term = Terminal::default();
        assert!(term.dims.is_none());

        tx.send(TermEvent::Resize(Dims { cols: 80, rows: 24 }))
            .unwrap();
        tx.send(TermEvent::Key(KeyEvent {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::NONE,
        }))
        .unwrap();

        driver.process(&mut term);

        assert_eq!(term.dims, Some(Dims { cols: 80, rows: 24 }));
        assert_eq!(term.pending.len(), 2);
    }

    #[test]
    fn input_driver_drains_multiple_events() {
        let (tx, rx) = mpsc::channel::<TermEvent>();
        let driver = TerminalInputDriver::new(rx, Arc::new(NoopTrace));
        let mut term = Terminal::default();

        for _ in 0..3 {
            tx.send(TermEvent::Key(KeyEvent {
                code: KeyCode::Char('x'),
                modifiers: KeyModifiers::NONE,
            }))
            .unwrap();
        }
        driver.process(&mut term);
        assert_eq!(term.pending.len(), 3);

        // Second call drains nothing new.
        driver.process(&mut term);
        assert_eq!(term.pending.len(), 3);
    }
}
