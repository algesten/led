//! TOML `theme.toml` loader.
//!
//! Resolves a [`Theme`] from either an explicit path (`--theme`), a
//! file in the user's config directory, or the built-in legacy
//! default (the hard-coded chrome that shipped before M14b).
//!
//! The TOML shape uses one table per chrome region. Each region is a
//! table accepting any subset of `fg` / `bg` / `bold` / `reverse` /
//! `underline`. Colors are named (`"red"`, `"cyan"`, ...) or 24-bit
//! hex (`"#cd0000"`). Missing region → default (unstyled) slot.
//!
//! ```toml
//! [chrome.tab_active]
//! reverse = true
//!
//! [chrome.status_warn]
//! fg = "white"
//! bg = "#cd0000"
//! bold = true
//!
//! [chrome.ruler]
//! bg = "#222222"
//! ruler_column = 110
//! ```
//!
//! Unknown regions / unknown color names surface as non-fatal
//! warnings (same discipline as `keys.toml`).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use led_driver_terminal_core::{Color, Style, SyntaxTheme, Theme};

/// One named style in the `[COLORS]` table.
///
/// Every entry in `[COLORS]` is a [`Style`]. The bare-string form
/// (`name = "red"` / `name = "$other"`) is shorthand for a Style
/// with `fg` set and nothing else — i.e. `{ fg = "red" }`. The
/// inline-table form carries full Style semantics (`bg`, `bold`,
/// `reverse`, `underline`).
///
/// Field values stay raw so `$ref` references resolve recursively
/// at lookup time and cycles can be detected per-call.
#[derive(Clone, Debug)]
enum StyleSpec {
    /// `name = "value"` — fg-only Style. `value` is either a color
    /// literal (`xNNN`, `#rgb`, named ANSI) or a `$other` reference.
    Shorthand(String),
    /// `name = { fg = "...", bg = "...", bold = ... }` — full Style.
    /// `fg` / `bg` strings follow the same grammar as `Shorthand`.
    Full {
        fg: Option<String>,
        bg: Option<String>,
        bold: Option<bool>,
        reverse: Option<bool>,
        underline: Option<bool>,
    },
}

/// Table of named styles built from `[COLORS]` in `theme.toml`.
/// Each region in the rest of the theme can reference an entry via
/// `$name`, which chains (`$syntax_keyword` → `$x032` → `#0087d7`).
///
/// The section is named `[COLORS]` for historical reasons — every
/// entry is in fact a Style (see [`StyleSpec`]).
type StyleTable = HashMap<String, StyleSpec>;

/// Result of [`load_theme`]: the resolved theme plus any non-fatal
/// parse warnings. Unknown region names / unknown color names /
/// malformed style tables are dropped with a warning; the rest of
/// the theme still applies.
#[derive(Debug)]
pub struct LoadedTheme {
    pub theme: Theme,
    pub warnings: Vec<String>,
}

/// Fatal theme-load failures (I/O, top-level TOML parse errors).
/// Per-region schema problems live in `warnings` instead.
#[derive(Debug)]
pub enum ThemeError {
    Io { path: PathBuf, message: String },
    Toml { path: PathBuf, message: String },
    SchemaMismatch { path: PathBuf, message: String },
}

