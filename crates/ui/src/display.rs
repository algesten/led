use std::rc::Rc;
use std::sync::Arc;

use led_core::PanelSlot;
use led_core::wrap::{chars_to_string, compute_chunks, expand_tabs, find_sub_line};
use led_core::{BufferId, Doc};
use led_state::{AppState, Dimensions, EntryKind};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::style;

// ── Display lines ──

#[derive(Clone)]
pub struct DisplayInputs {
    buffer_id: BufferId,
    doc: Arc<dyn Doc>,
    scroll_row: usize,
    scroll_sub_line: usize,
    text_width: usize,
    buffer_height: usize,
    gutter_style: Style,
    text_style: Style,
}

impl PartialEq for DisplayInputs {
    fn eq(&self, other: &Self) -> bool {
        self.buffer_id == other.buffer_id
            && self.doc.version() == other.doc.version()
            && self.scroll_row == other.scroll_row
            && self.scroll_sub_line == other.scroll_sub_line
            && self.text_width == other.text_width
            && self.buffer_height == other.buffer_height
            && self.gutter_style == other.gutter_style
            && self.text_style == other.text_style
    }
}

pub fn display_inputs(s: &AppState) -> Option<DisplayInputs> {
    let dims = s.dims?;
    let theme = s.config_theme.as_ref()?;
    let id = s.active_buffer?;
    let buf = s.buffers.get(&id)?;
    let theme = theme.file.as_ref();
    Some(DisplayInputs {
        buffer_id: id,
        doc: buf.doc.clone(),
        scroll_row: buf.scroll_row,
        scroll_sub_line: buf.scroll_sub_line,
        text_width: dims.text_width(),
        buffer_height: dims.buffer_height(),
        gutter_style: style::resolve(theme, &theme.editor.gutter),
        text_style: style::resolve(theme, &theme.editor.text),
    })
}

pub fn build_display_lines(d: &DisplayInputs) -> Rc<Vec<Line<'static>>> {
    let mut display_lines: Vec<Line<'static>> = Vec::with_capacity(d.buffer_height);
    let line_count = d.doc.line_count();
    let mut screen_row: usize = 0;
    let mut line_idx = d.scroll_row;
    let mut skip_sub_lines = d.scroll_sub_line;

    while screen_row < d.buffer_height && line_idx < line_count {
        let line = d.doc.line(line_idx);
        let (display, _char_map) = expand_tabs(&line);
        let chunks = compute_chunks(display.len(), d.text_width);

        for (chunk_idx, &(cs, ce)) in chunks.iter().enumerate() {
            if skip_sub_lines > 0 {
                skip_sub_lines -= 1;
                continue;
            }
            if screen_row >= d.buffer_height {
                break;
            }

            let is_last = chunk_idx == chunks.len() - 1;
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(3);

            // Gutter: 2 spaces
            spans.push(Span::styled("  ", d.gutter_style));

            // Text content
            let chunk_text = chars_to_string(&display[cs..ce]);
            spans.push(Span::styled(chunk_text, d.text_style));

            // Wrap indicator on non-last chunks
            if !is_last {
                spans.push(Span::styled("\\", d.gutter_style));
            }

            display_lines.push(Line::from(spans));
            screen_row += 1;
        }

        line_idx += 1;
        skip_sub_lines = 0;
    }

    // Past-EOF rows
    while screen_row < d.buffer_height {
        display_lines.push(Line::from(vec![Span::styled("~ ", d.gutter_style)]));
        screen_row += 1;
    }

    Rc::new(display_lines)
}

// ── Cursor position ──

#[derive(Clone)]
pub struct CursorInputs {
    buffer_id: BufferId,
    doc: Arc<dyn Doc>,
    cursor_row: usize,
    cursor_col: usize,
    scroll_row: usize,
    scroll_sub_line: usize,
    text_width: usize,
    gutter_width: u16,
}

impl PartialEq for CursorInputs {
    fn eq(&self, other: &Self) -> bool {
        self.buffer_id == other.buffer_id
            && self.doc.version() == other.doc.version()
            && self.cursor_row == other.cursor_row
            && self.cursor_col == other.cursor_col
            && self.scroll_row == other.scroll_row
            && self.scroll_sub_line == other.scroll_sub_line
            && self.text_width == other.text_width
            && self.gutter_width == other.gutter_width
    }
}

pub fn cursor_inputs(s: &AppState) -> Option<CursorInputs> {
    let dims = s.dims?;
    let id = s.active_buffer?;
    let buf = s.buffers.get(&id)?;
    Some(CursorInputs {
        buffer_id: id,
        doc: buf.doc.clone(),
        cursor_row: buf.cursor_row,
        cursor_col: buf.cursor_col,
        scroll_row: buf.scroll_row,
        scroll_sub_line: buf.scroll_sub_line,
        text_width: dims.text_width(),
        gutter_width: dims.gutter_width,
    })
}

