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
pub use theme::{Attrs, Color, DiagnosticsTheme, Style, SyntaxTheme, Theme};

// ── Mirror types — the ABI boundary ────────────────────────────────────

/// Viewport size in columns × rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, drv::Input)]
pub struct Dims {
    pub cols: u16,
    pub rows: u16,
}

/// Screen-coordinate rectangle. Inclusive `x` / `y`, exclusive
/// `x + cols` / `y + rows`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, drv::Input)]
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

/// Body view. All owned-string fields use `Arc<str>` / `Arc<Vec<BodyLine>>`
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
        lines: Arc<Vec<BodyLine>>,
        /// Body-relative cursor position `(row, col)`. `None` when the
        /// cursor is outside the visible scroll window (defensive — the
        /// runtime's scroll invariant should keep it in view).
        cursor: Option<(u16, u16)>,
        /// When set, the painter overlays `theme.search_match` on a
        /// single run of characters inside one visible row. Used by
        /// the file-search preview to show "this is the hit you're
        /// looking at" on top of the buffer, mirroring the sidebar
        /// highlight.
        match_highlight: Option<BodyMatch>,
    },
}

/// One rendered body row: the already-truncated row text plus
/// the list of syntax token spans inside it, plus any diagnostic
/// markers that land on this row. Spans' and diagnostics'
/// `col_*` values are relative to the start of `text` (gutter
/// included).
///
/// `gutter_diagnostic` is the highest-severity LSP diagnostic
/// whose range intersects this row — the painter colours the
/// second gutter cell (col 1) with a ● in the matching
/// `theme.diagnostics.*` style.
///
/// `gutter_category` is the precedence-winning
/// [`led_core::IssueCategory`] across git line statuses and LSP
/// diagnostics for this row. The painter renders it as a left-
/// eighth-block bar (`▎`) in the first gutter cell (col 0)
/// coloured via `Theme::category_style`. `None` leaves col 0
/// blank. The two fields are intentionally parallel — M19
/// matches legacy's side-by-side gutter, keeping the LSP dot at
/// col 1 even when col 0 already shows the same colour.
///
/// `diagnostics` are the per-row underline ranges (in column
/// coordinates). Empty vectors mean "no styling" and the painter
/// takes its default path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BodyLine {
    pub text: String,
    pub spans: Vec<LineSpan>,
    pub gutter_diagnostic: Option<led_state_diagnostics::DiagnosticSeverity>,
    pub gutter_category: Option<led_core::IssueCategory>,
    pub diagnostics: Vec<BodyDiagnostic>,
}

/// A single diagnostic underline on one body row. Coordinates are
/// in column units relative to the row's `text` (so gutter
/// offset is already applied upstream). `col_end` is exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyDiagnostic {
    pub col_start: u16,
    pub col_end: u16,
    pub severity: led_state_diagnostics::DiagnosticSeverity,
}

impl From<String> for BodyLine {
    fn from(text: String) -> Self {
        Self {
            text,
            spans: Vec::new(),
            gutter_diagnostic: None,
            gutter_category: None,
            diagnostics: Vec::new(),
        }
    }
}

impl From<&str> for BodyLine {
    fn from(text: &str) -> Self {
        Self::from(text.to_string())
    }
}

/// One syntax-highlighted run inside a [`BodyLine`]. `col_start` is
/// inclusive and `col_end` is exclusive. Character columns, not
/// byte offsets — the painter skips/takes chars to slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineSpan {
    pub col_start: u16,
    pub col_end: u16,
    pub kind: led_state_syntax::TokenKind,
}

/// One currently-previewed search hit, expressed in body-visible
/// coordinates (post-scroll, post-gutter). The painter doesn't need
/// to know about the rope or the scroll offset — everything is
/// pre-resolved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BodyMatch {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
}

/// Side-panel row. `chevron` is `None` for files, `Some(true)` for
/// expanded dirs (▽), `Some(false)` for collapsed dirs (▷). `depth`
/// indent and `selected` highlighting are resolved in the painter.
///
/// `match_range` is `(char_start, char_end)` inside `name` — set on
/// file-search hit rows so the painter can highlight the matched
/// substring with `theme.search_match`. Skipped when the row is
/// selected (selection style wins end-to-end).
///
/// `replaced` is set on file-search hit rows the user has already
/// applied a per-hit replace to (Right-arrow). The painter styles
/// these rows dimly via `theme.search_hit_replaced` so they're
/// visibly distinct from pending rows — the user can Left-arrow
/// back onto any of them to undo that specific replace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidePanelRow {
    pub depth: u16,
    pub chevron: Option<bool>,
    pub name: Arc<str>,
    pub selected: bool,
    pub match_range: Option<(u16, u16)>,
    pub replaced: bool,
    /// Winning [`IssueCategory`] for this row + the letter the
    /// painter draws in the right-aligned status column. For files:
    /// the row's own categories resolved via `resolve_display`. For
    /// directories: the union of every descendant file's categories
    /// via `directory_categories`, resolved the same way.
    ///
    /// `None` = no category → no coloured name, no status glyph.
    ///
    /// The painter matches on the `IssueCategory` to pick a style
    /// (`Theme::category_style`) and uses `letter` verbatim for the
    /// right-column glyph (a bullet `•` for letterless categories,
    /// always a bullet for directories — the resolver embeds that
    /// fallback). Mirrors legacy's `StatusDisplay` in shape.
    ///
    /// [`IssueCategory`]: led_core::IssueCategory
    pub status: Option<RowStatus>,
}