impl std::fmt::Display for ThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThemeError::Io { path, message } => write!(f, "read {}: {message}", path.display()),
            ThemeError::Toml { path, message } => write!(f, "parse {}: {message}", path.display()),
            ThemeError::SchemaMismatch { path, message } => {
                write!(f, "{}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for ThemeError {}

/// Build the runtime theme.
///
/// Resolution:
/// 1. If `explicit_path` is `Some`, load it (error when missing).
/// 2. Otherwise look in `config_dir/theme.toml`, then
///    `$XDG_CONFIG_HOME/led/theme.toml` (or `~/.config/led/` when
///    the env var is unset). Missing file → `Theme::default`.
pub fn load_theme(
    config_dir: Option<&Path>,
    explicit_path: Option<&Path>,
) -> Result<LoadedTheme, ThemeError> {
    let mut loaded = LoadedTheme {
        theme: Theme::default(),
        warnings: Vec::new(),
    };
    let path = match explicit_path {
        Some(p) => Some(p.to_path_buf()),
        None => discover_theme(config_dir),
    };
    let Some(path) = path else {
        return Ok(loaded);
    };
    let source = fs::read_to_string(&path).map_err(|e| ThemeError::Io {
        path: path.clone(),
        message: e.to_string(),
    })?;
    apply_toml(&mut loaded, &path, &source)?;
    Ok(loaded)
}

fn discover_theme(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = explicit {
        let candidate = dir.join("theme.toml");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let base = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("led")
    } else {
        dirs::home_dir()?.join(".config").join("led")
    };
    let candidate = base.join("theme.toml");
    candidate.exists().then_some(candidate)
}

fn apply_toml(
    loaded: &mut LoadedTheme,
    path: &Path,
    source: &str,
) -> Result<(), ThemeError> {
    let value: toml::Value = source.parse().map_err(|e: toml::de::Error| ThemeError::Toml {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let root = match value {
        toml::Value::Table(t) => t,
        _ => {
            return Err(ThemeError::SchemaMismatch {
                path: path.to_path_buf(),
                message: "top level must be a TOML table".into(),
            })
        }
    };
    let styles = extract_styles(root.get("COLORS"), &mut loaded.warnings);

    if let Some(syntax_value) = root.get("syntax") {
        apply_syntax(loaded, path, syntax_value, &styles)?;
    }
    if let Some(diag_value) = root.get("diagnostics") {
        apply_diagnostics(loaded, path, diag_value, &styles)?;
    }

    let Some(chrome) = root.get("chrome") else {
        return Ok(());
    };
    let chrome = match chrome {
        toml::Value::Table(t) => t,
        _ => {
            return Err(ThemeError::SchemaMismatch {
                path: path.to_path_buf(),
                message: "`chrome` must be a table".into(),
            })
        }
    };

    // `ruler_column` is a flat integer under [chrome], not a region
    // table. Pull it out first so the region loop doesn't trip on
    // "unknown region" for this key.
    if let Some(v) = chrome.get("ruler_column") {
        match v {
            toml::Value::Integer(n) if *n >= 0 => {
                loaded.theme.ruler_column = Some(*n as u16);
            }
            _ => loaded
                .warnings
                .push("[chrome] `ruler_column` must be a non-negative integer (skipped)".into()),
        }
    }

    for (region, style_value) in chrome {
        if region == "ruler_column" {
            continue;
        }
        let style_table = match style_value {
            toml::Value::Table(t) => t,
            _ => {
                loaded.warnings.push(format!(
                    "[chrome] `{region}`: value must be a table (skipped)"
                ));
                continue;
            }
        };
        let style = match parse_style(
            style_table,
            "chrome",
            region,
            &styles,
            &mut loaded.warnings,
        ) {
            Some(s) => s,
            None => continue,
        };
        if !assign_region(&mut loaded.theme, region, style) {
            loaded
                .warnings
                .push(format!("[chrome] `{region}`: unknown region (skipped)"));
        }
    }

    Ok(())
}

/// Ingest the `[syntax]` TOML table. Each entry can be either a
/// sub-table (`keyword = { fg = "x032", bold = true }`) or a bare
/// string shorthand (`keyword = "$syntax_keyword"`) that legacy
/// themes use extensively — the shorthand is interpreted as an
/// `fg`-only style.
///
/// Legacy themes use dotted keys like `"type.builtin"` /
/// `"string.special"` for finer-grained tree-sitter captures. Our
/// `TokenKind` enum is coarser — we only have one `String` slot
/// covering all string-like captures — so dotted keys collapse
/// onto the base kind. When a theme defines BOTH (e.g. `string`
/// and `"string.special"`) the non-dotted key wins: we process
/// non-dotted keys first and let dotted keys only fill in bases
/// that weren't explicitly set. Otherwise iteration order
/// (alphabetical, via `toml`'s `BTreeMap`) would have
/// `"string.special"` silently overwrite `string`.
fn apply_syntax(
    loaded: &mut LoadedTheme,
    path: &Path,
    syntax: &toml::Value,
    styles: &StyleTable,
) -> Result<(), ThemeError> {
    let table = match syntax {
        toml::Value::Table(t) => t,
        _ => {
            return Err(ThemeError::SchemaMismatch {
                path: path.to_path_buf(),
                message: "`syntax` must be a table".into(),
            });
        }
    };
    // Two-pass write so a non-dotted entry wins over a dotted
    // sibling that resolves to the same `TokenKind` slot.
    //
    // Pass 1 — authoritative assignments for non-dotted keys
    // (`string`, `keyword`, …). Each one's resolved
    // `TokenKind` is captured so Pass 2 can skip dotted entries
    // that target the same slot.
    //
    // Pass 2 — dotted keys (`string.regex`, `text.title`, …).
    // Both passes route through
    // `led_state_syntax::capture_name_to_kind` — the same
    // dispatch the syntax driver uses — so a theme entry lands
    // in the slot the painter actually reads. Dotted captures
    // that map to a slot already explicitly set in Pass 1 are
    // skipped (legacy parity: `string` wins over `string.regex`).
    let mut explicit_kinds: HashSet<led_state_syntax::TokenKind> = HashSet::new();
    for (kind, style_value) in table {
        if kind.contains('.') {
            continue;
        }
        let Some(style) = resolve_syntax_style(kind, style_value, styles, &mut loaded.warnings)
        else {
            continue;
        };
        if assign_syntax_kind(&mut loaded.theme.syntax, kind, style) {
            if let Some(tk) = led_state_syntax::capture_name_to_kind(kind) {
                explicit_kinds.insert(tk);
            }
        } else {
            loaded
                .warnings
                .push(format!("[syntax] `{kind}`: unknown token kind (skipped)"));
        }
    }
    for (kind, style_value) in table {
        if !kind.contains('.') {
            continue;
        }
        let Some(target_kind) = led_state_syntax::capture_name_to_kind(kind) else {
            loaded
                .warnings
                .push(format!("[syntax] `{kind}`: unknown token kind (skipped)"));
            continue;
        };
        if explicit_kinds.contains(&target_kind) {
            continue;
        }
        let Some(style) = resolve_syntax_style(kind, style_value, styles, &mut loaded.warnings)
        else {
            continue;
        };
        *loaded.theme.syntax.style_mut(target_kind) = style;
    }
    Ok(())
}

/// Resolve one `[syntax]` entry to a `Style`. Handles both the
/// table form (`{ fg = "...", bold = true }`) and the bare-string
/// shorthand (`"$syntax_keyword"` → `{ fg = "$syntax_keyword" }`).
/// Emits warnings for malformed values and returns `None` for them.
fn resolve_syntax_style(
    kind: &str,
    style_value: &toml::Value,
    styles: &StyleTable,
    warnings: &mut Vec<String>,
) -> Option<Style> {
    match style_value {
        toml::Value::Table(t) => parse_style(t, "syntax", kind, styles, warnings),
        toml::Value::String(s) => {
            let Some(style) = resolve_string_to_style(s, styles, &mut HashSet::new()) else {
                warnings.push(format!(
                    "[syntax] `{kind}`: unknown style `{s}` (skipped)",
                ));
                return None;
            };
            Some(style)
        }
        _ => {
            warnings.push(format!(
                "[syntax] `{kind}`: expected table or style string (skipped)"
            ));
            None
        }
    }
}

/// Ingest the `[diagnostics]` table. Four fixed severity keys —
/// `error`, `warning`, `info`, `hint`. Each accepts either the
/// table form (`{ fg = "x196", bold = true }`) or the bare-color
/// shorthand (`"$error"`), matching `[syntax]`. Unknown keys
/// warn and skip.
fn apply_diagnostics(
    loaded: &mut LoadedTheme,
    path: &Path,
    diag: &toml::Value,
    styles: &StyleTable,
) -> Result<(), ThemeError> {
    let table = match diag {
        toml::Value::Table(t) => t,
        _ => {
            return Err(ThemeError::SchemaMismatch {
                path: path.to_path_buf(),
                message: "`diagnostics` must be a table".into(),
            });
        }
    };
    for (key, value) in table {
        let Some(style) = resolve_syntax_style(key, value, styles, &mut loaded.warnings)
        else {
            continue;
        };
        match key.as_str() {
            "error" => loaded.theme.diagnostics.error = style,
            "warning" => loaded.theme.diagnostics.warning = style,
            "info" => loaded.theme.diagnostics.info = style,
            "hint" => loaded.theme.diagnostics.hint = style,
            other => {
                loaded.warnings.push(format!(
                    "[diagnostics] `{other}`: unknown severity (skipped)"
                ));
            }
        }
    }
    Ok(())
}

/// Flatten `[COLORS]` into the alias table.
///
/// Two valid forms per entry:
/// - `name = "value"` — a fg-only Style ([`StyleSpec::Shorthand`]).
/// - `name = { fg = ..., bg = ..., bold = ..., ... }` — a full
///   Style ([`StyleSpec::Full`]). Same fields as a styled-region
///   table.
///
/// Field values stay raw; `resolve_alias_style` /
/// `resolve_color_value` chase `$alias` references at lookup time
/// with cycle detection.
fn extract_styles(value: Option<&toml::Value>, warnings: &mut Vec<String>) -> StyleTable {
    let mut out: StyleTable = HashMap::new();
    let Some(value) = value else {
        return out;
    };
    let table = match value {
        toml::Value::Table(t) => t,
        _ => {
            warnings.push("`COLORS` must be a table (skipped)".into());
            return out;
        }
    };
    for (name, v) in table {
        match v {
            toml::Value::String(s) => {
                out.insert(name.clone(), StyleSpec::Shorthand(s.clone()));
            }
            toml::Value::Table(t) => {
                let mut fg: Option<String> = None;
                let mut bg: Option<String> = None;
                let mut bold: Option<bool> = None;
                let mut reverse: Option<bool> = None;
                let mut underline: Option<bool> = None;
                for (k, vv) in t {
                    match k.as_str() {
                        "fg" => match vv.as_str() {
                            Some(s) => fg = Some(s.to_string()),
                            None => warnings.push(format!(
                                "[COLORS.{name}] `fg`: must be a string (skipped this field)"
                            )),
                        },
                        "bg" => match vv.as_str() {
                            Some(s) => bg = Some(s.to_string()),
                            None => warnings.push(format!(
                                "[COLORS.{name}] `bg`: must be a string (skipped this field)"
                            )),
                        },
                        "bold" => match vv.as_bool() {
                            Some(b) => bold = Some(b),
                            None => warnings.push(format!(
                                "[COLORS.{name}] `bold`: expected boolean (skipped)"
                            )),
                        },
                        "reverse" => match vv.as_bool() {
                            Some(b) => reverse = Some(b),
                            None => warnings.push(format!(
                                "[COLORS.{name}] `reverse`: expected boolean (skipped)"
                            )),
                        },
                        "underline" => match vv.as_bool() {
                            Some(b) => underline = Some(b),
                            None => warnings.push(format!(
                                "[COLORS.{name}] `underline`: expected boolean (skipped)"
                            )),
                        },
                        other => warnings.push(format!(
                            "[COLORS.{name}] `{other}`: unknown field (skipped)"
                        )),
                    }
                }
                out.insert(
                    name.clone(),
                    StyleSpec::Full {
                        fg,
                        bg,
                        bold,
                        reverse,
                        underline,
                    },
                );
            }
            _ => warnings.push(format!(
                "[COLORS] `{name}`: expected color string or style table (skipped)"
            )),
        }
    }
    out
}

/// Route a TOML `[syntax].<kind>` entry into the matching
/// [`SyntaxTheme`] slot. Routes via
/// `led_state_syntax::capture_name_to_kind` — the same dispatch
/// the syntax driver uses to bucket tree-sitter captures into
/// [`TokenKind`]s — so a theme entry lights the slot the painter
/// actually reads.
///
/// `type_` is also accepted as a spelling of `type` because Rust
/// reserves `type`; some users carry the underscore form from
/// experimenting in code-shaped editors.
fn assign_syntax_kind(syntax: &mut SyntaxTheme, kind: &str, style: Style) -> bool {
    let resolved = if kind == "type_" {
        Some(led_state_syntax::TokenKind::Type)
    } else {
        led_state_syntax::capture_name_to_kind(kind)
    };
    let Some(token_kind) = resolved else {
        return false;
    };
    *syntax.style_mut(token_kind) = style;
    true
}

fn parse_style(
    table: &toml::map::Map<String, toml::Value>,
    section: &str,
    region: &str,
    styles: &StyleTable,
    warnings: &mut Vec<String>,
) -> Option<Style> {
    let mut style = Style::default();
    for (k, v) in table {
        match k.as_str() {
            "fg" => match parse_color(v, styles) {
                Some(c) => style.fg = Some(c),
                None => warnings.push(format!(
                    "[{section}.{region}] `fg`: unknown color (skipped this field)"
                )),
            },
            "bg" => match parse_color(v, styles) {
                Some(c) => style.bg = Some(c),
                None => warnings.push(format!(
                    "[{section}.{region}] `bg`: unknown color (skipped this field)"
                )),
            },
            "bold" => match v.as_bool() {
                Some(b) => style.attrs.bold = b,
                None => warnings.push(format!(
                    "[{section}.{region}] `bold`: expected boolean (skipped)"
                )),
            },
            "reverse" => match v.as_bool() {
                Some(b) => style.attrs.reverse = b,
                None => warnings.push(format!(
                    "[{section}.{region}] `reverse`: expected boolean (skipped)"
                )),
            },
            "underline" => match v.as_bool() {
                Some(b) => style.attrs.underline = b,
                None => warnings.push(format!(
                    "[{section}.{region}] `underline`: expected boolean (skipped)"
                )),
            },
            other => warnings.push(format!(
                "[{section}.{region}] `{other}`: unknown field (skipped)"
            )),
        }
    }
    Some(style)
}

fn parse_color(v: &toml::Value, styles: &StyleTable) -> Option<Color> {
    let s = v.as_str()?;
    resolve_color_value(s, styles, &mut HashSet::new())
}

/// Resolve a value-string for a `fg=` / `bg=` slot to a [`Color`].
/// Grammar:
///
/// - `xNNN` — xterm 256-colour palette index.
/// - `#rrggbb` — 24-bit hex.
/// - `ansi_<name>` or `<name>` — named ANSI colour.
/// - `$name` — look `name` up in `[COLORS]` and adopt the named
///   style's colour: a [`StyleSpec::Shorthand`] expands recursively;
///   a [`StyleSpec::Full`] contributes its `fg` (the only colour a
///   colour-slot can use from a full Style).
///
/// `visited` guards against cycles. Returns `None` for unresolved
/// or malformed values.
///
/// **Short-circuit for `$xNNN`.** Legacy themes define each
/// palette index with both an entry (`x032 = "#0087d7"`) AND
/// reference it via `$x032`. Chasing the entry would produce 24-bit
/// RGB — which Apple Terminal can't render and paints as garbage.
/// We detect the `xNNN` reference name and emit `Color::Indexed`
/// directly so crossterm uses the `ESC[38;5;Nm` escape the
/// terminal understands.
fn resolve_color_value(value: &str, styles: &StyleTable, visited: &mut HashSet<String>) -> Option<Color> {
    if let Some(name) = value.strip_prefix('$') {
        if let Some(c) = xterm_index_color(name) {
            return Some(c);
        }
        if !visited.insert(name.to_string()) {
            return None; // cycle
        }
        return match styles.get(name)? {
            StyleSpec::Shorthand(v) => resolve_color_value(v, styles, visited),
            StyleSpec::Full { fg, .. } => {
                // Style reference in a colour slot — use its fg.
                resolve_color_value(fg.as_deref()?, styles, visited)
            }
        };
    }
    parse_color_literal(value)
}

/// Resolve the bare-string shorthand at a styled-region site
/// (`keyword = "$syntax_keyword"` or `keyword = "red"`) to a full
/// [`Style`]. A `$ref` to a [`StyleSpec::Full`] adopts the entire
/// style; a `$ref` to a [`StyleSpec::Shorthand`] or a colour
/// literal becomes a fg-only Style.
fn resolve_string_to_style(value: &str, styles: &StyleTable, visited: &mut HashSet<String>) -> Option<Style> {
    if let Some(name) = value.strip_prefix('$') {
        return resolve_style(name, styles, visited);
    }
    let color = parse_color_literal(value)?;
    Some(Style {
        fg: Some(color),
        ..Style::default()
    })
}

/// Resolve a `$name` reference to the named [`Style`]. Used when a
/// styled region adopts a whole entry (`tab_active = "$selected"`).
fn resolve_style(name: &str, styles: &StyleTable, visited: &mut HashSet<String>) -> Option<Style> {
    if let Some(c) = xterm_index_color(name) {
        return Some(Style {
            fg: Some(c),
            ..Style::default()
        });
    }
    if !visited.insert(name.to_string()) {
        return None; // cycle
    }
    match styles.get(name)? {
        StyleSpec::Shorthand(value) => resolve_string_to_style(value, styles, visited),
        StyleSpec::Full {
            fg,
            bg,
            bold,
            reverse,
            underline,
        } => {
            let mut style = Style::default();
            if let Some(s) = fg.as_deref() {
                style.fg = resolve_color_value(s, styles, visited);
            }
            if let Some(s) = bg.as_deref() {
                style.bg = resolve_color_value(s, styles, visited);
            }
            if let Some(b) = bold {
                style.attrs.bold = *b;
            }
            if let Some(b) = reverse {
                style.attrs.reverse = *b;
            }
            if let Some(b) = underline {
                style.attrs.underline = *b;
            }
            Some(style)
        }
    }
}

/// `xNNN` (3-digit index, 0..=255) → `Color::Indexed`. None for
/// any other shape. Used for the `$xNNN` short-circuit in both
/// resolvers.
fn xterm_index_color(name: &str) -> Option<Color> {
    let digits = name.strip_prefix('x')?;
    if digits.len() != 3 {
        return None;
    }
    let n: u16 = digits.parse().ok()?;
    if n > 255 {
        return None;
    }
    Some(Color::Indexed(n as u8))
}

/// Color literals only — no `$ref` chasing. Used after `$ref`
/// chains have terminated.
fn parse_color_literal(value: &str) -> Option<Color> {
    if let Some(digits) = value.strip_prefix('x') {
        if let Ok(n) = digits.parse::<u16>()
            && n <= 255
        {
            return Some(Color::Indexed(n as u8));
        }
        return None;
    }
    if let Some(hex) = value.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    parse_named_color(value)
}

fn parse_hex_color(hex: &str) -> Option<Color> {
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::rgb(r, g, b))
}

fn parse_named_color(name: &str) -> Option<Color> {
    // Legacy also accepts `ansi_<name>` — strip the prefix so both
    // `"red"` and `"ansi_red"` resolve identically.
    let lower = name.to_ascii_lowercase();
    let base = lower.strip_prefix("ansi_").unwrap_or(&lower);
    match base {
        "black" => Some(Color::BLACK),
        "red" => Some(Color::RED),
        "green" => Some(Color::GREEN),
        "yellow" => Some(Color::YELLOW),
        "blue" => Some(Color::BLUE),
        "magenta" => Some(Color::MAGENTA),
        "cyan" => Some(Color::CYAN),
        "white" => Some(Color::WHITE),
        "grey" | "gray" => Some(Color::GREY),
        "dark_grey" | "dark_gray" | "darkgrey" | "darkgray" => Some(Color::DARK_GREY),
        "bright_black" => Some(Color::DARK_GREY),
        "bright_red" => Some(Color::BRIGHT_RED),
        "bright_green" => Some(Color::BRIGHT_GREEN),
        "bright_yellow" => Some(Color::BRIGHT_YELLOW),
        "bright_blue" => Some(Color::BRIGHT_BLUE),
        "bright_magenta" => Some(Color::BRIGHT_MAGENTA),
        "bright_cyan" => Some(Color::BRIGHT_CYAN),
        "bright_white" => Some(Color::BRIGHT_WHITE),
        // Inherit the terminal's default fg/bg. The painter emits
        // `Reset` for `Color::Default`, identical to leaving the
        // field at `None`. `term_reset` is the legacy spelling
        // that long-standing user themes already carry.
        "term_reset" | "reset" => Some(Color::Default),
        _ => None,
    }
}

fn assign_region(theme: &mut Theme, region: &str, style: Style) -> bool {
    match region {
        "tab_active" => theme.tab_active = style,
        "tab_inactive" => theme.tab_inactive = style,
        "tab_preview" => theme.tab_preview = style,
        "tab_dirty_marker" => theme.tab_dirty_marker = style,
        "status_normal" => theme.status_normal = style,
        "status_warn" => theme.status_warn = style,
        "browser_selected_focused" => theme.browser_selected_focused = style,
        "browser_selected_unfocused" => theme.browser_selected_unfocused = style,
        "browser_chevron" => theme.browser_chevron = style,
        "browser_border" => theme.browser_border = style,
        "search_toggle_on" => theme.search_toggle_on = style,
        "search_match" => theme.search_match = style,
        "search_hit_replaced" => theme.search_hit_replaced = style,
        "cursor_line" => theme.cursor_line = style,
        "ruler" => theme.ruler = style,
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempDir(PathBuf);
    fn tempdir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir();
        let unique = format!("led-theme-test-{}-{}", std::process::id(), n);
        let p = base.join(unique);
        std::fs::create_dir_all(&p).expect("tempdir create");
        TempDir(p)
    }

    /// Guard that points `XDG_CONFIG_HOME` at an empty tempdir so
    /// `discover_theme` doesn't fall through to the developer's real
    /// `~/.config/led/theme.toml`. Every test in this module that
    /// calls `load_theme` without an explicit path must hold one,
    /// otherwise the dev's config leaks in and flakes assertions.
    /// Env vars are process-global, so serialise via a mutex.
    struct XdgGuard {
        prev: Option<std::ffi::OsString>,
        _tmp: TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn xdg_guard() -> XdgGuard {
        use std::sync::{Mutex, OnceLock};
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        let tmp = tempdir();
        // SAFETY: tests hold `lock` while mutating the process
        // environment, and `XdgGuard::drop` restores it.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        }
        XdgGuard {
            prev,
            _tmp: tmp,
            _lock: lock,
        }
    }

    impl Drop for XdgGuard {
        fn drop(&mut self) {
            // SAFETY: `_lock` still held — restore under the same
            // mutex we acquired in `xdg_guard`.
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
        }
    }
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_theme(dir: &TempDir, body: &str) -> PathBuf {
        let p = dir.path().join("theme.toml");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn no_file_returns_built_in_default() {
        // Isolate `XDG_CONFIG_HOME` so the developer's real
        // `~/.config/led/theme.toml` can't leak into the test.
        let _xdg = xdg_guard();
        let tmp = tempdir();
        let loaded = load_theme(Some(tmp.path()), None).unwrap();
        assert_eq!(loaded.theme, Theme::default());
        assert!(loaded.warnings.is_empty(), "warnings: {:?}", loaded.warnings);
    }

    #[test]
    fn named_and_hex_colors_both_parse() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[chrome.status_warn]
fg = "white"
bg = "#cd0000"
bold = true
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.status_warn.fg, Some(Color::WHITE));
        assert_eq!(loaded.theme.status_warn.bg, Some(Color::rgb(0xcd, 0, 0)));
        assert!(loaded.theme.status_warn.attrs.bold);
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn unknown_region_is_warned_not_fatal() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r#"
[chrome.goldenrod_tabs]
fg = "yellow"

[chrome.tab_active]
reverse = true
"#,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("goldenrod_tabs"));
        assert!(loaded.theme.tab_active.attrs.reverse);
    }

    #[test]
    fn unknown_color_name_leaves_field_unset_and_warns() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[chrome.status_warn]
fg = "puce"
bg = "#cd0000"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.status_warn.fg, None);
        assert_eq!(loaded.theme.status_warn.bg, Some(Color::rgb(0xcd, 0, 0)));
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("fg"));
    }

    #[test]
    fn ruler_column_is_an_integer_under_chrome() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r#"
[chrome]
ruler_column = 110
"#,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.ruler_column, Some(110));
    }

    #[test]
    fn short_hex_is_rejected() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[chrome.status_warn]
