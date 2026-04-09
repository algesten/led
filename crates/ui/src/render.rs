use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::display::{LayoutInfo, OverlayContent, TabsInputs};

pub fn render(
    frame: &mut Frame,
    layout: &LayoutInfo,
    lines: &[Line],
    cursor: Option<(u16, u16)>,
    status: &crate::display::StatusContent,
    tabs: &TabsInputs,
    browser: &[Line],
    overlay: &OverlayContent,
) {
    let dims = layout.dims;
    let area = frame.area();

    // Vertical split: main area + status bar
    let [main_area, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(dims.status_bar_height),
    ])
    .areas(area);

    render_status_bar(status, layout, frame, status_area);

    // Horizontal split: optional side panel + editor area
    if dims.side_panel_visible() {
        let [side_area, editor_area] =
            Layout::horizontal([Constraint::Length(dims.side_width()), Constraint::Min(1)])
                .areas(main_area);

        render_side_panel(browser, layout, frame, side_area);
        render_editor_area(lines, tabs, layout, frame, editor_area);
    } else {
        render_editor_area(lines, tabs, layout, frame, main_area);
    }

    // Overlay (completion, code actions, rename)
    render_overlay(overlay, frame, area);

    // Cursor: absolute terminal coordinates
    if let Some((cx, cy)) = cursor {
        frame.set_cursor_position(Position::new(cx, cy));
    }
}

fn render_overlay(overlay: &OverlayContent, frame: &mut Frame, area: Rect) {
    match overlay {
        OverlayContent::None => {}
        OverlayContent::Completion {
            items,
            anchor_x,
            anchor_y,
        } => {
            if items.is_empty() {
                return;
            }
            let max_label = items
                .iter()
                .map(|(l, _, _)| l.chars().count())
                .max()
                .unwrap_or(10);
            let max_detail = items
                .iter()
                .filter_map(|(_, d, _)| d.as_ref().map(|d| d.chars().count()))
                .max()
                .unwrap_or(0);
            let width = (max_label + if max_detail > 0 { max_detail + 2 } else { 0 } + 2)
                .min(area.width as usize);
            let height = items.len().min(10);

            let x =
                (*anchor_x as usize).min(area.width.saturating_sub(width as u16) as usize) as u16;
            let y = if *anchor_y as usize + height + 1 > area.height as usize {
                anchor_y.saturating_sub(height as u16 + 1)
            } else {
                *anchor_y
            };
            let rect = Rect::new(x, y, width as u16, height as u16);
            frame.render_widget(Clear, rect);

            let normal = Style::default().bg(Color::DarkGray).fg(Color::White);
            let selected = Style::default().bg(Color::Blue).fg(Color::White);

            let lines: Vec<Line> = items
                .iter()
                .map(|(label, detail, is_sel)| {
                    let sty = if *is_sel { selected } else { normal };
                    let mut text = format!(" {:<w$}", label, w = max_label);
                    if let Some(d) = detail {
                        text.push_str(&format!("  {}", d));
                    }
                    let padded: String = text.chars().take(width).collect();
                    let pad = width.saturating_sub(padded.chars().count());
                    Line::from(Span::styled(format!("{padded}{:pad$}", ""), sty))
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), rect);
        }
        OverlayContent::CodeActions {
            items,
            anchor_x,
            anchor_y,
        } => {
            if items.is_empty() {
                return;
            }
            let max_w = items
                .iter()
                .map(|(t, _)| t.chars().count())
                .max()
                .unwrap_or(10);
            let width = (max_w + 2).min(area.width as usize);
            let height = items.len().min(15);

            let x =
                (*anchor_x as usize).min(area.width.saturating_sub(width as u16) as usize) as u16;
            let y = if *anchor_y as usize + height + 1 > area.height as usize {
                anchor_y.saturating_sub(height as u16 + 1)
            } else {
                *anchor_y
            };
            let rect = Rect::new(x, y, width as u16, height as u16);
            frame.render_widget(Clear, rect);

            let normal = Style::default().bg(Color::DarkGray).fg(Color::White);
            let selected = Style::default().bg(Color::Blue).fg(Color::White);

            let lines: Vec<Line> = items
                .iter()
                .map(|(title, is_sel)| {
                    let sty = if *is_sel { selected } else { normal };
                    let text: String = format!(" {}", title).chars().take(width).collect();
                    let pad = width.saturating_sub(text.chars().count());
                    Line::from(Span::styled(format!("{text}{:pad$}", ""), sty))
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), rect);
        }
        OverlayContent::Rename {
            input,
            cursor: _,
            anchor_x,
            anchor_y,
        } => {
            let label = "Rename: ";
            let width = (label.len() + input.chars().count() + 4).min(area.width as usize);
            let x =
                (*anchor_x as usize).min(area.width.saturating_sub(width as u16) as usize) as u16;
            let y = *anchor_y;
            let rect = Rect::new(x, y, width as u16, 1);
            frame.render_widget(Clear, rect);

            let sty = Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD);
            let text = format!(" {}{} ", label, input);
            let padded: String = text.chars().take(width).collect();
            let pad = width.saturating_sub(padded.chars().count());
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("{padded}{:pad$}", ""),
                    sty,
                ))),
                rect,
            );
        }
        OverlayContent::Diagnostic {
            messages,
            anchor_x,
            anchor_y,
        } => {
            if messages.is_empty() {
                return;
            }

            let max_content = 58usize.min(area.width.saturating_sub(4) as usize);

            // Build (text, fg_color) for each rendered line.
            let mut raw: Vec<(String, Color)> = Vec::new();
            for (i, (sev, msg)) in messages.iter().enumerate() {
                if i > 0 {
                    raw.push(("\u{2500}".repeat(max_content), Color::Gray));
                }
                let fg = match sev {
                    led_lsp::DiagnosticSeverity::Error => Color::Red,
                    led_lsp::DiagnosticSeverity::Warning => Color::Yellow,
                    led_lsp::DiagnosticSeverity::Info => Color::Cyan,
                    led_lsp::DiagnosticSeverity::Hint => Color::White,
                };
                for line in word_wrap(msg, max_content) {
                    raw.push((line, fg));
                }
            }

            let content_w = raw.iter().map(|(t, _)| t.chars().count()).max().unwrap_or(1);
            let width = (content_w + 2).min(area.width as usize);
            let height = raw.len().min(area.height as usize / 2).max(1);
            let raw = &raw[..height];

            // X: clamp so the box stays on screen.
            let x =
                (*anchor_x as usize).min(area.width.saturating_sub(width as u16) as usize) as u16;

            // Y: prefer above the anchor line; fall back to below.
            let y = if (*anchor_y as usize) >= height {
                anchor_y - height as u16
            } else {
                (*anchor_y + 1).min(area.height.saturating_sub(height as u16))
            };

            let rect = Rect::new(x, y, width as u16, height as u16);
            frame.render_widget(Clear, rect);

            let lines: Vec<Line> = raw
                .iter()
                .map(|(text, fg)| {
                    let inner: String = text.chars().take(width.saturating_sub(2)).collect();
                    let pad = width.saturating_sub(2).saturating_sub(inner.chars().count());
                    Line::from(Span::styled(
                        format!(" {inner}{:pad$} ", ""),
                        Style::default().bg(Color::DarkGray).fg(*fg),
                    ))
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), rect);
        }
    }
}

