use std::collections::HashMap;

use led_core::ElementStyle;
use led_core::color::{make_color, parse_ansi};
use ratatui::style::Color;

// ---------------------------------------------------------------------------
// Hex color scanning (any file)
// ---------------------------------------------------------------------------

pub(crate) fn scan_hex_color(line: &str) -> Option<Color> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            // Try #rrggbb first
            if i + 7 <= bytes.len() && bytes[i + 1..i + 7].iter().all(|b| b.is_ascii_hexdigit()) {
                // Make sure it's exactly 6 hex digits (not part of a longer hex string)
                if i + 7 >= bytes.len() || !bytes[i + 7].is_ascii_hexdigit() {
                    let r = u8::from_str_radix(&line[i + 1..i + 3], 16).ok()?;
                    let g = u8::from_str_radix(&line[i + 3..i + 5], 16).ok()?;
                    let b = u8::from_str_radix(&line[i + 5..i + 7], 16).ok()?;
                    return Some(make_color(r, g, b));
                }
            }
            // Try #rgb
            if i + 4 <= bytes.len() && bytes[i + 1..i + 4].iter().all(|b| b.is_ascii_hexdigit()) {
                if i + 4 >= bytes.len() || !bytes[i + 4].is_ascii_hexdigit() {
                    let r = u8::from_str_radix(&line[i + 1..i + 2], 16).ok()? * 17;
                    let g = u8::from_str_radix(&line[i + 2..i + 3], 16).ok()? * 17;
                    let b = u8::from_str_radix(&line[i + 3..i + 4], 16).ok()? * 17;
                    return Some(make_color(r, g, b));
                }
            }
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Theme.toml evaluation (text-based, no toml crate)
// ---------------------------------------------------------------------------

pub(crate) enum ColorDef {
    Value(String),
    Alias(String),
    Style {
        fg: Option<String>,
        bg: Option<String>,
        bold: bool,
        reversed: bool,
    },
}

pub(crate) struct ColorDefs {
    map: HashMap<String, ColorDef>,
}

const MAX_DEPTH: usize = 8;

impl ColorDefs {
    fn resolve_value(&self, value: &str) -> Option<Color> {
        self.resolve_value_depth(value, MAX_DEPTH)
    }

    fn resolve_value_depth(&self, value: &str, depth: usize) -> Option<Color> {
        if depth == 0 {
            return None;
        }
        if let Some(name) = value.strip_prefix('$') {
            self.resolve_name_depth(name, depth - 1)
        } else {
            parse_ansi(value)
        }
    }

    fn resolve_name_depth(&self, name: &str, depth: usize) -> Option<Color> {
        if depth == 0 {
            return None;
        }
        match self.map.get(name)? {
            ColorDef::Value(v) => parse_ansi(v),
            ColorDef::Alias(target) => self.resolve_value_depth(&format!("${target}"), depth - 1),
            ColorDef::Style { fg, .. } => fg
                .as_ref()
                .and_then(|f| self.resolve_value_depth(f, depth - 1)),
        }
    }

    fn resolve_style(&self, value: &str) -> Option<ElementStyle> {
        self.resolve_style_depth(value, MAX_DEPTH)
    }

    fn resolve_style_depth(&self, value: &str, depth: usize) -> Option<ElementStyle> {
        if depth == 0 {
            return None;
        }
        if let Some(name) = value.strip_prefix('$') {
            match self.map.get(name)? {
                ColorDef::Value(v) => {
                    let c = parse_ansi(v)?;
                    Some(ElementStyle {
                        fg: c,
                        bg: Color::Reset,
                        bold: false,
                        reversed: false,
                    })
                }
                ColorDef::Alias(target) => {
                    self.resolve_style_depth(&format!("${target}"), depth - 1)
                }
                ColorDef::Style {
                    fg,
                    bg,
                    bold,
                    reversed,
                } => {
                    let fg_c = fg
                        .as_ref()
                        .and_then(|f| self.resolve_value_depth(f, depth - 1))
                        .unwrap_or(Color::Reset);
                    let bg_c = bg
                        .as_ref()
                        .and_then(|b| self.resolve_value_depth(b, depth - 1))
                        .unwrap_or(Color::Reset);
                    Some(ElementStyle {
                        fg: fg_c,
                        bg: bg_c,
                        bold: *bold,
                        reversed: *reversed,
                    })
                }
            }
        } else {
            let c = parse_ansi(value)?;
            Some(ElementStyle {
                fg: c,
                bg: Color::Reset,
                bold: false,
                reversed: false,
            })
        }
    }
}