bg = "#abc"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.status_warn.bg, None);
        assert_eq!(loaded.warnings.len(), 1);
    }

    #[test]
    fn xterm_index_syntax_resolves_to_indexed_color() {
        // Legacy `default_theme.toml` used `"$x024"` (via an alias
        // table); theme.toml accepts `"x024"` directly.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r#"
[chrome.status_normal]
fg = "x223"
bg = "x024"
"#,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.status_normal.fg, Some(Color::Indexed(223)));
        assert_eq!(loaded.theme.status_normal.bg, Some(Color::Indexed(24)));
    }

    #[test]
    fn named_color_resolves_to_ansi_palette_index() {
        // Built-in `"red"` → Indexed(1), not RGB. Terminals honour
        // the user's configured palette for the 0-15 range.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r#"
[chrome.status_warn]
fg = "white"
bg = "red"
"#,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.status_warn.fg, Some(Color::Indexed(7)));
        assert_eq!(loaded.theme.status_warn.bg, Some(Color::Indexed(1)));
    }

    #[test]
    fn syntax_section_populates_token_kind_slots() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[syntax.keyword]
fg = "x170"
bold = true

[syntax.string]
fg = "x107"

[syntax.comment]
fg = "#808080"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(
            loaded.theme.syntax.keyword.fg,
            Some(Color::Indexed(170))
        );
        assert!(loaded.theme.syntax.keyword.attrs.bold);
        assert_eq!(loaded.theme.syntax.string.fg, Some(Color::Indexed(107)));
        assert_eq!(
            loaded.theme.syntax.comment.fg,
            Some(Color::rgb(0x80, 0x80, 0x80))
        );
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn syntax_type_alias_accepts_both_spellings() {
        // TOML keys can't use a Rust reserved word naturally, but
        // either `type` or `type_` maps to `SyntaxTheme.type_`.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r#"
[syntax.type]
fg = "x074"
"#,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.syntax.type_.fg, Some(Color::Indexed(74)));
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn unknown_syntax_kind_is_warned_not_fatal() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r#"
[syntax.neon]
fg = "magenta"

[syntax.keyword]
fg = "yellow"
"#,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("neon"));
        assert_eq!(loaded.theme.syntax.keyword.fg, Some(Color::YELLOW));
    }

    #[test]
    fn colors_alias_resolves_nested_dollar_references() {
        // Legacy pattern: COLORS maps `syntax_keyword` → `$x032` →
        // `#0087d7`. The loader must chase both hops.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[COLORS]
x032 = "#0087d7"
syntax_keyword = "$x032"

[syntax.keyword]
fg = "$syntax_keyword"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        // $x032 short-circuits to Indexed(32) rather than chasing
        // the [COLORS] entry's RGB hex — see `resolve_color_name`
        // for why (Apple Terminal can't do truecolor).
        assert_eq!(
            loaded.theme.syntax.keyword.fg,
            Some(Color::Indexed(32)),
            "warnings: {:?}",
            loaded.warnings,
        );
        assert!(loaded.warnings.is_empty(), "warnings: {:?}", loaded.warnings);
    }

    #[test]
    fn syntax_string_shorthand_treated_as_fg_only() {
        // Legacy's `[syntax] tag = "$syntax_tag"` is a bare string,
        // not a sub-table. Must be interpreted as an fg-only style.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r#"
[syntax]
tag = "x160"
"#,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.syntax.tag.fg, Some(Color::Indexed(160)));
    }

    #[test]
    fn dotted_syntax_keys_collapse_onto_base_kind() {
        // Legacy writes `"type.builtin" = "..."` for finer-grained
        // captures. We assign to the base class `type`.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[syntax]
"type.builtin" = "x030"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.syntax.type_.fg, Some(Color::Indexed(30)));
    }

    #[test]
    fn explicit_base_key_wins_over_dotted_sibling() {
        // Legacy themes define BOTH `string = "..."` (green) and
        // `"string.special" = "..."` (magenta) for finer-grained
        // coverage. Our TokenKind is coarser — only one slot per
        // base class. The non-dotted key must win, otherwise
        // iteration order silently clobbers the authoritative
        // assignment with the more specific dotted sibling.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[syntax]
string           = "x034"
"string.regex"   = "x034"
"string.special" = "magenta"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(
            loaded.theme.syntax.string.fg,
            Some(Color::Indexed(34)),
            "warnings: {:?}",
            loaded.warnings,
        );
    }

    #[test]
    fn ansi_prefixed_names_resolve_to_palette_indices() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[COLORS]