/// Returns cursor position relative to buffer area: (x_offset, y_offset).
pub fn compute_cursor_pos(c: &CursorInputs) -> Option<(u16, u16)> {
    let line = c.doc.line(c.cursor_row);
    let (display, char_map) = expand_tabs(&line);
    let cursor_dcol = char_map
        .get(c.cursor_col)
        .copied()
        .unwrap_or_else(|| char_map.last().copied().unwrap_or(0));
    let chunks = compute_chunks(display.len(), c.text_width);
    let cursor_sub = find_sub_line(&chunks, cursor_dcol);
    let (cs, _ce) = chunks[cursor_sub];

    // Compute visual row from scroll position
    let mut vrow: usize = 0;
    let line_count = c.doc.line_count();
    let mut line_idx = c.scroll_row;
    let mut skip_sub_lines = c.scroll_sub_line;

    while line_idx < line_count {
        let l = c.doc.line(line_idx);
        let (disp, _) = expand_tabs(&l);
        let ch = compute_chunks(disp.len(), c.text_width);

        for (chunk_idx, _) in ch.iter().enumerate() {
            if skip_sub_lines > 0 {
                skip_sub_lines -= 1;
                continue;
            }
            if line_idx == c.cursor_row && chunk_idx == cursor_sub {
                let cx = c.gutter_width + (cursor_dcol - cs) as u16;
                return Some((cx, vrow as u16));
            }
            vrow += 1;
        }

        line_idx += 1;
        skip_sub_lines = 0;
    }

    None
}

// ── Status bar ──

#[derive(Clone, PartialEq)]
pub struct StatusInputs {
    pub file_name: String,
    pub is_dirty: bool,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub info: Option<String>,
    pub warn: Option<String>,
    pub viewport_width: u16,
}

pub fn status_inputs(s: &AppState) -> StatusInputs {
    let (file_name, is_dirty, cursor_row, cursor_col) = s
        .active_buffer
        .and_then(|id| s.buffers.get(&id))
        .map(|buf| {
            let fname = buf
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            (fname, buf.doc.dirty(), buf.cursor_row, buf.cursor_col)
        })
        .unwrap_or_default();

    let file_name = if file_name.is_empty() {
        "led".to_string()
    } else {
        file_name
    };

    let viewport_width = s.dims.map_or(0, |d| d.viewport_width);

    StatusInputs {
        file_name,
        is_dirty,
        cursor_row,
        cursor_col,
        info: s.info.clone(),
        warn: s.warn.clone(),
        viewport_width,
    }
}

pub fn build_status_content(s: &StatusInputs) -> Rc<String> {
    let modified = if s.is_dirty { " \u{25cf}" } else { "" };
    let default_left = format!(" {}{}", s.file_name, modified);
    let right = format!("L{}:C{} ", s.cursor_row + 1, s.cursor_col + 1);

    let left = match s.warn.as_deref().or(s.info.as_deref()) {
        Some(m) => format!(" {}", m),
        None => default_left,
    };

    let total = s.viewport_width as usize;
    let padding = total.saturating_sub(left.len() + right.len());
    Rc::new(format!(
        "{}{:padding$}{}",
        left,
        "",
        right,
        padding = padding
    ))
}

// ── Tab bar ──

#[derive(Clone, PartialEq)]
pub struct TabEntry {
    pub label: String,
    pub is_active: bool,
    pub style: Style,
}

#[derive(Clone, PartialEq)]
pub struct TabsInputs {
    pub entries: Vec<TabEntry>,
    pub inactive_style: Style,
    pub gutter_width: u16,
}

pub fn tabs_inputs(s: &AppState) -> Option<TabsInputs> {
    let theme = s.config_theme.as_ref()?;
    let dims = s.dims?;
    let theme = theme.file.as_ref();
    let active_style = style::resolve(theme, &theme.tabs.active);
    let inactive_style = style::resolve(theme, &theme.tabs.inactive);

    let mut bufs: Vec<_> = s.buffers.values().collect();
    bufs.sort_by_key(|b| b.tab_order);

    let entries = bufs
        .iter()
        .map(|buf| {
            let name = buf
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("[{}]", buf.id.0));
            let dirty = buf.doc.dirty();
            let label = format_tab_label(&name, dirty);
            let is_active = s.active_buffer == Some(buf.id);
            let entry_style = if is_active {
                active_style
            } else {
                inactive_style
            };
            TabEntry {
                label,
                is_active,
                style: entry_style,
            }
        })
        .collect();

    Some(TabsInputs {
        entries,
        inactive_style,
        gutter_width: dims.gutter_width,
    })
}

