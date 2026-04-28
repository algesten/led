//! Pure parsing of style/color values from `theme.toml`.
//!
//! Converts TOML values to `Style` / `Color`. Does not mutate the
//! live [`Theme`] struct — callers in `apply.rs` route the parsed
//! [`Style`]s into chrome / syntax / diagnostics slots.

use std::collections::{HashMap, HashSet};

use led_driver_terminal_core::{Color, Style};

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
pub(super) enum StyleSpec {
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
pub(super) type StyleTable = HashMap<String, StyleSpec>;

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
pub(super) fn extract_styles(value: Option<&toml::Value>, warnings: &mut Vec<String>) -> StyleTable {
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

pub(super) fn parse_style(
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
pub(super) fn resolve_string_to_style(value: &str, styles: &StyleTable, visited: &mut HashSet<String>) -> Option<Style> {
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
