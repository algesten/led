//! Chrome theme — colors + attributes for every paintable region.
//!
//! Types only; the painter in `driver-terminal/native` consumes
//! [`Theme`] to decide which ANSI escapes to emit, the runtime's
//! config loader builds one from TOML. Default values reproduce the
//! hard-coded chrome the painter used before M14b so unthemed goldens
//! stay pixel-identical.
//!
//! ABI is narrow on purpose: three opaque types ([`Color`], [`Attrs`],
//! [`Style`]) plus a [`Theme`] struct naming every region. A region
//! whose [`Style`] is the default produces no ANSI output, letting
//! the terminal's native fg / bg show through.

/// Terminal color, either an ANSI/xterm palette index or 24-bit RGB.
///
/// - `Indexed(0..=7)` — the 8 basic ANSI colors. Terminals honour
///   the user's configured palette for these, which is what most
///   users expect from "red" / "white" / etc.
/// - `Indexed(8..=15)` — the 8 bright variants.
/// - `Indexed(16..=255)` — xterm 256-color cube + grayscale.
/// - `Rgb(r, g, b)` — 24-bit truecolor. Only reliable on terminals
///   that advertise `COLORTERM=truecolor`; prefer `Indexed` for
///   defaults.
///
/// led's built-in theme uses `Indexed` throughout so it renders on
/// any 256-color terminal and respects the user's basic palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Color {
    Indexed(u8),
    Rgb { r: u8, g: u8, b: u8 },
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::Rgb { r, g, b }
    }

    pub const fn indexed(i: u8) -> Self {
        Self::Indexed(i)
    }

    // ── Named palette — ANSI indices 0-15 ──────────────────────
    //
    // Using indexed colors (not fixed RGB) means terminals use the
    // user's configured palette. A theme.toml that writes `"red"`
    // renders as whatever the user's terminal calls red, same as
    // legacy led.
    pub const BLACK: Self = Self::Indexed(0);
    pub const RED: Self = Self::Indexed(1);
    pub const GREEN: Self = Self::Indexed(2);
    pub const YELLOW: Self = Self::Indexed(3);
    pub const BLUE: Self = Self::Indexed(4);
    pub const MAGENTA: Self = Self::Indexed(5);
    pub const CYAN: Self = Self::Indexed(6);
    pub const WHITE: Self = Self::Indexed(7);
    pub const DARK_GREY: Self = Self::Indexed(8); // aka bright_black
    pub const BRIGHT_RED: Self = Self::Indexed(9);
    pub const BRIGHT_GREEN: Self = Self::Indexed(10);
    pub const BRIGHT_YELLOW: Self = Self::Indexed(11);
    pub const BRIGHT_BLUE: Self = Self::Indexed(12);
    pub const BRIGHT_MAGENTA: Self = Self::Indexed(13);
    pub const BRIGHT_CYAN: Self = Self::Indexed(14);
    pub const BRIGHT_WHITE: Self = Self::Indexed(15);
    /// Alias for `DARK_GREY` — matches the named color both
    /// `"grey"` / `"gray"` resolve to in theme.toml.
    pub const GREY: Self = Self::DARK_GREY;
}

/// Boolean attribute flags. Additive with fg / bg — e.g. `Attrs
/// { bold: true, .. }` on a region with no explicit colors means "use
/// terminal default fg / bg, but bold".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Attrs {
    pub bold: bool,
    pub reverse: bool,
    pub underline: bool,
}

impl Attrs {
    pub const REVERSE: Self = Self {
        bold: false,
        reverse: true,
        underline: false,
    };

    /// True when at least one attribute is set.
    pub const fn is_empty(&self) -> bool {
        !self.bold && !self.reverse && !self.underline
    }
}

/// One painted region's style. `None` fg / bg means "leave terminal
/// default"; the painter emits no SetForeground / SetBackground then.
/// Attributes are additive — an explicit `reverse: true` combines
/// with whatever fg / bg the style carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub attrs: Attrs,
}

impl Style {
    /// Convenience for the common "just invert" case used by the
    /// pre-M14b painter for active tabs + selected side-panel rows.
    pub const REVERSE: Self = Self {
        fg: None,
        bg: None,
        attrs: Attrs::REVERSE,
    };

    /// `true` when this style would emit no ANSI — the painter can
    /// skip the Set* / Reset pair entirely and leave the rendered
    /// glyphs with terminal default styling.
    pub const fn is_default(&self) -> bool {
        self.fg.is_none() && self.bg.is_none() && self.attrs.is_empty()
    }
}

