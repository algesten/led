use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use led_core::theme::{StyleTable, StyleValue, Theme};
use ratatui::style::{Color, Modifier, Style};

const MAX_DEPTH: usize = 16;

/// Resolve a theme StyleValue to a ratatui Style.
pub fn resolve(theme: &Theme, sv: &StyleValue) -> Style {
    resolve_depth(theme, sv, MAX_DEPTH)
}

/// Cached version of [`resolve`]. Uses pointer identity on the `&Theme` (stable
/// because it lives behind an `Arc`) and on the `&StyleValue` (a field within
/// that same `Theme`). The cache clears automatically when the theme changes.
pub fn resolve_cached(theme: &Theme, sv: &StyleValue) -> Style {
    thread_local! {
        static CACHE: RefCell<(usize, HashMap<usize, Style>)> =
            RefCell::new((0, HashMap::new()));
    }
    let theme_ptr = theme as *const Theme as usize;
    CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache.0 != theme_ptr {
            cache.0 = theme_ptr;
            cache.1.clear();
        }
        let sv_ptr = sv as *const StyleValue as usize;
        *cache.1.entry(sv_ptr).or_insert_with(|| resolve(theme, sv))
    })
}

/// Pre-resolve all syntax.{name} entries from the theme into a style map.
/// Cached by Arc pointer identity — only recomputes when the theme Arc changes.
pub fn resolve_syntax_map(theme: &Arc<Theme>) -> Rc<HashMap<String, Style>> {
    thread_local! {
        static CACHE: RefCell<Option<(usize, Rc<HashMap<String, Style>>)>> = RefCell::new(None);
    }
    let ptr = Arc::as_ptr(theme) as usize;
    CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        if let Some((cached_ptr, ref map)) = *cache {
            if cached_ptr == ptr {
                return map.clone();
            }
        }
        let map: Rc<HashMap<String, Style>> = Rc::new(
            theme
                .syntax
                .iter()
                .map(|(name, sv)| (name.clone(), resolve(theme, sv)))
                .collect(),
        );
        *cache = Some((ptr, map.clone()));
        map
    })
}

/// Resolve a capture name to a style, with parent fallback.
/// E.g. "function.call" → try "function.call", then "function", then text_style.
pub fn resolve_capture_style(
    capture_name: &str,
    syntax_styles: &HashMap<String, Style>,
    text_style: Style,
) -> Style {
    if let Some(s) = syntax_styles.get(capture_name) {
        return *s;
    }
    if let Some(dot) = capture_name.find('.') {
        let parent = &capture_name[..dot];
        if let Some(s) = syntax_styles.get(parent) {
            return *s;
        }
    }
    text_style
}

fn resolve_depth(theme: &Theme, sv: &StyleValue, depth: usize) -> Style {
    if depth == 0 {
        return Style::default();
    }
    match sv {
        StyleValue::Style(st) => resolve_table(theme, st, depth),
        StyleValue::Scalar(s) => resolve_ref(theme, s, depth),
    }
}

fn resolve_ref(theme: &Theme, s: &str, depth: usize) -> Style {
    if depth == 0 {
        return Style::default();
    }
    if let Some(name) = s.strip_prefix('$') {
        match theme.colors.get(name) {
            Some(sv) => resolve_depth(theme, sv, depth - 1),
            None => Style::default(),
        }
    } else {
        Style::default().fg(parse_color(s))
    }
}

fn resolve_table(theme: &Theme, st: &StyleTable, depth: usize) -> Style {
    let mut style = Style::default();
    if let Some(fg) = &st.fg {
        style = style.fg(resolve_color(theme, fg, depth - 1));
    }
    if let Some(bg) = &st.bg {
        style = style.bg(resolve_color(theme, bg, depth - 1));
    }
    if st.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if st.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    style
}

fn resolve_color(theme: &Theme, s: &str, depth: usize) -> Color {
    if depth == 0 {
        return Color::Reset;
    }
    if let Some(name) = s.strip_prefix('$') {
        match theme.colors.get(name) {
            Some(StyleValue::Scalar(inner)) => resolve_color(theme, inner, depth - 1),
            Some(StyleValue::Style(st)) => st
                .fg
                .as_ref()
                .map_or(Color::Reset, |fg| resolve_color(theme, fg, depth - 1)),
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
    let (r, g, b) = match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).unwrap_or(0);
            let g = u8::from_str_radix(&hex[1..2], 16).unwrap_or(0);
            let b = u8::from_str_radix(&hex[2..3], 16).unwrap_or(0);
            (r * 17, g * 17, b * 17)
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
            let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
            let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
            (r, g, b)
        }
        _ => return Color::Reset,
    };
    make_color(r, g, b)
}