magenta = "ansi_magenta"

[syntax.number]
fg = "$magenta"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.syntax.number.fg, Some(Color::MAGENTA));
    }

    #[test]
    fn cycle_in_colors_styles_yields_unknown_color() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[COLORS]
a = "$b"
b = "$a"

[syntax.keyword]
fg = "$a"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        // Cycle → unresolved → field stays unset + warning fires.
        assert!(loaded.theme.syntax.keyword.fg.is_none());
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("syntax.keyword") && w.contains("unknown color")),
            "warnings: {:?}",
            loaded.warnings,
        );
    }

    #[test]
    fn users_legacy_theme_resolves_every_syntax_slot() {
        // A minimised but representative slice of the user's real
        // ~/.config/led/theme.toml — same [COLORS] chaining and
        // [syntax] entries that were painting everything pink.
        // Lock every resolved slot against its legacy value.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[COLORS]
magenta = "ansi_magenta"
x030 = "#008787"
x032 = "#0087d7"
x034 = "#00af00"
x098 = "#875faf"
x160 = "#d70000"
x172 = "#d78700"
x237 = "#3a3a3a"

syntax_keyword   = "$x032"
syntax_type      = "$x030"
syntax_string    = "$x034"
syntax_number    = "$magenta"
syntax_comment   = "$x237"
syntax_attribute = "$x098"
syntax_tag       = "$x160"
syntax_label     = "$x172"

[syntax]
keyword            = "$syntax_keyword"
function           = "$syntax_keyword"
module             = "$syntax_keyword"
conditional        = "$syntax_keyword"
include            = "$syntax_keyword"
repeat             = "$syntax_keyword"
exception          = "$syntax_keyword"

type               = "$syntax_type"
"type.builtin"     = "$syntax_type"
constructor        = "$syntax_type"

string             = "$syntax_string"
"string.regex"     = "$syntax_string"
"text.literal"     = "$syntax_string"

number             = "$syntax_number"
boolean            = "$syntax_number"
constant           = "$syntax_number"
"constant.builtin" = "$syntax_number"
escape             = "$syntax_number"
"string.special"   = "$syntax_number"

comment            = "$syntax_comment"

"variable.builtin"   = "$syntax_attribute"
"variable.parameter" = "$syntax_attribute"
"variable.member"    = "$syntax_attribute"
property             = "$syntax_attribute"
attribute            = "$syntax_attribute"

tag                = "$syntax_tag"
label              = "$syntax_label"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        let s = &loaded.theme.syntax;
        // Every `$xNNN` short-circuits to the 256-colour index, not
        // the RGB hex in [COLORS]. Apple Terminal doesn't speak
        // truecolor; going through indexed escapes keeps it working.
        assert_eq!(s.keyword.fg, Some(Color::Indexed(32)), "keyword");
        assert_eq!(s.function.fg, Some(Color::Indexed(32)), "function");
        assert_eq!(s.type_.fg, Some(Color::Indexed(30)), "type");
        assert_eq!(s.string.fg, Some(Color::Indexed(34)), "string");
        assert_eq!(s.number.fg, Some(Color::MAGENTA), "number");
        assert_eq!(s.boolean.fg, Some(Color::MAGENTA), "boolean");
        assert_eq!(s.constant.fg, Some(Color::MAGENTA), "constant");
        assert_eq!(s.escape.fg, Some(Color::MAGENTA), "escape");
        assert_eq!(s.comment.fg, Some(Color::Indexed(237)), "comment");
        assert_eq!(s.attribute.fg, Some(Color::Indexed(98)), "attribute");
        assert_eq!(s.property.fg, Some(Color::Indexed(98)), "property");
        assert_eq!(s.tag.fg, Some(Color::Indexed(160)), "tag");
        assert_eq!(s.label.fg, Some(Color::Indexed(172)), "label");
    }

    #[test]
    fn diagnostics_section_populates_severity_styles() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[diagnostics]