/// Parse the [COLORS] section from raw theme.toml lines.
pub(crate) fn parse_color_defs<'a>(lines: impl Iterator<Item = &'a str>) -> ColorDefs {
    let mut map = HashMap::new();
    let mut in_colors = false;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_colors = trimmed.eq_ignore_ascii_case("[colors]");
            continue;
        }
        if !in_colors {
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = parse_key_value(trimmed) {
            map.insert(key, value);
        }
    }

    ColorDefs { map }
}

fn parse_key_value(line: &str) -> Option<(String, ColorDef)> {
    let eq = line.find('=')?;
    let key = line[..eq].trim().to_string();
    let raw_value = line[eq + 1..].trim();

    if raw_value.starts_with('{') {
        // Inline table: { fg = "...", bg = "...", bold = true }
        let inner = raw_value.trim_start_matches('{').trim_end_matches('}');
        let mut fg = None;
        let mut bg = None;
        let mut bold = false;
        let mut reversed = false;
        for part in inner.split(',') {
            let part = part.trim();
            if let Some(eq_pos) = part.find('=') {
                let field = part[..eq_pos].trim();
                let val = part[eq_pos + 1..].trim().trim_matches('"');
                match field {
                    "fg" => fg = Some(val.to_string()),
                    "bg" => bg = Some(val.to_string()),
                    "bold" => bold = val == "true",
                    "reversed" => reversed = val == "true",
                    _ => {}
                }
            }
        }
        Some((
            key,
            ColorDef::Style {
                fg,
                bg,
                bold,
                reversed,
            },
        ))
    } else {
        // Simple string value: "value" or "$alias"
        let val = raw_value.trim_matches('"');
        if let Some(alias) = val.strip_prefix('$') {
            Some((key, ColorDef::Alias(alias.to_string())))
        } else {
            Some((key, ColorDef::Value(val.to_string())))
        }
    }
}

/// Evaluate a single theme.toml line to determine its display style.
/// `section` is the current TOML section header (e.g., "COLORS", "editor").
pub(crate) fn evaluate_theme_line(
    line: &str,
    section: &str,
    defs: &ColorDefs,
) -> Option<ElementStyle> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
        return None;
    }
    let eq = trimmed.find('=')?;
    let raw_value = trimmed[eq + 1..].trim();

    if section.eq_ignore_ascii_case("COLORS") {
        // In [COLORS] section, resolve the defined value itself
        if raw_value.starts_with('{') {
            // Inline table style definition
            let key = trimmed[..eq].trim();
            defs.resolve_style(&format!("${key}"))
        } else {
            let val = raw_value.trim_matches('"');
            defs.resolve_style(val)
        }
    } else {
        // In other sections, values reference colors
        if raw_value.starts_with('{') {
            // Inline table: parse fg/bg/bold/reversed directly
            let inner = raw_value.trim_start_matches('{').trim_end_matches('}');
            let mut fg = None;
            let mut bg = None;
            let mut bold = false;
            let mut reversed = false;
            for part in inner.split(',') {
                let part = part.trim();
                if let Some(eq_pos) = part.find('=') {
                    let field = part[..eq_pos].trim();
                    let fval = part[eq_pos + 1..].trim().trim_matches('"');
                    match field {
                        "fg" => fg = Some(fval.to_string()),
                        "bg" => bg = Some(fval.to_string()),
                        "bold" => bold = fval == "true",
                        "reversed" => reversed = fval == "true",
                        _ => {}
                    }
                }
            }
            let fg_c = fg
                .as_ref()
                .and_then(|f| defs.resolve_value(f))
                .unwrap_or(Color::Reset);
            let bg_c = bg
                .as_ref()
                .and_then(|b| defs.resolve_value(b))
                .unwrap_or(Color::Reset);
            Some(ElementStyle {
                fg: fg_c,
                bg: bg_c,
                bold,
                reversed,
            })
        } else {
            let val = raw_value.trim_matches('"');
            defs.resolve_style(val)
        }
    }
}
