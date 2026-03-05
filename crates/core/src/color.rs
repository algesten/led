use std::sync::OnceLock;

use ratatui::style::Color;

pub fn supports_truecolor() -> bool {
    static TRUECOLOR: OnceLock<bool> = OnceLock::new();
    *TRUECOLOR.get_or_init(|| {
        std::env::var("COLORTERM")
            .map(|v| v == "truecolor" || v == "24bit")
            .unwrap_or(false)
    })
}

/// Map an RGB color to the nearest xterm 256-color index.
pub fn rgb_to_indexed(r: u8, g: u8, b: u8) -> Color {
    const CUBE_VALUES: [u8; 6] = [0, 95, 135, 175, 215, 255];

    fn cube_index(v: u8) -> usize {
        match v {
            0..=47 => 0,
            48..=114 => 1,
            115..=154 => 2,
            155..=194 => 3,
            195..=234 => 4,
            _ => 5,
        }
    }

    let ri = cube_index(r);
    let gi = cube_index(g);
    let bi = cube_index(b);

    let cr = CUBE_VALUES[ri] as i32;
    let cg = CUBE_VALUES[gi] as i32;
    let cb = CUBE_VALUES[bi] as i32;
    let cube_dist = (r as i32 - cr).pow(2) + (g as i32 - cg).pow(2) + (b as i32 - cb).pow(2);
    let cube_idx = 16 + 36 * ri + 6 * gi + bi;

    // Check grayscale ramp (indices 232-255, values 8, 18, 28, ..., 238)
    let avg = (r as u16 + g as u16 + b as u16) / 3;
    let gi_gray = if avg < 4 {
        0usize
    } else if avg > 243 {
        23
    } else {
        ((avg as usize - 8 + 5) / 10).min(23)
    };
    let gv = (8 + 10 * gi_gray) as i32;
    let gray_dist = (r as i32 - gv).pow(2) + (g as i32 - gv).pow(2) + (b as i32 - gv).pow(2);

    if gray_dist < cube_dist {
        Color::Indexed((232 + gi_gray) as u8)
    } else {
        Color::Indexed(cube_idx as u8)
    }
}

pub fn make_color(r: u8, g: u8, b: u8) -> Color {
    if supports_truecolor() {
        Color::Rgb(r, g, b)
    } else {
        rgb_to_indexed(r, g, b)
    }
}

pub fn parse_ansi(s: &str) -> Option<Color> {
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
            Some(make_color(r, g, b))
        }
        _ if s.starts_with('#') && s.len() == 7 => {
            let r = u8::from_str_radix(&s[1..3], 16).ok()?;
            let g = u8::from_str_radix(&s[3..5], 16).ok()?;
            let b = u8::from_str_radix(&s[5..7], 16).ok()?;
            Some(make_color(r, g, b))
        }
        _ => None,
    }
}
