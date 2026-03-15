use led_core::theme::Theme;
use led_core::wrap::{chars_to_string, compute_chunks, expand_tabs, find_sub_line};
use led_state::{AppState, BufferState, Dimensions};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::style;

pub fn render(state: &AppState, frame: &mut Frame) {
    let theme = match state.config_theme.as_ref() {
        Some(ct) => ct.file.as_ref(),
        None => return,
    };

    let dims = match state.dims {
        Some(d) => d,
        None => return,
    };

    let area = frame.area();

    // Vertical split: main area + status bar
    let [main_area, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(dims.status_bar_height),
    ])
    .areas(area);

    render_status_bar(state, theme, frame, status_area);

    // Horizontal split: optional side panel + editor area
    if dims.side_panel_visible() {
        let [side_area, editor_area] =
            Layout::horizontal([Constraint::Length(dims.side_width()), Constraint::Min(1)])
                .areas(main_area);

        render_side_panel(theme, frame, side_area);
        render_editor_area(state, &dims, theme, frame, editor_area);
    } else {
        render_editor_area(state, &dims, theme, frame, main_area);
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

fn render_editor_area(
    state: &AppState,
    dims: &Dimensions,
    theme: &Theme,
    frame: &mut Frame,
    area: Rect,
) {
    // Vertical split: buffer content + tab bar
    let [buffer_area, tab_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(dims.tab_bar_height)]).areas(area);

    render_tab_bar(state, theme, frame, tab_area);

    if let Some(buf) = active_buffer(state) {
        render_buffer(buf, dims, theme, frame, buffer_area);
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

        let dirty = buf.doc.dirty();
        let label = if dirty {
            format!(" {} \u{25CF} ", name)
        } else {
            format!(" {} ", name)
        };
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

fn render_buffer(
    buf: &BufferState,
    dims: &Dimensions,
    theme: &Theme,
    frame: &mut Frame,
    area: Rect,
) {
    let gutter_w = dims.gutter_width;
    let text_width = dims.text_width();

    let [gutter_area, text_area] =
        Layout::horizontal([Constraint::Length(gutter_w), Constraint::Min(1)]).areas(area);

    let gutter_style = style::resolve(theme, &theme.editor.gutter);
    let text_style = style::resolve(theme, &theme.editor.text);

    // Fill backgrounds
    frame.render_widget(Block::default().style(gutter_style), gutter_area);
    frame.render_widget(Block::default().style(text_style), text_area);

    let visible_rows = area.height as usize;
    let line_count = buf.doc.line_count();
    let mut screen_row: usize = 0;
    let mut cursor_screen_pos: Option<(u16, u16)> = None;

    let mut line_idx = buf.scroll_row;
    let mut skip_sub_lines = buf.scroll_sub_line;

    while screen_row < visible_rows && line_idx < line_count {
        let line = buf.doc.line(line_idx);
        let (display, char_map) = expand_tabs(&line);
        let chunks = compute_chunks(display.len(), text_width);

        for (chunk_idx, &(cs, ce)) in chunks.iter().enumerate() {
            // Skip sub-lines on the first logical line (scroll_sub_line)
            if skip_sub_lines > 0 {
                skip_sub_lines -= 1;
                continue;
            }

            if screen_row >= visible_rows {
                break;
            }

            let y = area.y + screen_row as u16;

            // Gutter
            let gutter_content = if chunk_idx == 0 {
                let num = line_idx + 1;
                format!(
                    "{:>width$}",
                    num,
                    width = (gutter_w as usize).saturating_sub(1)
                )
            } else {
                // Continuation line: show wrap indicator
                let pad = (gutter_w as usize).saturating_sub(1);
                format!("{:>width$}", "\\", width = pad)
            };
            let gutter_line_area = Rect::new(gutter_area.x, y, gutter_w, 1);
            frame.render_widget(
                Paragraph::new(gutter_content).style(gutter_style),
                gutter_line_area,
            );

            // Text content for this chunk
            let chunk_text = chars_to_string(&display[cs..ce]);

            // Append wrap indicator for non-last chunks
            let content = if chunk_idx < chunks.len() - 1 {
                format!("{}{}", chunk_text, "\\")
            } else {
                chunk_text
            };

            let text_line_area = Rect::new(text_area.x, y, text_area.width, 1);
            frame.render_widget(Paragraph::new(content).style(text_style), text_line_area);

            // Track cursor position
            if line_idx == buf.cursor_row {
                let cursor_dcol = char_map
                    .get(buf.cursor_col)
                    .copied()
                    .unwrap_or_else(|| char_map.last().copied().unwrap_or(0));
                let cursor_sub = find_sub_line(&chunks, cursor_dcol);
                if cursor_sub == chunk_idx {
                    let cursor_x = text_area.x + (cursor_dcol - cs) as u16;
                    let cursor_y = y;
                    cursor_screen_pos = Some((cursor_x, cursor_y));
                }
            }

            screen_row += 1;
        }

        line_idx += 1;
        skip_sub_lines = 0;
    }

    // Fill remaining rows with ~ in gutter
    while screen_row < visible_rows {
        let y = area.y + screen_row as u16;
        let tilde = format!(
            "{:>width$}",
            "~",
            width = (gutter_w as usize).saturating_sub(1)
        );
        let gutter_line_area = Rect::new(gutter_area.x, y, gutter_w, 1);
        frame.render_widget(Paragraph::new(tilde).style(gutter_style), gutter_line_area);
        screen_row += 1;
    }

    // Set cursor
    if let Some((cx, cy)) = cursor_screen_pos {
        frame.set_cursor_position(Position::new(cx, cy));
    }
}