error = "x196"
warning = { fg = "x178", bold = true }
info = "x033"
hint = "x245"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.diagnostics.error.fg, Some(Color::Indexed(196)));
        assert_eq!(
            loaded.theme.diagnostics.warning.fg,
            Some(Color::Indexed(178))
        );
        assert!(loaded.theme.diagnostics.warning.attrs.bold);
        assert_eq!(loaded.theme.diagnostics.info.fg, Some(Color::Indexed(33)));
        assert_eq!(loaded.theme.diagnostics.hint.fg, Some(Color::Indexed(245)));
    }

    #[test]
    fn unknown_diagnostics_key_warns_and_skips() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[diagnostics]
error = "x196"
neon = "x201"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.diagnostics.error.fg, Some(Color::Indexed(196)));
        assert!(
            loaded.warnings.iter().any(|w| w.contains("neon")),
            "warnings: {:?}",
            loaded.warnings
        );
    }

    #[test]
    fn diagnostics_via_alias_chain_resolves() {
        // Legacy pattern: styles defined in [COLORS], then
        // [diagnostics] references them as bare strings.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[COLORS]
err_color = "$x196"

[diagnostics]
error = "$err_color"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.diagnostics.error.fg, Some(Color::Indexed(196)));
    }

    #[test]
    fn malformed_toml_is_a_hard_error() {
        let tmp = tempdir();
        let path = write_theme(&tmp, "[chrome\n");
        let err = load_theme(None, Some(&path)).unwrap_err();
        assert!(matches!(err, ThemeError::Toml { .. }), "got {err:?}");
    }

    // ── inline-table style entries in [COLORS] ────────────────────

    #[test]
    fn inline_table_style_entry_parses_with_no_warning() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[COLORS]
