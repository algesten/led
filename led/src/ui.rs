use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::widgets::{Block, Borders, Paragraph};

use led_core::{DrawContext, PanelSlot};
use crate::shell::{Shell, Modal};

const GUTTER_WIDTH: u16 = 2;
const SIDE_PANEL_WIDTH: u16 = 25;

pub fn render(shell: &mut Shell, frame: &mut Frame) {
    let area = frame.area();

    let vertical = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);

    let main_area = vertical[0];
    let status_area = vertical[1];

    let show_panel = shell.show_side_panel && main_area.width > SIDE_PANEL_WIDTH + 2;

    if show_panel {
        let horizontal =
            Layout::horizontal([Constraint::Length(SIDE_PANEL_WIDTH), Constraint::Min(1)])
                .split(main_area);

        let browser_area = horizontal[0];
        let main_area_inner = horizontal[1];

        // Draw side panel component
        {
            let focused = shell.focus == PanelSlot::Side;
            let theme = shell.theme.clone();
            let ctx = DrawContext { theme: &theme, focused };
            if let Some(comp) = shell.side_component_mut() {
                comp.draw(frame, browser_area, &ctx);
            }
        }

        render_main_content(shell, frame, main_area_inner);
    } else {
        render_main_content(shell, frame, main_area);
    }

    render_status_bar(shell, frame, status_area);

    if let Some(modal) = &shell.modal {
        render_modal(modal, frame, area);
    }
}

fn render_tab_bar(shell: &Shell, frame: &mut Frame, area: Rect) {
    let theme = &shell.theme;
    let active = shell.active_tab();

    let buf = frame.buffer_mut();
    let mut x = area.x + GUTTER_WIDTH - 1;
    let max_x = area.x + area.width;

    let mut tab_idx: usize = 0;
    for comp in shell.components().iter() {
        let Some(tab) = comp.tab() else { continue };

        if tab_idx > 0 {
            x += 1;
        }

        let prefix = if tab.dirty { "\u{25cf}" } else { "" };
        let filename = &tab.label;
        let max_chars = 15;
        let char_count = prefix.chars().count() + filename.chars().count();
        let truncated = char_count > max_chars;
        let take = if truncated {
            max_chars - prefix.chars().count() - 1
        } else {
            filename.chars().count()
        };
        let label: String = " "
            .chars()
            .chain(prefix.chars())
            .chain(filename.chars().take(take))
            .chain(if truncated { Some('…') } else { None })
            .chain(" ".chars())
            .collect();
        let tab_width = label.chars().count() as u16;

        if x + tab_width > max_x {
            break;
        }

        let style = if tab_idx == active {
            theme.get("tabs.active").to_style()
        } else {
            theme.get("tabs.inactive").to_style()
        };

        buf.set_string(x, area.y, &label, style);
        x += tab_width;
        tab_idx += 1;
    }
}

fn render_main_content(shell: &mut Shell, frame: &mut Frame, area: Rect) {
    if !shell.has_tabs() {
        return;
    }

    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(area);
    render_tab_bar(shell, frame, chunks[0]);

    // Debug flash at top right of tab bar
    if let Some(text) = shell.debug_flash_text() {
        let tab_area = chunks[0];
        let flash_width = text.len() as u16;
        if flash_width < tab_area.width {
            let x = tab_area.x + tab_area.width - flash_width;
            let style = shell.theme.get("editor.gutter").to_style();
            frame.buffer_mut().set_string(x, tab_area.y, text, style);
        }
    }

    let text_area = chunks[1];

    // Get cursor/scroll info from active component
    let comp = shell.active_buffer().unwrap();
    let cursor_row = comp.cursor_position().map_or(0, |(r, _)| r);
    let current_scroll = comp.scroll_offset();
    let visible_height = text_area.height as usize;
    shell.viewport_height = visible_height;
    let scroll = compute_scroll(current_scroll, cursor_row, visible_height);
    shell.active_buffer_mut().unwrap().set_scroll_offset(scroll);

    // Draw the active buffer component
    {
        let focused = shell.focus == PanelSlot::Main;
        let theme = shell.theme.clone();
        let ctx = DrawContext { theme: &theme, focused };
        shell.active_buffer_mut().unwrap().draw(frame, text_area, &ctx);
    }

    // Place cursor (only when main panel focused)
    if shell.focus == PanelSlot::Main {
        let comp = shell.active_buffer().unwrap();
        if let Some((row, col)) = comp.cursor_position() {
            let cursor_screen_row = row.saturating_sub(scroll) as u16;
            let cursor_screen_col = col as u16 + GUTTER_WIDTH;
            frame.set_cursor_position(Position::new(
                text_area.x + cursor_screen_col,
                text_area.y + cursor_screen_row,
            ));
        }
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

fn render_status_bar(shell: &Shell, frame: &mut Frame, area: Rect) {
    let (left, right) = if let Some(comp) = shell.active_buffer() {
        let tab = comp.tab().unwrap_or(led_core::TabDescriptor {
            label: String::new(),
            dirty: false,
            path: None,
        });
        let modified = if tab.dirty { " \u{25cf}" } else { "" };
        let filename = &tab.label;

        let (line, col) = comp
            .status_info()
            .map_or((1, 1), |(_, l, c)| (l, c));
        let pos = format!("L{}:C{}", line, col);
        (format!(" led: {filename}{modified}"), format!("{pos} "))
    } else {
        (" led".to_string(), String::new())
    };

    let padding = (area.width as usize).saturating_sub(left.len() + right.len());
    let bar = format!("{left}{:padding$}{right}", "");

    let style = shell.theme.get("status_bar.style").to_style();
    let paragraph = Paragraph::new(bar).style(style);
    frame.render_widget(paragraph, area);
}

fn render_modal(modal: &Modal, frame: &mut Frame, area: Rect) {
    use ratatui::widgets::Clear;

    let width = (modal.prompt.len() as u16 + 4).min(area.width);
    let height: u16 = 4;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let modal_area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, modal_area);

    let block = Block::default().borders(Borders::ALL);
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    if inner.height > 0 {
        let prompt = Paragraph::new(modal.prompt.as_str()).alignment(Alignment::Left);
        let prompt_area = Rect::new(inner.x, inner.y, inner.width, 1);
        frame.render_widget(prompt, prompt_area);
    }

    if inner.height > 1 {
        let input_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
        let input = Paragraph::new(modal.input.as_str());
        frame.render_widget(input, input_area);

        let cursor_x = input_area.x + modal.input.len() as u16;
        frame.set_cursor_position(Position::new(cursor_x, input_area.y));
    }
}
