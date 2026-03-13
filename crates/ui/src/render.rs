use led_core::theme::Theme;
use led_state::AppState;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::style;

const SIDE_PANEL_WIDTH: u16 = 25;
const MIN_EDITOR_WIDTH: u16 = 25;

pub fn render(state: &AppState, frame: &mut Frame) {
    let theme = match state.config_theme.as_ref() {
        Some(ct) => ct.file.as_ref(),
        None => return,
    };

    let area = frame.area();

    // Vertical split: main area + status bar (1 line at bottom)
    let [main_area, status_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

    render_status_bar(state, theme, frame, status_area);

    // Horizontal split: optional side panel + editor area
    let show_side = state.show_side_panel && main_area.width > SIDE_PANEL_WIDTH + MIN_EDITOR_WIDTH;

    if show_side {
        let [side_area, editor_area] =
            Layout::horizontal([Constraint::Length(SIDE_PANEL_WIDTH), Constraint::Min(1)])
                .areas(main_area);

        render_side_panel(theme, frame, side_area);
        render_editor_area(theme, frame, editor_area);
    } else {
        render_editor_area(theme, frame, main_area);
    }
}

fn render_status_bar(state: &AppState, theme: &Theme, frame: &mut Frame, area: Rect) {
    let status_style = style::resolve(theme, &theme.status_bar.style);

    let name = state
        .workspace
        .as_ref()
        .map(|w| {
            w.root
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| {
            state
                .startup
                .start_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });

    let left = format!(" led: {}", name);

    // Show warning or info message if present
    let msg = state
        .warn
        .as_deref()
        .or(state.info.as_deref())
        .unwrap_or("");

    let total = area.width as usize;
    let content = if msg.is_empty() {
        format!("{:<width$}", left, width = total)
    } else {
        let gap = 2;
        let used = left.len() + gap + msg.len();
        if used <= total {
            format!(
                "{}  {}{:>pad$}",
                left,
                msg,
                "",
                pad = total.saturating_sub(used)
            )
        } else {
            format!("{:<width$}", left, width = total)
        }
    };

    frame.render_widget(Paragraph::new(content).style(status_style), area);
}

fn render_side_panel(theme: &Theme, frame: &mut Frame, area: Rect) {
    let border_style = style::resolve(theme, &theme.browser.border);
    let bg_style = style::resolve(theme, &theme.browser.file);

    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(border_style)
        .style(bg_style);

    frame.render_widget(block, area);
}

fn render_editor_area(theme: &Theme, frame: &mut Frame, area: Rect) {
    // Vertical split: buffer content + tab bar (1 line at bottom)
    let [buffer_area, tab_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

    // Buffer area: editor text background
    let editor_style = style::resolve(theme, &theme.editor.text);
    frame.render_widget(Block::default().style(editor_style), buffer_area);

    // Tab bar: inactive tab background (no tabs yet)
    let tab_style = style::resolve(theme, &theme.tabs.inactive);
    frame.render_widget(Block::default().style(tab_style), tab_area);
}