fn word_wrap(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for raw_line in text.lines() {
        if raw_line.chars().count() <= max_width {
            lines.push(raw_line.to_string());
        } else {
            let mut current = String::new();
            let mut current_w = 0;
            for word in raw_line.split_whitespace() {
                let ww = word.chars().count();
                if current.is_empty() {
                    current = word.to_string();
                    current_w = ww;
                } else if current_w + 1 + ww <= max_width {
                    current.push(' ');
                    current.push_str(word);
                    current_w += 1 + ww;
                } else {
                    lines.push(current);
                    current = word.to_string();
                    current_w = ww;
                }
            }
            if !current.is_empty() {
                lines.push(current);
            }
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn render_status_bar(
    status: &crate::display::StatusContent,
    layout: &LayoutInfo,
    frame: &mut Frame,
    area: Rect,
) {
    let style = if status.is_warn {
        Style::default().bg(Color::Red).fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        layout.status_style
    };
    frame.render_widget(
        Paragraph::new(status.text.as_str().to_owned()).style(style),
        area,
    );
}

fn render_side_panel(browser: &[Line], layout: &LayoutInfo, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(layout.side_border_style)
        .style(layout.side_bg_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if !browser.is_empty() {
        let paragraph = Paragraph::new(browser.to_vec()).style(layout.side_bg_style);
        frame.render_widget(paragraph, inner);
    }
}

fn render_editor_area(
    lines: &[Line],
    tabs: &TabsInputs,
    layout: &LayoutInfo,
    frame: &mut Frame,
    area: Rect,
) {
    let dims = layout.dims;

    // Vertical split: buffer content + tab bar
    let [buffer_area, tab_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(dims.tab_bar_height)]).areas(area);

    render_tab_bar(tabs, layout, frame, tab_area);

    if !lines.is_empty() {
        render_buffer(lines, layout, frame, buffer_area);
    } else {
        frame.render_widget(Block::default().style(layout.text_style), buffer_area);
    }
}

fn render_tab_bar(tabs: &TabsInputs, layout: &LayoutInfo, frame: &mut Frame, area: Rect) {
    let mut x = area.x + layout.dims.gutter_width - 1;
    let mut first = true;

    for entry in &tabs.entries {
        if !first {
            x += 1;
        }
        first = false;

        let width = entry.label.chars().count() as u16;

        if x + width > area.x + area.width {
            break;
        }

        let tab_area = Rect::new(x, area.y, width, 1);
        frame.render_widget(
            Paragraph::new(entry.label.clone()).style(entry.style),
            tab_area,
        );

        x += width;
    }
}

fn render_buffer(lines: &[Line], layout: &LayoutInfo, frame: &mut Frame, area: Rect) {
    let paragraph = Paragraph::new(lines.to_vec()).style(layout.text_style);
    frame.render_widget(paragraph, area);
}