/// The full chrome theme. Every paintable region has a named slot so
/// the painter looks up by field access (zero allocation, no maps).
///
/// Per the M14b roadmap: syntax highlighting lives in M15 and extends
/// this with a `syntax` table of its own; M14b ships the chrome-only
/// slots below.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    // ── Tabs ────────────────────────────────────────────────
    pub tab_active: Style,
    pub tab_inactive: Style,
    pub tab_preview: Style,
    pub tab_dirty_marker: Style,

    // ── Status bar ──────────────────────────────────────────
    pub status_normal: Style,
    pub status_warn: Style,

    // ── Side panel ──────────────────────────────────────────
    /// Selected row while focus lives on the side panel.
    pub browser_selected_focused: Style,
    /// Selected row when focus lives in the editor — dimmer than
    /// focused so the user can tell which pane owns their input.
    pub browser_selected_unfocused: Style,
    pub browser_chevron: Style,
    pub browser_border: Style,

    // ── File-search overlay toggles ─────────────────────────
    //
    // The three toggle glyphs (`Aa`, `.*`, `=>`) each have an "on"
    // style applied when their corresponding flag is set. Off state
    // uses the default (plain) style — no field needed.
    pub search_toggle_on: Style,

    // ── Editor body ─────────────────────────────────────────
    /// Background applied to the row the cursor is on. Default (no
    /// bg) is fine — the terminal's native cursor-row highlighting
    /// takes over.
    pub cursor_line: Style,
    /// Ruler column. Renders as a single-column background strip
    /// down the editor body. Only drawn when `ruler_column` is set.
    pub ruler: Style,
    /// Column index (0-based, editor-relative) where the ruler
    /// paints. `None` → no ruler.
    pub ruler_column: Option<u16>,
}

// Built-in palette — the exact xterm 256-color indices that legacy
// led's `default_theme.toml` used:
//
//   theme_dark      = x024  (deep blue, #005faf)
//   theme_bright    = x216  (peach,     #ffaf87)
//   theme_bold      = x223  (pale yellow, #ffd7af)
//   inverse fg      = x232  (near-black, #080808)
//   inactive bg     = x238  (dark grey,  #444444)
//   ruler bg        = x236  (ruler grey, #303030)
//
// Using `Color::Indexed` (not `Color::Rgb`) means we emit
// `ESC[38;5;Nm` / `ESC[48;5;Nm` escapes, which every 256-color
// terminal renders consistently. Truecolor's not universal — the
// rewrite ships the same look as legacy did.
const PEACH: Color = Color::Indexed(216);
const DEEP_BLUE: Color = Color::Indexed(24);
const PALE_YELLOW: Color = Color::Indexed(223);
const NEAR_BLACK: Color = Color::Indexed(232);
const INACTIVE_GREY: Color = Color::Indexed(238);
const RULER_GREY: Color = Color::Indexed(236);

impl Default for Theme {
    /// Built-in chrome. Colored end-to-end so an unthemed led ships
    /// with a coherent look — not "unstyled" or "whatever the
    /// terminal defaults to". Users overlay their own palette via
    /// `theme.toml` or `--theme`.
    fn default() -> Self {
        let inverse_active = Style {
            fg: Some(NEAR_BLACK),
            bg: Some(PEACH),
            ..Style::default()
        };
        Self {
            tab_active: inverse_active,
            tab_inactive: Style {
                fg: Some(PEACH),
                bg: Some(INACTIVE_GREY),
                ..Style::default()
            },
            tab_preview: Style::default(),
            tab_dirty_marker: Style::default(),

            status_normal: Style {
                fg: Some(PALE_YELLOW),
                bg: Some(DEEP_BLUE),
                ..Style::default()
            },
            status_warn: Style {
                fg: Some(Color::WHITE),
                bg: Some(Color::RED),
                attrs: Attrs {
                    bold: true,
                    ..Attrs::default()
                },
            },

            browser_selected_focused: inverse_active,
            browser_selected_unfocused: Style {
                bg: Some(INACTIVE_GREY),
                ..Style::default()
            },
            browser_chevron: Style::default(),
            browser_border: Style {
                fg: Some(DEEP_BLUE),
                ..Style::default()
            },

            search_toggle_on: inverse_active,

            cursor_line: Style::default(),
            ruler: Style {
                bg: Some(RULER_GREY),
                ..Style::default()
            },
            // No ruler by default — users opt in from theme.toml
            // with a `ruler_column = N` under `[chrome]`. Rendering
            // it automatically would pick a number that'd surprise
            // users of different editor widths (sidebar on/off).
            ruler_column: None,
        }
    }
}
