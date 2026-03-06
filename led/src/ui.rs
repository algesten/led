use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::shell::{Modal, Shell};
use led_core::{DrawContext, PanelSlot};

const GUTTER_WIDTH: u16 = 2;
const SIDE_PANEL_WIDTH: u16 = 25;

pub fn render(shell: &mut Shell, frame: &mut Frame) {
    let area = frame.area();

    let vertical = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

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
        if shell.focus == PanelSlot::Side {
            let theme = shell.theme.clone();
            let mut ctx = DrawContext {
                theme: &theme,
                focused: true,
                cursor_pos: None,
                slot: PanelSlot::Side,
            };
            if let Some(comp) = shell.side_component_mut() {
                comp.draw(frame, browser_area, &mut ctx);
            }
            if let Some((x, y)) = ctx.cursor_pos {
                frame.set_cursor_position(Position::new(x, y));
            }
        } else {
            let theme = shell.theme.clone();
            let mut ctx = DrawContext {
                theme: &theme,
                focused: false,
                cursor_pos: None,
                slot: PanelSlot::Side,
            };
            if let Some(comp) = shell.side_component_mut() {
                comp.draw(frame, browser_area, &mut ctx);
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

        let lead = if tab.dirty { "\u{25cf}" } else { " " };
        let filename = &tab.label;
        let max_chars = 15;
        let char_count = filename.chars().count();
        let truncated = char_count + 1 > max_chars; // +1 for lead char
        let take = if truncated {
            max_chars - 2 // lead + ellipsis
        } else {
            char_count
        };
        let label: String = lead
            .chars()
            .chain(filename.chars().take(take))
            .chain(if truncated { Some('…') } else { None })
            .chain(" ".chars())
            .collect();
        let tab_width = label.chars().count() as u16;

        if x + tab_width > max_x {
            break;
        }

        let style = if tab.preview {
            if tab_idx == active {
                theme.get("tabs.preview_active").to_style()
            } else {
                theme.get("tabs.preview_inactive").to_style()
            }
        } else if tab_idx == active {
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

    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    let text_area = chunks[0];
    let tab_area = chunks[1];

    shell.set_tab_bar_width(tab_area.width);
    render_tab_bar(shell, frame, tab_area);

    // Debug flash at right of tab bar
    if let Some(text) = shell.debug_flash_text() {
        let flash_width = text.len() as u16;
        if flash_width < tab_area.width {
            let x = tab_area.x + tab_area.width - flash_width;
            let style = shell.theme.get("editor.gutter").to_style();
            frame.buffer_mut().set_string(x, tab_area.y, text, style);
        }
    }
    shell.set_viewport_height(text_area.height as usize);

    // Draw the active buffer component
    let focused = shell.focus == PanelSlot::Main;
    let theme = shell.theme.clone();
    let mut ctx = DrawContext {
        theme: &theme,
        focused,
        cursor_pos: None,
        slot: PanelSlot::Main,
    };
    shell
        .active_buffer_mut()
        .unwrap()
        .draw(frame, text_area, &mut ctx);

    // Place cursor (only when main panel focused)
    if focused {
        if let Some((x, y)) = ctx.cursor_pos {
            frame.set_cursor_position(Position::new(x, y));
        }
    }
}

fn render_status_bar(shell: &mut Shell, frame: &mut Frame, area: Rect) {
    // Check if any component claims the status bar
    let theme = shell.theme.clone();
    if let Some(comp) = shell.status_bar_component_mut() {
        let mut ctx = DrawContext {
            theme: &theme,
            focused: true,
            cursor_pos: None,
            slot: PanelSlot::StatusBar,
        };
        comp.draw(frame, area, &mut ctx);
        if let Some((x, y)) = ctx.cursor_pos {
            frame.set_cursor_position(Position::new(x, y));
        }
        return;
    }

    // Fallback: no component claims the status bar
    let left = " led";
    let padding = (area.width as usize).saturating_sub(left.len());
    let bar = format!("{left}{:padding$}", "");
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
