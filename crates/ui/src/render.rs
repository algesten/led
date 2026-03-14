use led_core::theme::Theme;
use led_state::{AppState, BufferState};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
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
        render_editor_area(state, theme, frame, editor_area);
    } else {
        render_editor_area(state, theme, frame, main_area);
    }
}

fn active_buffer(state: &AppState) -> Option<&BufferState> {
    let id = state.active_buffer?;
    state.buffers.get(&id)
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

    // Right side: active buffer filename + cursor position
    let right = if let Some(buf) = active_buffer(state) {
        let fname = buf
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("{} L{}:C{} ", fname, buf.cursor_row + 1, buf.cursor_col + 1)
    } else {
        String::new()
    };

    let total = area.width as usize;
    let content = if msg.is_empty() && right.is_empty() {
        format!("{:<width$}", left, width = total)
    } else if msg.is_empty() {
        let pad = total.saturating_sub(left.len() + right.len());
        format!("{}{:>pad$}{}", left, "", right, pad = pad)
    } else {
        let gap = 2;
        let used = left.len() + gap + msg.len() + gap + right.len();
        if used <= total {
            let mid_pad = total.saturating_sub(left.len() + gap + msg.len() + right.len());
            format!("{}  {}{:>pad$}{}", left, msg, "", right, pad = mid_pad)
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

fn render_editor_area(state: &AppState, theme: &Theme, frame: &mut Frame, area: Rect) {
    // Vertical split: buffer content + tab bar (1 line at bottom)
    let [buffer_area, tab_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

    render_tab_bar(state, theme, frame, tab_area);

    if let Some(buf) = active_buffer(state) {
        render_buffer(buf, theme, frame, buffer_area);
    } else {
        // Empty editor background
        let editor_style = style::resolve(theme, &theme.editor.text);
        frame.render_widget(Block::default().style(editor_style), buffer_area);
    }
}

fn render_tab_bar(state: &AppState, theme: &Theme, frame: &mut Frame, area: Rect) {
    let inactive_style = style::resolve(theme, &theme.tabs.inactive);
    // Fill background
    frame.render_widget(Block::default().style(inactive_style), area);

    let mut tabs: Vec<&BufferState> = state.buffers.values().collect();
    tabs.sort_by_key(|b| b.tab_order);

    let active_id = state.active_buffer;
    let mut x = area.x;

    for buf in &tabs {
        let name = buf
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("[{}]", buf.id.0));

        let label = format!(" {} ", name);
        let width = label.len() as u16;

        if x + width > area.x + area.width {
            break;
        }

        let is_active = active_id == Some(buf.id);
        let tab_style = if is_active {
            style::resolve(theme, &theme.tabs.active)
        } else {
            inactive_style
        };

        let tab_area = Rect::new(x, area.y, width, 1);
        frame.render_widget(Paragraph::new(label).style(tab_style), tab_area);

        x += width;
    }
}

fn gutter_width(line_count: usize) -> u16 {
    let mut n = line_count.max(1);
    let mut digits: u16 = 0;
    while n > 0 {
        digits += 1;
        n /= 10;
    }
    digits + 2
}

fn render_buffer(buf: &BufferState, theme: &Theme, frame: &mut Frame, area: Rect) {
    let line_count = buf.doc.line_count();
    let gutter_w = gutter_width(line_count);

    let [gutter_area, text_area] = Layout::horizontal([
        Constraint::Length(gutter_w),
        Constraint::Min(1),
    ])
    .areas(area);

    let gutter_style = style::resolve(theme, &theme.editor.gutter);
    let text_style = style::resolve(theme, &theme.editor.text);

    // Fill backgrounds
    frame.render_widget(Block::default().style(gutter_style), gutter_area);
    frame.render_widget(Block::default().style(text_style), text_area);

    let visible_rows = area.height as usize;
    let scroll = buf.scroll_row;
    let num_width = (gutter_w - 2) as usize;

    for row in 0..visible_rows {
        let line_idx = scroll + row;
        let y = area.y + row as u16;

        if line_idx < line_count {
            // Gutter: right-aligned line number with padding
            let num = format!(" {:>width$} ", line_idx + 1, width = num_width);
            let gutter_line_area = Rect::new(gutter_area.x, y, gutter_w, 1);
            frame.render_widget(Paragraph::new(num).style(gutter_style), gutter_line_area);

            // Text content
            let line = buf.doc.line(line_idx);
            let text_line_area = Rect::new(text_area.x, y, text_area.width, 1);
            frame.render_widget(Paragraph::new(line).style(text_style), text_line_area);
        }
    }

    // Cursor
    let cursor_row_on_screen = buf.cursor_row.saturating_sub(scroll);
    if cursor_row_on_screen < visible_rows {
        let cursor_x = text_area.x + buf.cursor_col as u16;
        let cursor_y = text_area.y + cursor_row_on_screen as u16;
        frame.set_cursor_position(Position::new(cursor_x, cursor_y));
    }
}
