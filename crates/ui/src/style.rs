use led_core::theme::{StyleTable, StyleValue, Theme};
use ratatui::style::{Color, Modifier, Style};

/// Resolve a theme StyleValue to a ratatui Style.
pub fn resolve(theme: &Theme, sv: &StyleValue) -> Style {
    match sv {
        StyleValue::Style(st) => resolve_table(theme, st),
        StyleValue::Scalar(s) => resolve_ref(theme, s),
    }
}

/// Follow a `$name` reference chain until we reach a terminal color or a StyleTable.
fn resolve_ref(theme: &Theme, s: &str) -> Style {
    if let Some(name) = s.strip_prefix('$') {
        match theme.colors.get(name) {
            Some(sv) => resolve(theme, sv),
            None => Style::default(),
        }
    } else {
        Style::default().fg(parse_color(s))
    }
}

fn resolve_table(theme: &Theme, st: &StyleTable) -> Style {
    let mut style = Style::default();
    if let Some(fg) = &st.fg {
        style = style.fg(resolve_color(theme, fg));
    }
    if let Some(bg) = &st.bg {
        style = style.bg(resolve_color(theme, bg));
    }
    if st.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

/// Follow a `$name` reference chain expecting a single color at the end.
fn resolve_color(theme: &Theme, s: &str) -> Color {
    if let Some(name) = s.strip_prefix('$') {
        match theme.colors.get(name) {
            Some(StyleValue::Scalar(inner)) => resolve_color(theme, inner),
            Some(StyleValue::Style(st)) => st
                .fg
                .as_ref()
                .map_or(Color::Reset, |fg| resolve_color(theme, fg)),
            None => Color::Reset,
        }
    } else {
        parse_color(s)
    }
}

fn parse_color(s: &str) -> Color {
    match s {
        "term_reset" => Color::Reset,
        "ansi_black" => Color::Black,
        "ansi_red" => Color::Red,
        "ansi_green" => Color::Green,
        "ansi_yellow" => Color::Yellow,
        "ansi_blue" => Color::Blue,
        "ansi_magenta" => Color::Magenta,
        "ansi_cyan" => Color::Cyan,
        "ansi_white" => Color::Gray,
        "ansi_bright_black" | "ansi_gray" => Color::DarkGray,
        "ansi_bright_red" => Color::LightRed,
        "ansi_bright_green" => Color::LightGreen,
        "ansi_bright_yellow" => Color::LightYellow,
        "ansi_bright_blue" => Color::LightBlue,
        "ansi_bright_magenta" => Color::LightMagenta,
        "ansi_bright_cyan" => Color::LightCyan,
        "ansi_bright_white" => Color::White,
        s if s.starts_with('#') => parse_hex(s),
        _ => Color::Reset,
    }
}

fn parse_hex(s: &str) -> Color {
    let hex = &s[1..];
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).unwrap_or(0);
            let g = u8::from_str_radix(&hex[1..2], 16).unwrap_or(0);
            let b = u8::from_str_radix(&hex[2..3], 16).unwrap_or(0);
            Color::Rgb(r * 17, g * 17, b * 17)
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
            let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
            let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
            Color::Rgb(r, g, b)
        }
        _ => Color::Reset,
    }
}
