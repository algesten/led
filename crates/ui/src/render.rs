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
    status: &str,
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
            let width = (label.len() + input.len() + 4).min(area.width as usize);
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
    }
}

fn render_status_bar(status: &str, layout: &LayoutInfo, frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(status.to_owned()).style(layout.status_style),
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
