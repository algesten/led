use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use ratatui::style::Color;

use led_core::Component;

pub use led_core::{BLANK_STYLE, Theme, ElementStyle};

pub fn build_default_theme_toml(components: &[Box<dyn Component>]) -> String {
    let mut toml = String::from(SHELL_THEME_TOML);
    let mut seen = std::collections::HashSet::new();
    for comp in components {
        let fragment = comp.default_theme_toml();
        if !fragment.is_empty() && seen.insert(fragment as *const str) {
            toml.push_str(fragment);
        }
    }
    toml
}

pub fn default_theme(components: &[Box<dyn Component>]) -> Theme {
    let toml_str = build_default_theme_toml(components);
    let doc: toml::Value = toml_str.parse()
        .expect("built-in theme must parse");
    theme_from_toml(&doc)
}

// ---------------------------------------------------------------------------
// Color parsing
// ---------------------------------------------------------------------------

fn parse_ansi(s: &str) -> Option<Color> {
    match s {
        "" | "term_reset" => Some(Color::Reset),
        "ansi_black" => Some(Color::Black),
        "ansi_red" => Some(Color::Red),
        "ansi_green" => Some(Color::Green),
        "ansi_yellow" => Some(Color::Yellow),
        "ansi_blue" => Some(Color::Blue),
        "ansi_magenta" => Some(Color::Magenta),
        "ansi_cyan" => Some(Color::Cyan),
        "ansi_white" => Some(Color::White),
        "ansi_bright_black" | "ansi_gray" | "ansi_grey" => Some(Color::DarkGray),
        "ansi_bright_red" => Some(Color::LightRed),
        "ansi_bright_green" => Some(Color::LightGreen),
        "ansi_bright_yellow" => Some(Color::LightYellow),
        "ansi_bright_blue" => Some(Color::LightBlue),
        "ansi_bright_magenta" => Some(Color::LightMagenta),
        "ansi_bright_cyan" => Some(Color::LightCyan),
        "ansi_bright_white" => Some(Color::Gray),
        _ if s.starts_with('#') && s.len() == 4 => {
            let r = u8::from_str_radix(&s[1..2], 16).ok()? * 17;
            let g = u8::from_str_radix(&s[2..3], 16).ok()? * 17;
            let b = u8::from_str_radix(&s[3..4], 16).ok()? * 17;
            Some(Color::Rgb(r, g, b))
        }
        _ if s.starts_with('#') && s.len() == 7 => {
            let r = u8::from_str_radix(&s[1..3], 16).ok()?;
            let g = u8::from_str_radix(&s[3..5], 16).ok()?;
            let b = u8::from_str_radix(&s[5..7], 16).ok()?;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
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

fn resolve(name: &str, colors: &HashMap<String, ColorEntry>) -> Option<Color> {
    if let Some(entry) = colors.get(name) {
        match entry {
            ColorEntry::Value(v) => return parse_ansi(v),
            ColorEntry::Style(so) => return so.fg.as_ref().and_then(|f| resolve_color(f, colors)),
            ColorEntry::Alias(target) => {
                if let Some(target_entry) = colors.get(target.as_str()) {
                    match target_entry {
                        ColorEntry::Value(v) => return parse_ansi(v),
                        ColorEntry::Style(so) => return so.fg.as_ref().and_then(|f| resolve_color(f, colors)),
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

fn resolve_color(value: &str, colors: &HashMap<String, ColorEntry>) -> Option<Color> {
    if let Some(name) = value.strip_prefix('$') {
        resolve(name, colors)
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
        if let Some(c) = resolve_color(fg, colors) {
            style.fg = c;
        }
    }
    if let Some(ref bg) = so.bg {
        if let Some(c) = resolve_color(bg, colors) {
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
        ColorEntry::Alias(target) => {
            match colors.get(target.as_str())? {
                ColorEntry::Style(so) => Some(so),
                _ => None,
            }
        }
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
            } else if t.contains_key("fg") || t.contains_key("bg")
                || t.contains_key("bold") || t.contains_key("reversed")
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

fn parse_colors_section(table: &toml::map::Map<String, toml::Value>) -> HashMap<String, ColorEntry> {
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
        if let Some(c) = resolve_color(fg, colors) {
            style.fg = c;
        }
    }
    if let Some(bg) = section.get("bg").and_then(|v| v.as_str()) {
        if let Some(c) = resolve_color(bg, colors) {
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
            if let Some(c) = resolve_color(s, colors) {
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

const SHELL_THEME_TOML: &str = r##"# led theme
# Colors: ansi_black, ansi_red, ansi_green, ansi_yellow, ansi_blue,
#   ansi_magenta, ansi_cyan, ansi_white, ansi_bright_black (ansi_gray),
#   ansi_bright_red, ansi_bright_green, ansi_bright_yellow,
#   ansi_bright_blue, ansi_bright_magenta, ansi_bright_cyan,
#   ansi_bright_white, or hex "#rgb" / "#rrggbb"
# term_reset = inherit the terminal's default foreground/background color.

####################################################################

# Define named colors under [COLORS] and reference them with $name.

[COLORS]
# Base ANSI colors
black   = "ansi_black"
red     = "ansi_red"
green   = "ansi_green"
yellow  = "ansi_yellow"
blue    = "ansi_blue"
magenta = "ansi_magenta"
cyan    = "ansi_cyan"
white   = "ansi_white"

bright_black   = "ansi_bright_black"
bright_red     = "ansi_bright_red"
bright_green   = "ansi_bright_green"
bright_yellow  = "ansi_bright_yellow"
bright_blue    = "ansi_bright_blue"
bright_magenta = "ansi_bright_magenta"
bright_cyan    = "ansi_bright_cyan"
bright_white   = "ansi_bright_white"

# Semantic aliases
normal           = "term_reset"
accent           = "$bright_magenta"
muted            = "$magenta"
inverse_active   = { fg = "$bright_yellow", bg = "$magenta", bold = true }
inverse_inactive = { fg = "term_reset", bg = "$bright_black", bold = true }
selected         = { fg = "term_reset", bg = "$bright_black" }


####################################################################

[tabs]
active           = "$inverse_active"
inactive         = "$inverse_inactive"
preview_active   = "$inverse_active"
preview_inactive = "$inverse_inactive"

[status_bar]
style = "$inverse_active"
"##;

pub fn load_theme(components: &[Box<dyn Component>]) -> Theme {
    let path = match theme_path() {
        Some(p) => p,
        None => return default_theme(components),
    };

    if !path.exists() {
        let full_toml = build_default_theme_toml(components);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, &full_toml);
        return default_theme(components);
    }

    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return default_theme(components),
    };

    let mut doc: toml::Value = match toml::from_str(&content) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("warning: failed to parse theme.toml: {e}; using defaults");
            return default_theme(components);
        }
    };

    // Backfill missing sections/keys from defaults
    let defaults_toml = build_default_theme_toml(components);
    if let Ok(defaults) = toml::from_str::<toml::Value>(&defaults_toml) {
        if backfill_missing(&mut doc, &defaults) {
            // Re-serialize and write back
            if let Ok(updated) = toml::to_string_pretty(&doc) {
                let _ = fs::write(&path, &updated);
            }
        }
    }

    theme_from_toml(&doc)
}

/// Merge missing sections and keys from `defaults` into `doc`.
/// Returns true if anything was added.
fn backfill_missing(doc: &mut toml::Value, defaults: &toml::Value) -> bool {
    let (Some(doc_table), Some(def_table)) = (doc.as_table_mut(), defaults.as_table()) else {
        return false;
    };
    let mut changed = false;
    for (section, def_value) in def_table {
        match doc_table.get_mut(section) {
            None => {
                doc_table.insert(section.clone(), def_value.clone());
                changed = true;
            }
            Some(existing) => {
                if let (Some(existing_tbl), Some(def_tbl)) =
                    (existing.as_table_mut(), def_value.as_table())
                {
                    for (key, val) in def_tbl {
                        if !existing_tbl.contains_key(key) {
                            existing_tbl.insert(key.clone(), val.clone());
                            changed = true;
                        }
                    }
                }
            }
        }
    }
    changed
}

pub fn reset_theme(components: &[Box<dyn Component>]) {
    let Some(path) = theme_path() else { return };
    let full_toml = build_default_theme_toml(components);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, &full_toml);
}
