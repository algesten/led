use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use ratatui::style::Color;

pub use led_core::{Theme, ElementStyle};

pub fn default_theme() -> Theme {
    let doc: toml::Value = toml::from_str(DEFAULT_THEME_TOML)
        .expect("built-in theme.toml must parse");
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

fn theme_from_toml(doc: &toml::Value) -> Theme {
    let mut theme = Theme::blank();
    let table = match doc.as_table() {
        Some(t) => t,
        None => return theme,
    };

    let colors = table
        .get("COLORS")
        .and_then(|v| v.as_table())
        .map(|t| parse_colors_section(t))
        .unwrap_or_default();

    if let Some(section) = table.get("editor").and_then(|v| v.as_table()) {
        apply_inline_element(section, "text", &colors, &mut theme.editor_text);
        apply_inline_element(section, "gutter", &colors, &mut theme.gutter);
    }

    if let Some(section) = table.get("tabs").and_then(|v| v.as_table()) {
        apply_inline_element(section, "active", &colors, &mut theme.tab_active);
        apply_inline_element(section, "inactive", &colors, &mut theme.tab_inactive);
    }

    if let Some(section) = table.get("status_bar").and_then(|v| v.as_table()) {
        apply_inline_element(section, "style", &colors, &mut theme.status_bar);
    }

    if let Some(section) = table.get("browser").and_then(|v| v.as_table()) {
        apply_inline_element(section, "directory", &colors, &mut theme.browser_dir);
        apply_inline_element(section, "file", &colors, &mut theme.browser_file);
        apply_inline_element(section, "selected", &colors, &mut theme.browser_selected);
        apply_inline_element(section, "selected_unfocused", &colors, &mut theme.browser_selected_unfocused);
        apply_inline_element(section, "border", &colors, &mut theme.browser_border);
    }

    theme
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn theme_path() -> Option<PathBuf> {
    dirs::home_dir().map(|d| d.join(".config").join("led").join("theme.toml"))
}

pub const DEFAULT_THEME_TOML: &str = r##"# led theme
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


####################################################################

[editor]
text   = "$normal"
gutter = "$muted"

[tabs]
active   = "$inverse_active"
inactive = "$inverse_inactive"

[status_bar]
style = "$inverse_active"

[browser]
directory          = "$accent"
file               = "$normal"
selected           = "$inverse_active"
selected_unfocused = "$inverse_inactive"
border             = "$muted"
"##;

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

    match toml::from_str::<toml::Value>(&content) {
        Ok(doc) => theme_from_toml(&doc),
        Err(e) => {
            eprintln!("warning: failed to parse theme.toml: {e}; using defaults");
            default_theme()
        }
    }
}

pub fn reset_theme() {
    let Some(path) = theme_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, DEFAULT_THEME_TOML);
}
