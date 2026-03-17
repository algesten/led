use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::display::{LayoutInfo, TabsInputs};

pub fn render(
    frame: &mut Frame,
    layout: &LayoutInfo,
    lines: &[Line],
    cursor: Option<(u16, u16)>,
    status: &str,
    tabs: &TabsInputs,
    browser: &[Line],
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
        render_editor_area(lines, cursor, tabs, layout, frame, editor_area);
    } else {
        render_editor_area(lines, cursor, tabs, layout, frame, main_area);
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
    cursor: Option<(u16, u16)>,
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
        render_buffer(lines, cursor, layout, frame, buffer_area);
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

fn render_buffer(
    lines: &[Line],
    cursor: Option<(u16, u16)>,
    layout: &LayoutInfo,
    frame: &mut Frame,
    area: Rect,
) {
    let paragraph = Paragraph::new(lines.to_vec()).style(layout.text_style);
    frame.render_widget(paragraph, area);

    // Cursor: offset by buffer area position
    if let Some((cx, cy)) = cursor {
        frame.set_cursor_position(Position::new(area.x + cx, area.y + cy));
    }
}