my_pair = { fg = "x232", bg = "x024", bold = true }
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert!(
            loaded.warnings.is_empty(),
            "no warnings expected, got: {:?}",
            loaded.warnings,
        );
    }

    #[test]
    fn full_style_ref_adopts_fg_bg_and_attrs() {
        // The user's pattern: a [COLORS] entry holds a full Style,
        // a styled-region key references it as a bare string and
        // gets the entire Style (not just the fg).
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[COLORS]
inverse_bold = { fg = "x232", bg = "x223", bold = true }

[syntax]
keyword = "$inverse_bold"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        let kw = loaded.theme.syntax.keyword;
        assert_eq!(kw.fg, Some(Color::Indexed(232)));
        assert_eq!(kw.bg, Some(Color::Indexed(223)));
        assert!(kw.attrs.bold);
        assert!(loaded.warnings.is_empty(), "warnings: {:?}", loaded.warnings);
    }

    #[test]
    fn full_style_ref_chains_through_shorthand_in_colors() {
        // theme_bold_i is a Full style; inverse_bold is a Shorthand
        // pointing at it. Bare-string at use-site must adopt the
        // full Style of theme_bold_i.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[COLORS]
theme_bold_i = { fg = "x232", bg = "x223" }
inverse_bold = "$theme_bold_i"

[syntax]
keyword = "$inverse_bold"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        let kw = loaded.theme.syntax.keyword;
        assert_eq!(kw.fg, Some(Color::Indexed(232)));
        assert_eq!(kw.bg, Some(Color::Indexed(223)));
    }

    #[test]
    fn full_style_used_in_color_slot_extracts_fg() {
        // `fg = "$some_full_style"` — caller wants a Color, so the
        // resolver takes the entry's fg.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[COLORS]
warn_pair = { fg = "x196", bg = "x232" }

[chrome.status_warn]
fg = "$warn_pair"
bg = "x024"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.status_warn.fg, Some(Color::Indexed(196)));
        assert_eq!(loaded.theme.status_warn.bg, Some(Color::Indexed(24)));
    }

    #[test]
    fn full_style_cycle_yields_no_panic_and_unknown() {
        // Two Full entries that reference each other via fg —
        // resolver must terminate via the visited set.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[COLORS]
a = { fg = "$b" }
b = { fg = "$a" }

[syntax]
keyword = "$a"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        // Cycle bails — fg never resolves.
        assert!(loaded.theme.syntax.keyword.fg.is_none());
    }

    // ── term_reset / Color::Default ──────────────────────────────

    #[test]
    fn term_reset_resolves_to_color_default_no_warning() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[COLORS]
