use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};


use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::editor::{Editor, Focus};
use crate::file_browser::FileBrowser;
use crate::theme::Theme;

const GUTTER_WIDTH: u16 = 2; // indicator + space
const SIDE_PANEL_WIDTH: u16 = 25;

pub fn render(editor: &mut Editor, frame: &mut Frame) {
    let area = frame.area();

    // Layout: main content area, status bar
    let vertical = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);

    let main_area = vertical[0];
    let status_area = vertical[1];

    // Determine if we show the side panel
    let show_panel = editor.show_side_panel && main_area.width > SIDE_PANEL_WIDTH + 2;

    if show_panel {
        let horizontal =
            Layout::horizontal([Constraint::Length(SIDE_PANEL_WIDTH), Constraint::Min(1)])
                .split(main_area);

        let browser_area = horizontal[0];
        let editor_area = horizontal[1];

        render_file_browser(&editor.file_browser, editor.focus, &editor.theme, frame, browser_area);
        render_editor_content(editor, frame, editor_area);
    } else {
        render_editor_content(editor, frame, main_area);
    }

    render_status_bar(editor, frame, status_area);
}

fn render_tab_bar(editor: &Editor, frame: &mut Frame, area: Rect) {
    let buffers = editor.buffers();
    let active = editor.active_tab();
    let theme = &editor.theme;

    let buf = frame.buffer_mut();
    let mut x = area.x + GUTTER_WIDTH - 1; // align tab text with buffer text
    let max_x = area.x + area.width;

    for (i, b) in buffers.iter().enumerate() {
        if i > 0 {
            x += 1;
        }

        let mut name = b.filename().to_string();
        if b.dirty {
            name.push('\u{25cf}');
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
            theme.tab_active.to_style()
        } else {
            theme.tab_inactive.to_style()
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

    // Debug flash at top right of tab bar
    if let Some(text) = editor.debug_flash_text() {
        let tab_area = chunks[0];
        let flash_width = text.len() as u16;
        if flash_width < tab_area.width {
            let x = tab_area.x + tab_area.width - flash_width;
            let style = editor.theme.gutter.to_style();
            frame.buffer_mut().set_string(x, tab_area.y, text, style);
        }
    }

    let text_area = chunks[1];

    let buf = editor.active_buffer().unwrap();
    let cursor_row = buf.cursor_row;
    let current_scroll = buf.scroll_offset;
    let visible_height = text_area.height as usize;
    editor.viewport_height = visible_height;
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
    let total_lines = buf.line_count();
    let gutter_style = editor.theme.gutter.to_style();
    let text_style = editor.theme.editor_text.to_style();

    let mut display_lines = Vec::with_capacity(height);

    for i in 0..height {
        let line_idx = scroll + i;
        if line_idx < total_lines {
            let gutter = Span::styled("  ", gutter_style);
            let text = Span::styled(buf.line(line_idx).replace('\t', "    "), text_style);
            display_lines.push(Line::from(vec![gutter, text]));
        } else {
            let gutter = Span::styled("~ ", gutter_style);
            display_lines.push(Line::from(vec![gutter]));
        }
    }

    let paragraph = Paragraph::new(display_lines).style(text_style);
    frame.render_widget(paragraph, area);
}

fn render_file_browser(
    browser: &FileBrowser,
    focus: Focus,
    theme: &Theme,
    frame: &mut Frame,
    area: Rect,
) {
    // Block with right border to separate from editor
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(theme.browser_border.to_style());
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
                    theme.browser_selected.to_style()
                } else {
                    theme.browser_selected_unfocused.to_style()
                }
            } else if is_dir {
                theme.browser_dir.to_style()
            } else {
                theme.browser_file.to_style()
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
        let modified = if buf.dirty { " \u{25cf}" } else { "" };
        let pos = format!("L{}:C{}", buf.cursor_row + 1, buf.cursor_col + 1,);
        (format!(" led: {filename}{modified}"), format!("{pos} "))
    } else {
        (" led".to_string(), String::new())
    };

    let padding = (area.width as usize).saturating_sub(left.len() + right.len());
    let bar = format!("{left}{:padding$}{right}", "");

    let style = editor.theme.status_bar.to_style();

    let paragraph = Paragraph::new(bar).style(style);
    frame.render_widget(paragraph, area);
}

