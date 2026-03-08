use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use ratatui::style::Color;

use led_core::color::parse_ansi;

pub use led_core::{BLANK_STYLE, ElementStyle, Theme};

const DEFAULT_THEME_TOML: &str = include_str!("default_theme.toml");

pub fn default_theme() -> Theme {
    let doc: toml::Value = DEFAULT_THEME_TOML
        .parse()
        .expect("built-in theme must parse");
    theme_from_toml(&doc)
}

// ---------------------------------------------------------------------------
// Color entry & resolution
// ---------------------------------------------------------------------------

struct StyleOverride {
    fg: Option<String>,
    bg: Option<String>,
    bold: Option<bool>,
    reversed: Option<bool>,
}

enum ColorEntry {
    Value(String),
    Alias(String),
    Style(StyleOverride),
}

const MAX_RESOLVE_DEPTH: usize = 8;

fn resolve(name: &str, colors: &HashMap<String, ColorEntry>, depth: usize) -> Option<Color> {
    if depth == 0 {
        return None;
    }
    if let Some(entry) = colors.get(name) {
        match entry {
            ColorEntry::Value(v) => return parse_ansi(v),
            ColorEntry::Style(so) => {
                return so
                    .fg
                    .as_ref()
                    .and_then(|f| resolve_color(f, colors, depth - 1));
            }
            ColorEntry::Alias(target) => {
                if let Some(target_entry) = colors.get(target.as_str()) {
                    match target_entry {
                        ColorEntry::Value(v) => return parse_ansi(v),
                        ColorEntry::Style(so) => {
                            return so
                                .fg
                                .as_ref()
                                .and_then(|f| resolve_color(f, colors, depth - 1));
                        }
                        ColorEntry::Alias(t2) => {
                            if let Some(ColorEntry::Value(v)) = colors.get(t2.as_str()) {
                                return parse_ansi(v);
                            }
                        }
                    }
                }
                return parse_ansi(target);
            }
        }
    }
    parse_ansi(name)
}

fn resolve_color(value: &str, colors: &HashMap<String, ColorEntry>, depth: usize) -> Option<Color> {
    if depth == 0 {
        return None;
    }
    if let Some(name) = value.strip_prefix('$') {
        resolve(name, colors, depth - 1)
    } else {
        parse_ansi(value)
    }
}

fn apply_style_override(
    so: &StyleOverride,
    colors: &HashMap<String, ColorEntry>,
    style: &mut ElementStyle,
) {
    if let Some(ref fg) = so.fg {
        if let Some(c) = resolve_color(fg, colors, MAX_RESOLVE_DEPTH) {
            style.fg = c;
        }
    }
    if let Some(ref bg) = so.bg {
        if let Some(c) = resolve_color(bg, colors, MAX_RESOLVE_DEPTH) {
            style.bg = c;
        }
    }
    if let Some(b) = so.bold {
        style.bold = b;
    }
    if let Some(r) = so.reversed {
        style.reversed = r;
    }
}

fn resolve_style_override<'a>(
    name: &str,
    colors: &'a HashMap<String, ColorEntry>,
) -> Option<&'a StyleOverride> {
    match colors.get(name)? {
        ColorEntry::Style(so) => Some(so),
        ColorEntry::Alias(target) => match colors.get(target.as_str())? {
            ColorEntry::Style(so) => Some(so),
            _ => None,
        },
        ColorEntry::Value(_) => None,
    }
}

// ---------------------------------------------------------------------------
// TOML parsing
// ---------------------------------------------------------------------------