fn supports_truecolor() -> bool {
    use std::sync::OnceLock;
    static TRUECOLOR: OnceLock<bool> = OnceLock::new();
    *TRUECOLOR.get_or_init(|| {
        std::env::var("COLORTERM")
            .map(|v| v == "truecolor" || v == "24bit")
            .unwrap_or(false)
    })
}

fn make_color(r: u8, g: u8, b: u8) -> Color {
    if supports_truecolor() {
        Color::Rgb(r, g, b)
    } else {
        rgb_to_indexed(r, g, b)
    }
}

fn rgb_to_indexed(r: u8, g: u8, b: u8) -> Color {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn default_theme() -> Theme {
        let toml = include_str!("../../config-file/src/default_theme.toml");
        toml::from_str(toml).expect("default theme must parse")
    }

    #[test]
    fn parse_hex_maps_to_indexed_without_truecolor() {
        // Without COLORTERM=truecolor, hex colors map to indexed
        // #005f87 → RGB(0,95,135) → xterm color 24
        assert_eq!(parse_hex("#005f87"), Color::Indexed(24));
    }

    #[test]
    fn parse_hex_3_digit() {
        let c = parse_hex("#fff");
        assert!(matches!(c, Color::Indexed(_)), "got {:?}", c);
    }

    #[test]
    fn parse_ansi_names() {
        assert_eq!(parse_color("term_reset"), Color::Reset);
        assert_eq!(parse_color("ansi_red"), Color::Red);
    }

    #[test]
    fn resolve_scalar_direct_color() {
        let theme = default_theme();
        // editor.text = "$normal" → "term_reset" → fg=Reset
        let style = resolve(&theme, &theme.editor.text);
        assert_eq!(style.fg, Some(Color::Reset));
    }

    #[test]
    fn resolve_scalar_chain() {
        let theme = default_theme();
        // editor.gutter = "$muted" → "$theme_dark" → "$x024" → "#005f87" → Indexed(24)
        let style = resolve(&theme, &theme.editor.gutter);
        assert_eq!(style.fg, Some(Color::Indexed(24)));
    }

    #[test]
    fn resolve_table_with_fg_and_bg() {
        let theme = default_theme();
        // status_bar.style = { fg = "$bold", bg = "$muted" }
        let style = resolve(&theme, &theme.status_bar.style);
        assert!(style.fg.is_some(), "should have fg");
        assert!(style.bg.is_some(), "should have bg");
        assert!(matches!(style.fg, Some(Color::Indexed(_))));
        assert!(matches!(style.bg, Some(Color::Indexed(_))));
    }

    #[test]
    fn resolve_tabs_active_through_alias_to_style() {
        let theme = default_theme();
        let style = resolve(&theme, &theme.tabs.active);
        assert!(style.fg.is_some(), "should have fg");
        assert!(style.bg.is_some(), "should have bg");
    }

    #[test]
    fn resolve_tabs_inactive_through_alias_to_style() {
        let theme = default_theme();
        let style = resolve(&theme, &theme.tabs.inactive);
        assert!(style.fg.is_some(), "should have fg");
        assert!(style.bg.is_some(), "should have bg");
    }

    #[test]
    fn status_bar_renders_with_bg_color() {
        use ratatui::backend::TestBackend;
        use ratatui::text::Span;
        use ratatui::widgets::Paragraph;

        let theme = default_theme();
        let status_style = resolve(&theme, &theme.status_bar.style);

        let backend = TestBackend::new(20, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let frame = terminal
            .draw(|f| {
                let area = f.area();
                let p = Paragraph::new(Span::styled("test", status_style)).style(status_style);
                f.render_widget(p, area);
            })
            .unwrap();

        let cell = &frame.buffer[(0, 0)];
        assert_ne!(cell.bg, Color::Reset, "status bar bg should not be Reset");
        assert_ne!(cell.fg, Color::Reset, "status bar fg should not be Reset");

        let empty_cell = &frame.buffer[(10, 0)];
        assert_ne!(
            empty_cell.bg,
            Color::Reset,
            "empty cell should inherit Paragraph bg"
        );
    }

    #[test]
    fn display_lines_render_with_gutter_style() {
        use ratatui::backend::TestBackend;
        use ratatui::text::{Line, Span};
        use ratatui::widgets::Paragraph;

        let theme = default_theme();
        let gutter_style = resolve(&theme, &theme.editor.gutter);
        let text_style = resolve(&theme, &theme.editor.text);

        let line = Line::from(vec![
            Span::styled("  ", gutter_style),
            Span::styled("hello", text_style),
        ]);

        let backend = TestBackend::new(20, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let frame = terminal
            .draw(|f| {
                let area = f.area();
                let p = Paragraph::new(vec![line]).style(text_style);
                f.render_widget(p, area);
            })
            .unwrap();

        let gutter_cell = &frame.buffer[(0, 0)];
        assert_eq!(
            gutter_cell.fg,
            Color::Indexed(24),
            "gutter fg should be xterm color 24"
        );

        let text_cell = &frame.buffer[(2, 0)];
        assert_eq!(text_cell.symbol(), "h");
    }

    #[test]
    fn full_render_applies_styles() {
        use ratatui::backend::TestBackend;
        use ratatui::text::{Line, Span};

        use crate::display::{self, LayoutInfo, OverlayContent, TabEntry, TabsInputs};
        use crate::render;
        use led_state::Dimensions;

        let theme = default_theme();
        let status_style = resolve(&theme, &theme.status_bar.style);
        let text_style = resolve(&theme, &theme.editor.text);
        let gutter_style = resolve(&theme, &theme.editor.gutter);
        let tab_active = resolve(&theme, &theme.tabs.active);
        let tab_inactive = resolve(&theme, &theme.tabs.inactive);
        let side_border = resolve(&theme, &theme.browser.border);
        let side_bg = resolve(&theme, &theme.browser.file);

        let dims = Dimensions::new(40, 10, false);
        let layout = LayoutInfo {
            dims,
            force_redraw: led_core::RedrawSeq(0),
            side_border_style: side_border,
            side_bg_style: side_bg,
            text_style,
            status_style,
        };

        let display_lines = vec![Line::from(vec![
            Span::styled("  ", gutter_style),
            Span::styled("hello world", text_style),
        ])];

        let tabs = TabsInputs {
            entries: vec![TabEntry {
                label: " test.rs ".into(),
                is_active: true,
                style: tab_active,
            }],
            inactive_style: tab_inactive,
            gutter_width: 2,
        };

        let status = display::StatusContent {
            text: " test.rs              L1:C1 ".to_string(),
            is_warn: false,
        };

        let backend = TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let frame = terminal
            .draw(|f| {
                render::render(
                    f,
                    &layout,
                    &display_lines,
                    Some((2, 0)),
                    &status,
                    &tabs,
                    &[],
                    &OverlayContent::None,
                );
            })
            .unwrap();

        // Status bar (row 9)
        let status_cell = &frame.buffer[(1, 9)];
        assert_ne!(
            status_cell.bg,
            Color::Reset,
            "status bar bg should be themed, got {:?}",
            status_cell.bg
        );

        // Tab label (row 8, offset by gutter_width-1=1)
        let tab_cell = &frame.buffer[(2, 8)];
        assert_ne!(
            tab_cell.bg,
            Color::Reset,
            "tab label bg should be themed, got {:?}",
            tab_cell.bg
        );

        // Gutter (col 0, row 0) fg
        let gutter_cell = &frame.buffer[(0, 0)];
        assert_ne!(
            gutter_cell.fg,
            Color::Reset,
            "gutter fg should be themed, got {:?}",
            gutter_cell.fg
        );
    }

    #[test]
    fn full_pipeline_from_appstate() {
        use std::sync::Arc;

        use ratatui::backend::TestBackend;

        use crate::display::{self, OverlayContent};
        use crate::render;

        use led_core::{CanonPath, Startup, TextDoc, UserPath};
        use led_state::{AppState, BufferState, Dimensions};

        let theme_toml = include_str!("../../config-file/src/default_theme.toml");
        let theme: led_core::theme::Theme = toml::from_str(theme_toml).unwrap();
        let config_theme = led_config_file::ConfigFile {
            file: Arc::new(theme),
        };

        let test_path: CanonPath = UserPath::new("test.rs").canonicalize();
        let doc = TextDoc::from_reader("hello world\n".as_bytes()).unwrap();
        let mut buf = BufferState::new(test_path.clone());
        buf.materialize(Arc::new(doc), false);

        let mut state = AppState::new(Startup {
            headless: true,
            enable_watchers: false,
            arg_paths: vec![],
            arg_dir: None,
            start_dir: Arc::new(UserPath::new("/tmp").canonicalize()),
            user_start_dir: UserPath::new("/tmp"),
            config_dir: UserPath::new("/tmp/config"),
            test_lsp_server: None,
            test_gh_binary: None,
            no_workspace: false,
        });
        state.dims = Some(Dimensions::new(40, 10, false));
        state.config_theme = Some(config_theme);
        state.active_tab = Some(test_path.clone());
        state.tabs.push_back(led_state::Tab::new(test_path.clone()));
        std::rc::Rc::make_mut(&mut state.buffers).insert(test_path, std::rc::Rc::new(buf));

        let display_inputs = display::display_inputs(&state).expect("display_inputs");
        let cursor_inputs = display::cursor_inputs(&state).expect("cursor_inputs");
        let status_inputs = display::status_inputs(&state);
        let tabs_inputs = display::tabs_inputs(&state).expect("tabs_inputs");
        let layout_inputs = display::layout_inputs(&state);
        let layout = display::build_layout(&layout_inputs).expect("build_layout");

        let lines = display::build_display_lines(&display_inputs);
        let cursor = display::compute_cursor_pos(&cursor_inputs);
        let status = display::build_status_content(&status_inputs);
        let tabs = display::build_tab_entries(&tabs_inputs);

        let backend = TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let frame = terminal
            .draw(|f| {
                render::render(
                    f,
                    &layout,
                    &lines,
                    cursor,
                    &status,
                    &tabs,
                    &[],
                    &OverlayContent::None,
                );
            })
            .unwrap();

        // Status bar (row 9)
        let status_cell = &frame.buffer[(1, 9)];
        assert_ne!(
            status_cell.bg,
            Color::Reset,
            "status bar bg should be themed, got {:?}",
            status_cell.bg,
        );

        // Tab label (row 8, offset by gutter_width-1=1)
        let tab_bg_cell = &frame.buffer[(2, 8)];
        assert_ne!(
            tab_bg_cell.bg,
            Color::Reset,
            "tab label bg should be themed, got {:?}",
            tab_bg_cell.bg,
        );
    }

    #[test]
    fn resolve_circular_ref_does_not_crash() {
        let mut theme = default_theme();
        theme
            .colors
            .insert("a".into(), StyleValue::Scalar("$b".into()));
        theme
            .colors
            .insert("b".into(), StyleValue::Scalar("$a".into()));
        let style = resolve(&theme, &StyleValue::Scalar("$a".into()));
        let _ = style;
    }

    #[test]
    fn resolve_self_ref_does_not_crash() {
        let mut theme = default_theme();
        theme
            .colors
            .insert("self_ref".into(), StyleValue::Scalar("$self_ref".into()));
        let style = resolve(&theme, &StyleValue::Scalar("$self_ref".into()));
        let _ = style;
    }

    #[test]
    fn resolve_circular_table_does_not_crash() {
        let mut theme = default_theme();
        theme.colors.insert(
            "cyc".into(),
            StyleValue::Style(StyleTable {
                fg: Some("$cyc".into()),
                bg: None,
                bold: false,
                italic: false,
            }),
        );
        let style = resolve(&theme, &StyleValue::Scalar("$cyc".into()));
        let _ = style;
    }

    #[test]
    fn rgb_to_indexed_maps_exact_cube_values() {
        // #005f87 = RGB(0,95,135) → cube indices (0,1,2) → 16 + 0 + 6 + 2 = 24
        assert_eq!(rgb_to_indexed(0, 95, 135), Color::Indexed(24));
        // #ffaf87 = RGB(255,175,135) → cube indices (5,3,2) → 16 + 180 + 18 + 2 = 216
        assert_eq!(rgb_to_indexed(255, 175, 135), Color::Indexed(216));
    }
}
