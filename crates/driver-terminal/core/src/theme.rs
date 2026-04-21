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

/// 24-bit RGB color. Named palette colors resolve to these at parse
/// time — the painter never sees names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Legacy ANSI palette. Exact values match crossterm's
    /// `Color::Red` / `Color::White` / etc. so a `theme.toml` that
    /// names `red` renders the same as today's hard-coded
    /// `Color::Red`.
    pub const BLACK: Self = Self::rgb(0, 0, 0);
    pub const RED: Self = Self::rgb(205, 0, 0);
    pub const GREEN: Self = Self::rgb(0, 205, 0);
    pub const YELLOW: Self = Self::rgb(205, 205, 0);
    pub const BLUE: Self = Self::rgb(0, 0, 238);
    pub const MAGENTA: Self = Self::rgb(205, 0, 205);
    pub const CYAN: Self = Self::rgb(0, 205, 205);
    pub const WHITE: Self = Self::rgb(229, 229, 229);
    pub const GREY: Self = Self::rgb(127, 127, 127);
    pub const DARK_GREY: Self = Self::rgb(64, 64, 64);
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

// Built-in palette. Matches led's long-standing look: peach accents
// for active chrome, deep blue muted gutter / border / status bar, a
// pale-yellow status foreground, dark-grey bg for inactive /
// unfocused highlights. Hex values mirror the xterm 256-color indices
// the legacy `default_theme.toml` referenced (x216 peach, x024 deep
// blue, x223 pale yellow, x232 near-black, x238 dark grey, x236
// ruler grey).
const PEACH: Color = Color::rgb(0xff, 0xaf, 0x87);
const DEEP_BLUE: Color = Color::rgb(0x00, 0x5f, 0xaf);
const PALE_YELLOW: Color = Color::rgb(0xff, 0xd7, 0xaf);
const NEAR_BLACK: Color = Color::rgb(0x08, 0x08, 0x08);
const INACTIVE_GREY: Color = Color::rgb(0x44, 0x44, 0x44);
const RULER_GREY: Color = Color::rgb(0x30, 0x30, 0x30);

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
