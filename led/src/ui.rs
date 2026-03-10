use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::shell::{Modal, RenameModal, Shell};
use led_core::{DrawContext, PanelSlot, TabDescriptor, Theme};

const GUTTER_WIDTH: u16 = 2;
const SIDE_PANEL_WIDTH: u16 = 25;
const MAX_TAB_CHARS: usize = 15;

// ---------------------------------------------------------------------------
// Pure display helpers
// ---------------------------------------------------------------------------

fn format_tab_label(tab: &TabDescriptor) -> String {
    let lead = if tab.dirty {
        "\u{25cf}"
    } else if tab.read_only {
        "#"
    } else {
        " "
    };
    let filename = &tab.label;
    let char_count = filename.chars().count();
    let truncated = char_count + 1 > MAX_TAB_CHARS; // +1 for lead char
    let take = if truncated {
        MAX_TAB_CHARS - 2 // lead + ellipsis
    } else {
        char_count
    };
    lead.chars()
        .chain(filename.chars().take(take))
        .chain(if truncated { Some('\u{2026}') } else { None })
        .chain(" ".chars())
        .collect()
}

fn tab_style(tab: &TabDescriptor, is_active: bool, theme: &Theme) -> Style {
    if tab.preview {
        if is_active {
            theme.get("tabs.preview_active").to_style()
        } else {
            theme.get("tabs.preview_inactive").to_style()
        }
    } else if is_active {
        theme.get("tabs.active").to_style()
    } else {
        theme.get("tabs.inactive").to_style()
    }
}

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
        let side_idx = shell.side_component_idx();
        let focused = shell.focus == PanelSlot::Side;
        let theme = shell.theme.clone();
        let fs = shell.file_statuses.clone();
        let lsp = shell.lsp_status.clone();
        let mut ctx = DrawContext {
            theme: &theme,
            focused,
            cursor_pos: None,
            slot: PanelSlot::Side,
            file_statuses: &fs,
            lsp_status: lsp.as_ref(),
            docs: &shell.docs,
        };
        if let Some(idx) = side_idx {
            shell.components[idx].draw(frame, browser_area, &mut ctx);
        }
        if focused {
            if let Some((x, y)) = ctx.cursor_pos {
                frame.set_cursor_position(Position::new(x, y));
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
    if let Some(modal) = &shell.rename_modal {
        render_rename_modal(modal, frame, area);
    }
    if let Some(idx) = shell.overlay_component_idx() {
        let focused = shell.focus == PanelSlot::Overlay;
        let theme = shell.theme.clone();
        let fs = shell.file_statuses.clone();
        let lsp = shell.lsp_status.clone();
        let mut ctx = DrawContext {
            theme: &theme,
            focused,
            cursor_pos: None,
            slot: PanelSlot::Overlay,
            file_statuses: &fs,
            lsp_status: lsp.as_ref(),
            docs: &shell.docs,
        };
        shell.components[idx].draw(frame, area, &mut ctx);
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

        let label = format_tab_label(&tab);
        let tab_width = label.chars().count() as u16;

        if x + tab_width > max_x {
            break;
        }

        let style = tab_style(&tab, tab_idx == active, theme);

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
    let active_idx = shell.active_tab_component_idx();
    let focused = shell.focus == PanelSlot::Main;
    let theme = shell.theme.clone();
    let fs = shell.file_statuses.clone();
    let lsp = shell.lsp_status.clone();
    let mut ctx = DrawContext {
        theme: &theme,
        focused,
        cursor_pos: None,
        slot: PanelSlot::Main,
        file_statuses: &fs,
        lsp_status: lsp.as_ref(),
        docs: &shell.docs,
    };
    if let Some(idx) = active_idx {
        shell.components[idx].draw(frame, text_area, &mut ctx);
    }

    // Place cursor (only when main panel focused)
    if focused {
        if let Some((x, y)) = ctx.cursor_pos {
            frame.set_cursor_position(Position::new(x, y));
        }
    }
}

fn render_status_bar(shell: &mut Shell, frame: &mut Frame, area: Rect) {
    // Shell-level message takes priority
    if let Some(msg) = shell.message_text() {
        let left = format!(" {msg}");
        let padding = (area.width as usize).saturating_sub(left.len());
        let bar = format!("{left}{:padding$}", "");
        let style = shell.theme.get("status_bar.style").to_style();
        let paragraph = Paragraph::new(bar).style(style);
        frame.render_widget(paragraph, area);
        return;
    }

    // Check if any component claims the status bar
    let sb_idx = shell.status_bar_component_idx();
    let theme = shell.theme.clone();
    let fs = shell.file_statuses.clone();
    let lsp = shell.lsp_status.clone();
    if let Some(idx) = sb_idx {
        let mut ctx = DrawContext {
            theme: &theme,
            focused: true,
            cursor_pos: None,
            slot: PanelSlot::StatusBar,
            file_statuses: &fs,
            lsp_status: lsp.as_ref(),
            docs: &shell.docs,
        };
        shell.components[idx].draw(frame, area, &mut ctx);
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

fn render_rename_modal(modal: &RenameModal, frame: &mut Frame, area: Rect) {
    use ratatui::widgets::Clear;

    let width = (modal.prompt.len() as u16 + 4).max(30).min(area.width);
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
