use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::editor::Editor;

const GUTTER_WIDTH: u16 = 4; // "NNN│" = 3 digits + separator

pub fn render(editor: &mut Editor, frame: &mut Frame) {
    let area = frame.area();

    // Layout: text area, status bar, message bar
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    let text_area = chunks[0];
    let status_area = chunks[1];
    let message_area = chunks[2];

    // Scroll offset
    let visible_height = text_area.height as usize;
    let scroll = compute_scroll(editor.scroll_offset, editor.buffer.cursor_row, visible_height);
    editor.scroll_offset = scroll;

    render_text(editor, frame, text_area, scroll);
    render_status_bar(editor, frame, status_area);
    render_message_bar(editor, frame, message_area);

    // Place cursor
    let cursor_screen_row = editor.buffer.cursor_row.saturating_sub(scroll) as u16;
    let cursor_screen_col = editor.buffer.cursor_col as u16 + GUTTER_WIDTH;
    frame.set_cursor_position(Position::new(
        text_area.x + cursor_screen_col,
        text_area.y + cursor_screen_row,
    ));
}

fn compute_scroll(current: usize, cursor_row: usize, height: usize) -> usize {
    let mut scroll = current;
    if cursor_row < scroll {
        scroll = cursor_row;
    } else if cursor_row >= scroll + height {
        scroll = cursor_row - height + 1;
    }
    scroll
}

fn render_text(editor: &Editor, frame: &mut Frame, area: Rect, scroll: usize) {
    let height = area.height as usize;
    let total_lines = editor.buffer.lines.len();
    let line_num_width = total_lines.to_string().len().max(3);

    let mut display_lines = Vec::with_capacity(height);

    for i in 0..height {
        let line_idx = scroll + i;
        if line_idx < total_lines {
            let num = format!("{:>width$}", line_idx + 1, width = line_num_width);
            let gutter = Span::styled(
                format!("{num}\u{2502}"),
                Style::default().fg(Color::DarkGray),
            );
            let text = Span::raw(&editor.buffer.lines[line_idx]);
            display_lines.push(Line::from(vec![gutter, text]));
        } else {
            let gutter = Span::styled(
                format!("{:>width$}\u{2502}", "~", width = line_num_width),
                Style::default().fg(Color::DarkGray),
            );
            display_lines.push(Line::from(vec![gutter]));
        }
    }

    let paragraph = Paragraph::new(display_lines);
    frame.render_widget(paragraph, area);
}

fn render_status_bar(editor: &Editor, frame: &mut Frame, area: Rect) {
    let filename = editor.buffer.filename();
    let modified = if editor.buffer.dirty { " [modified]" } else { "" };
    let pos = format!(
        "L{}:C{}",
        editor.buffer.cursor_row + 1,
        editor.buffer.cursor_col + 1,
    );

    let left = format!(" led: {filename}{modified}");
    let right = format!("{pos} ");
    let padding = (area.width as usize).saturating_sub(left.len() + right.len());
    let bar = format!("{left}{:padding$}{right}", "");

    let style = Style::default()
        .fg(Color::Black)
        .bg(Color::White)
        .add_modifier(Modifier::BOLD);

    let paragraph = Paragraph::new(bar).style(style);
    frame.render_widget(paragraph, area);
}

fn render_message_bar(editor: &Editor, frame: &mut Frame, area: Rect) {
    let content = if let Some((label, input)) = editor.prompt_display() {
        format!("{label}{input}")
    } else if editor.is_chord_pending() {
        "Ctrl-X-".into()
    } else if let Some(ref msg) = editor.message {
        msg.clone()
    } else {
        String::new()
    };

    let paragraph = Paragraph::new(content);
    frame.render_widget(paragraph, area);
}