fn parse_color_entry(value: &toml::Value) -> Option<ColorEntry> {
    match value {
        toml::Value::String(s) => {
            if let Some(alias) = s.strip_prefix('$') {
                Some(ColorEntry::Alias(alias.to_string()))
            } else {
                Some(ColorEntry::Value(s.clone()))
            }
        }
        toml::Value::Table(t) => {
            if let Some(v) = t.get("value").and_then(|v| v.as_str()) {
                Some(ColorEntry::Value(v.to_string()))
            } else if let Some(a) = t.get("alias").and_then(|v| v.as_str()) {
                Some(ColorEntry::Alias(a.to_string()))
            } else if t.contains_key("fg")
                || t.contains_key("bg")
                || t.contains_key("bold")
                || t.contains_key("reversed")
            {
                Some(ColorEntry::Style(StyleOverride {
                    fg: t.get("fg").and_then(|v| v.as_str()).map(String::from),
                    bg: t.get("bg").and_then(|v| v.as_str()).map(String::from),
                    bold: t.get("bold").and_then(|v| v.as_bool()),
                    reversed: t.get("reversed").and_then(|v| v.as_bool()),
                }))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn parse_colors_section(
    table: &toml::map::Map<String, toml::Value>,
) -> HashMap<String, ColorEntry> {
    let mut colors = HashMap::new();
    for (name, value) in table {
        if let Some(entry) = parse_color_entry(value) {
            colors.insert(name.clone(), entry);
        }
    }
    colors
}

fn apply_element_style(
    section: &toml::map::Map<String, toml::Value>,
    colors: &HashMap<String, ColorEntry>,
    style: &mut ElementStyle,
) {
    for field in ["fg", "bg"] {
        if let Some(s) = section.get(field).and_then(|v| v.as_str()) {
            if let Some(name) = s.strip_prefix('$') {
                if let Some(so) = resolve_style_override(name, colors) {
                    apply_style_override(so, colors, style);
                }
            }
        }
    }

    if let Some(fg) = section.get("fg").and_then(|v| v.as_str()) {
        if let Some(c) = resolve_color(fg, colors, MAX_RESOLVE_DEPTH) {
            style.fg = c;
        }
    }
    if let Some(bg) = section.get("bg").and_then(|v| v.as_str()) {
        if let Some(c) = resolve_color(bg, colors, MAX_RESOLVE_DEPTH) {
            style.bg = c;
        }
    }
    if let Some(b) = section.get("bold").and_then(|v| v.as_bool()) {
        style.bold = b;
    }
    if let Some(r) = section.get("reversed").and_then(|v| v.as_bool()) {
        style.reversed = r;
    }
}

fn apply_inline_element(
    section: &toml::map::Map<String, toml::Value>,
    key: &str,
    colors: &HashMap<String, ColorEntry>,
    style: &mut ElementStyle,
) {
    match section.get(key) {
        Some(toml::Value::Table(sub)) => apply_element_style(sub, colors, style),
        Some(toml::Value::String(s)) => {
            if let Some(name) = s.strip_prefix('$') {
                if let Some(so) = resolve_style_override(name, colors) {
                    apply_style_override(so, colors, style);
                    return;
                }
            }
            if let Some(c) = resolve_color(s, colors, MAX_RESOLVE_DEPTH) {
                style.fg = c;
            }
        }
        _ => {}
    }
}

fn resolve_inline_element(
    section: &toml::map::Map<String, toml::Value>,
    key: &str,
    colors: &HashMap<String, ColorEntry>,
) -> ElementStyle {
    let mut style = BLANK_STYLE;
    apply_inline_element(section, key, colors, &mut style);
    style
}

fn theme_from_toml(doc: &toml::Value) -> Theme {
    let mut theme = Theme::new();
    let table = match doc.as_table() {
        Some(t) => t,
        None => return theme,
    };

    let colors = table
        .get("COLORS")
        .and_then(|v| v.as_table())
        .map(|t| parse_colors_section(t))
        .unwrap_or_default();

    for (section_name, section_value) in table {
        if section_name == "COLORS" {
            continue;
        }
        let Some(section) = section_value.as_table() else {
            continue;
        };
        for key in section.keys() {
            let dotted = format!("{section_name}.{key}");
            let style = resolve_inline_element(section, key, &colors);
            theme.set(dotted, style);
        }
    }

    theme
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn theme_path() -> Option<PathBuf> {
    dirs::home_dir().map(|d| d.join(".config").join("led").join("theme.toml"))
}

pub fn load_theme() -> Theme {
    let path = match theme_path() {
        Some(p) => p,
        None => return default_theme(),
    };

    if !path.exists() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, DEFAULT_THEME_TOML);
        return default_theme();
    }

    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return default_theme(),
    };

    let doc: toml::Value = match toml::from_str(&content) {
        Ok(d) => d,
        Err(e) => {
            log::warn!("failed to parse theme.toml: {e}; using defaults");
            return default_theme();
        }
    };

    theme_from_toml(&doc)
}

pub fn reset_theme() {
    let Some(path) = theme_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, DEFAULT_THEME_TOML);
}