/// Displayable status for one browser row. The split from
/// [`SidePanelRow`] lets the memo memoize Option<RowStatus>
/// cheaply without pointer-comparing letters and categories
/// separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowStatus {
    pub category: led_core::IssueCategory,
    pub letter: char,
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

/// Floating popover anchored near the cursor — currently the
/// LSP diagnostic hover (one box per cursor-line diagnostic).
///
/// The popover floats above body content; it doesn't push any
/// other region aside. The painter draws a solid-fill box (no
/// border) with one `PopoverLine` per row. Multiple messages are
/// separated by a horizontal rule line the painter inserts
/// between entries. Positioning: prefer above the anchor row,
/// fall back to below when there isn't room; X clamps to stay
/// on-screen.
///
/// `lines` is `Arc`-wrapped so cache-hit `Frame` clones stay a
/// pointer copy.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PopoverModel {
    pub lines: Arc<Vec<PopoverLine>>,
    /// Absolute terminal `(col, row)` anchor — the cursor
    /// position that triggered the popover. The painter derives
    /// the actual box origin from this with the screen-edge clamp
    /// and above/below fallback rules described on [`PopoverModel`].
    pub anchor: (u16, u16),
}

/// One row inside a [`PopoverModel`]. `text` is already wrapped
/// to the popover width; the painter does not re-wrap.
///
/// The painter resolves `severity` against `theme.diagnostics.*`
/// at paint time — the model carries the domain enum, not a
/// `Style`, so theme changes don't require a frame rebuild.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopoverLine {
    pub text: Arc<str>,
    /// Which theme colour to use. `None` marks a separator row
    /// (horizontal rule between messages); painter draws ─ fill.
    pub severity: Option<PopoverSeverity>,
}

/// Mirror of `led_state_diagnostics::DiagnosticSeverity` without
/// the cross-crate dependency. Driver-core intentionally depends
/// on no state crates; the runtime translates before emitting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopoverSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Frame {
    pub tab_bar: TabBarModel,
    pub body: BodyModel,
    pub status_bar: StatusBarModel,
    pub side_panel: Option<SidePanelModel>,
    pub popover: Option<PopoverModel>,
    /// LSP completion popup (M17). Disjoint from the diagnostic
    /// `popover` because the two have different UX and can
    /// theoretically co-exist; painter draws the completion on
    /// top so it wins visually. `None` when no session is live.
    pub completion: Option<CompletionPopupModel>,
    pub layout: Layout,
    /// Absolute terminal cursor position as `(col, row)` — matches
    /// crossterm's `cursor::MoveTo` argument order. `None` hides the
    /// cursor (no active content / cursor scrolled away).
    pub cursor: Option<(u16, u16)>,
    pub dims: Dims,
}

/// Visible state for an LSP completion popup.
///
/// The memo (`query::completion_popup_model`) builds this when
/// `CompletionsState.session` is `Some`, stamping in the
/// server-provided label / detail columns and the popup's
/// anchor / selected-row state. The painter flips the popup
/// above the cursor when it would overflow the body bottom,
/// and clamps it to the editor area horizontally.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CompletionPopupModel {
    /// Visible rows (already windowed to `scroll..scroll + N`).
    /// Each row carries its label + optional detail; the
    /// painter pads to column widths at render time.
    pub rows: Arc<Vec<CompletionRow>>,
    /// Index into `rows` of the highlighted item. Painter draws
    /// this row with the theme's selection style.
    pub selected: usize,
    /// Absolute terminal `(col, row)` anchor — the cursor
    /// position that's driving the session. Painter chooses
    /// "below the anchor" first, "above" on overflow.
    pub anchor: (u16, u16),
    /// Max width (in chars) of any row's label — the painter
    /// left-pads every label to this before printing the
    /// detail column.
    pub label_width: u16,
    /// Max width of any row's detail. Used alongside
    /// `label_width` to compute the popup's outer width.
    pub detail_width: u16,
}

/// One visible row in [`CompletionPopupModel`]. `label` is the
/// primary identifier, `detail` the optional right-side hint
/// (signature / type / module). Both pre-trimmed by the memo
/// so the painter never re-measures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionRow {
    pub label: Arc<str>,
    pub detail: Option<Arc<str>>,
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