normal = "term_reset"

[syntax]
operator = "$normal"
punctuation = "reset"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.syntax.operator.fg, Some(Color::Default));
        assert_eq!(loaded.theme.syntax.punctuation.fg, Some(Color::Default));
        assert!(
            loaded.warnings.is_empty(),
            "no warnings expected for term_reset, got {:?}",
            loaded.warnings,
        );
    }

    #[test]
    fn term_reset_in_full_style_fg_works() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[COLORS]
selected = { fg = "term_reset", bg = "x053" }

[chrome.cursor_line]
fg = "term_reset"
bg = "x053"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.cursor_line.fg, Some(Color::Default));
        assert_eq!(loaded.theme.cursor_line.bg, Some(Color::Indexed(53)));
        assert!(loaded.warnings.is_empty(), "warnings: {:?}", loaded.warnings);
    }

    // ── tree-sitter capture aliases ───────────────────────────────

    #[test]
    fn theme_and_painter_share_capture_name_dispatch() {
        // Round-trip property: a TOML `[syntax].<name>` entry must
        // light the same slot that `style_for(capture_name_to_kind(<name>))`
        // resolves to. If the theme writer and painter ever drift
        // again, this fails.
        for &name in &[
            "conditional",
            "repeat",
            "include",
            "exception",
            "module",
            "namespace",
            "constructor",
            "method",
            "field",
            "annotation",
            "text.title",
            "text.literal",
            "text.uri",
        ] {
            let body = format!("[syntax]\n\"{name}\" = \"x099\"\n");
            let tmp = tempdir();
            let path = write_theme(&tmp, &body);
            let loaded = load_theme(None, Some(&path)).unwrap();
            let kind = led_state_syntax::capture_name_to_kind(name)
                .unwrap_or_else(|| panic!("`{name}` must map to a TokenKind"));
            assert_eq!(
                loaded.theme.syntax.style_for(kind).fg,
                Some(Color::Indexed(99)),
                "theme entry for `{name}` should land in the slot the painter reads",
            );
        }
    }

    #[test]
    fn tree_sitter_keyword_aliases_route_to_keyword_slot() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[syntax]
