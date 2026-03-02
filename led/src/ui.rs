use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::editor::{Editor, Focus};
use crate::file_browser::FileBrowser;

const GUTTER_WIDTH: u16 = 4; // "NNN│" = 3 digits + separator
const SIDE_PANEL_WIDTH: u16 = 25;

pub fn render(editor: &mut Editor, frame: &mut Frame) {
    let area = frame.area();

    // Layout: main content area, status bar, message bar
    let vertical = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    let main_area = vertical[0];
    let status_area = vertical[1];
    let message_area = vertical[2];

    // Determine if we show the side panel
    let show_panel = editor.show_side_panel && main_area.width > SIDE_PANEL_WIDTH + 2;

    if show_panel {
        let horizontal =
            Layout::horizontal([Constraint::Length(SIDE_PANEL_WIDTH), Constraint::Min(1)])
                .split(main_area);

        let browser_area = horizontal[0];
        let editor_area = horizontal[1];

        render_file_browser(&editor.file_browser, editor.focus, frame, browser_area);
        render_editor_content(editor, frame, editor_area);
    } else {
        render_editor_content(editor, frame, main_area);
    }

    render_status_bar(editor, frame, status_area);
    render_message_bar(editor, frame, message_area);
}

fn render_tab_bar(editor: &Editor, frame: &mut Frame, area: Rect) {
    let buffers = editor.buffers();
    let active = editor.active_tab();

    let buf = frame.buffer_mut();
    let mut x = area.x;
    let max_x = area.x + area.width;

    for (i, b) in buffers.iter().enumerate() {
        if i > 0 {
            x += 1;
        }

        let mut name = b.filename().to_string();
        if b.dirty {
            name.push('*');
        }
        if name.len() > 15 {
            name.truncate(14);
            name.push('…');
        }
        let label = format!(" {name} ");
        let tab_width = label.chars().count() as u16;

        if x + tab_width > max_x {
            break;
        }

        let style = if i == active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::White)
        } else {
            Style::default()
                .fg(Color::Gray)
                .bg(Color::DarkGray)
        };

        buf.set_string(x, area.y, &label, style);
        x += tab_width;
    }
}

fn render_editor_content(editor: &mut Editor, frame: &mut Frame, area: Rect) {
    if editor.buffers().is_empty() {
        return;
    }

    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(area);
    render_tab_bar(editor, frame, chunks[0]);
    let text_area = chunks[1];

    let buf = editor.active_buffer().unwrap();
    let cursor_row = buf.cursor_row;
    let current_scroll = buf.scroll_offset;
    let visible_height = text_area.height as usize;
    let scroll = compute_scroll(current_scroll, cursor_row, visible_height);
    editor.active_buffer_mut().unwrap().scroll_offset = scroll;

    render_text(editor, frame, text_area, scroll);

    // Place cursor (only when editor focused)
    if editor.focus == Focus::Editor {
        let buf = editor.active_buffer().unwrap();
        let cursor_screen_row = buf.cursor_row.saturating_sub(scroll) as u16;
        let cursor_screen_col = buf.cursor_col as u16 + GUTTER_WIDTH;
        frame.set_cursor_position(Position::new(
            text_area.x + cursor_screen_col,
            text_area.y + cursor_screen_row,
        ));
    }
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
    let buf = editor.active_buffer().unwrap();
    let total_lines = buf.lines.len();
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
            let text = Span::raw(buf.lines[line_idx].replace('\t', "    "));
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

fn render_file_browser(browser: &FileBrowser, focus: Focus, frame: &mut Frame, area: Rect) {
    // Block with right border to separate from editor
    let block = Block::default().borders(Borders::RIGHT);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;
    if height == 0 {
        return;
    }

    // Compute scroll for browser
    let browser_scroll = if browser.selected >= height {
        browser.selected - height + 1
    } else {
        0
    };

    let mut lines = Vec::with_capacity(height);

    for i in 0..height {
        let idx = browser_scroll + i;
        if idx < browser.entries.len() {
            let entry = &browser.entries[idx];
            let name = FileBrowser::display_name(entry);

            // Truncate to fit panel width
            let max_width = inner.width as usize;
            let display: String = if name.len() > max_width {
                name[..max_width].to_string()
            } else {
                name
            };

            let is_selected = idx == browser.selected;
            let is_dir = matches!(entry.kind, crate::file_browser::EntryKind::Directory { .. });

            let style = if is_selected {
                if focus == Focus::Browser {
                    Style::default().bg(Color::White).fg(Color::Black)
                } else {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                }
            } else if is_dir {
                Style::default().fg(Color::Blue)
            } else {
                Style::default()
            };

            // Pad to fill the line
            let padded = format!("{:<width$}", display, width = max_width);
            lines.push(Line::from(Span::styled(padded, style)));
        } else {
            lines.push(Line::from(""));
        }
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);

}

fn render_status_bar(editor: &Editor, frame: &mut Frame, area: Rect) {
    let (left, right) = if let Some(buf) = editor.active_buffer() {
        let filename = buf.filename();
        let modified = if buf.dirty { " [modified]" } else { "" };
        let pos = format!("L{}:C{}", buf.cursor_row + 1, buf.cursor_col + 1,);
        (format!(" led: {filename}{modified}"), format!("{pos} "))
    } else {
        (" led".to_string(), String::new())
    };

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
