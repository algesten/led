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

// ── Mirror types — the ABI boundary ────────────────────────────────────

/// Viewport size in columns × rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dims {
    pub cols: u16,
    pub rows: u16,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodyModel {
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

impl Default for BodyModel {
    fn default() -> Self {
        BodyModel::Empty
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Frame {
    pub tab_bar: TabBarModel,
    pub body: BodyModel,
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