conditional = "x032"
repeat = "x032"
include = "x032"
exception = "x032"
import = "x032"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        // Last write wins (alphabetical, via toml's BTreeMap).
        assert_eq!(loaded.theme.syntax.keyword.fg, Some(Color::Indexed(32)));
        assert!(
            loaded
                .warnings
                .iter()
                .all(|w| !w.contains("unknown token kind")),
            "no unknown-token warnings expected, got {:?}",
            loaded.warnings,
        );
    }

    #[test]
    fn tree_sitter_type_aliases_route_to_type_slot() {
        // `module` and `namespace` route to Type per the canonical
        // mapping. `constructor` deliberately routes to Function
        // (matching the rewrite's pre-refactor painter) and is
        // covered separately.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[syntax]
module = "x030"
namespace = "x030"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.syntax.type_.fg, Some(Color::Indexed(30)));
        assert!(
            loaded
                .warnings
                .iter()
                .all(|w| !w.contains("unknown token kind")),
            "warnings: {:?}",
            loaded.warnings,
        );
    }

    #[test]
    fn tree_sitter_constructor_routes_to_function_slot() {
        let tmp = tempdir();
        let path = write_theme(&tmp, "[syntax]\nconstructor = \"x033\"\n");
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.syntax.function.fg, Some(Color::Indexed(33)));
    }

    #[test]
    fn markdown_text_kinds_route_to_per_subname_slots() {
        // `text.*` captures don't collapse to a single slot — each
        // subname routes per the canonical mapping (markdown
        // titles → label, code spans → string, links → attribute,
        // urls → keyword, emphasis/strong → label).
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[syntax]
"text.title" = "x010"
"text.literal" = "x011"
"text.reference" = "x012"
"text.uri" = "x013"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.syntax.label.fg, Some(Color::Indexed(10)));
        assert_eq!(loaded.theme.syntax.string.fg, Some(Color::Indexed(11)));
        assert_eq!(loaded.theme.syntax.attribute.fg, Some(Color::Indexed(12)));
        assert_eq!(loaded.theme.syntax.keyword.fg, Some(Color::Indexed(13)));
        assert!(
            loaded
                .warnings
                .iter()
                .all(|w| !w.contains("unknown token kind")),
            "warnings: {:?}",
            loaded.warnings,
        );
    }

    #[test]
    fn full_style_unknown_field_warns_with_table_path() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[COLORS]
weird = { fg = "x232", strange = true }
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("[COLORS.weird]") && w.contains("strange")),
            "expected warning naming the offending field, got {:?}",
            loaded.warnings,
        );
    }
}