const MAX_TAB_CHARS: usize = 15;

fn format_tab_label(name: &str, dirty: bool) -> String {
    let lead = if dirty { "\u{25cf}" } else { " " };
    let char_count = name.chars().count();
    let truncated = char_count + 1 > MAX_TAB_CHARS;
    let take = if truncated {
        MAX_TAB_CHARS - 2
    } else {
        char_count
    };
    lead.chars()
        .chain(name.chars().take(take))
        .chain(if truncated { Some('\u{2026}') } else { None })
        .chain(" ".chars())
        .collect()
}

pub fn build_tab_entries(t: &TabsInputs) -> Rc<TabsInputs> {
    // Tabs are already built in the inputs — just wrap in Rc for cheap cloning
    Rc::new(t.clone())
}

// ── Layout ──

#[derive(Clone, Copy, PartialEq)]
pub struct LayoutInputs {
    pub dims: Option<Dimensions>,
    pub has_theme: bool,
    pub force_redraw: u64,
    pub side_border_style: Style,
    pub side_bg_style: Style,
    pub text_style: Style,
    pub status_style: Style,
}

pub fn layout_inputs(s: &AppState) -> LayoutInputs {
    let (side_border_style, side_bg_style, text_style, status_style) = s
        .config_theme
        .as_ref()
        .map(|ct| {
            let t = ct.file.as_ref();
            (
                style::resolve(t, &t.browser.border),
                style::resolve(t, &t.browser.file),
                style::resolve(t, &t.editor.text),
                style::resolve(t, &t.status_bar.style),
            )
        })
        .unwrap_or_default();

    LayoutInputs {
        dims: s.dims,
        has_theme: s.config_theme.is_some(),
        force_redraw: s.force_redraw,
        side_border_style,
        side_bg_style,
        text_style,
        status_style,
    }
}

#[derive(Clone, Copy)]
pub struct LayoutInfo {
    pub dims: Dimensions,
    pub force_redraw: u64,
    pub side_border_style: Style,
    pub side_bg_style: Style,
    pub text_style: Style,
    pub status_style: Style,
}

pub fn build_layout(l: &LayoutInputs) -> Option<LayoutInfo> {
    let dims = l.dims?;
    if !l.has_theme {
        return None;
    }
    Some(LayoutInfo {
        dims,
        force_redraw: l.force_redraw,
        side_border_style: l.side_border_style,
        side_bg_style: l.side_bg_style,
        text_style: l.text_style,
        status_style: l.status_style,
    })
}

// ── Browser ──

#[derive(Clone, PartialEq)]
pub struct BrowserInputs {
    pub entries: Vec<led_state::TreeEntry>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub focused: bool,
    pub height: usize,
    pub dir_style: Style,
    pub file_style: Style,
    pub selected_style: Style,
    pub selected_unfocused_style: Style,
}

pub fn browser_inputs(s: &AppState) -> Option<BrowserInputs> {
    let dims = s.dims?;
    let theme = s.config_theme.as_ref()?.file.as_ref();
    Some(BrowserInputs {
        entries: s.browser.entries.clone(),
        selected: s.browser.selected,
        scroll_offset: s.browser.scroll_offset,
        focused: s.focus == PanelSlot::Side,
        height: dims.buffer_height(),
        dir_style: style::resolve(theme, &theme.browser.directory),
        file_style: style::resolve(theme, &theme.browser.file),
        selected_style: style::resolve(theme, &theme.browser.selected),
        selected_unfocused_style: style::resolve(theme, &theme.browser.selected_unfocused),
    })
}

pub fn build_browser_lines(b: &BrowserInputs) -> Rc<Vec<Line<'static>>> {
    let end = (b.scroll_offset + b.height).min(b.entries.len());
    let visible = &b.entries[b.scroll_offset..end];

    let lines: Vec<Line<'static>> = visible
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let abs_idx = b.scroll_offset + i;
            let is_selected = abs_idx == b.selected;

            let indent = "  ".repeat(entry.depth);
            let icon = match &entry.kind {
                EntryKind::Directory { expanded: true } => "\u{25bd} ",
                EntryKind::Directory { expanded: false } => "\u{25b7} ",
                EntryKind::File => "  ",
            };
            let text = format!("{}{}{}", indent, icon, entry.name);

            let entry_style = if is_selected {
                if b.focused {
                    b.selected_style
                } else {
                    b.selected_unfocused_style
                }
            } else {
                match &entry.kind {
                    EntryKind::Directory { .. } => b.dir_style,
                    EntryKind::File => b.file_style,
                }
            };

            Line::from(Span::styled(text, entry_style))
        })
        .collect();

    Rc::new(lines)
}
