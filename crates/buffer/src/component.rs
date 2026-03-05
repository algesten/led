use std::path::PathBuf;

use led_core::{
    Action, Component, Context, DrawContext, Effect, ElementStyle, Event, PanelClaim, PanelSlot,
    TabDescriptor,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::color_hint::{evaluate_theme_line, parse_color_defs, scan_hex_color};
use crate::wrap::{
    chars_to_string, compute_chunks, expand_tabs, find_sub_line, visual_line_count,
};
use crate::{Buffer, UndoEntry};

impl Buffer {
    /// Adjust scroll so the cursor is visible within `height` visual rows.
    /// Scroll is tracked as (scroll_offset, scroll_sub_line) — a logical line
    /// plus a sub-line offset within it — so scrolling is visual-line granular.
    fn adjust_scroll(&mut self, text_width: usize, height: usize) {
        if height == 0 || text_width == 0 {
            return;
        }

        let total = self.line_count();

        // Clamp scroll_offset / scroll_sub_line to valid range
        if self.scroll_offset >= total {
            self.scroll_offset = total.saturating_sub(1);
            self.scroll_sub_line = 0;
        }
        let scroll_vl = visual_line_count(
            expand_tabs(&self.line(self.scroll_offset)).0.len(),
            text_width,
        );
        if self.scroll_sub_line >= scroll_vl {
            self.scroll_sub_line = scroll_vl.saturating_sub(1);
        }

        // Compute cursor's sub-line within its logical line
        let (cursor_display, cursor_cm) = expand_tabs(&self.line(self.cursor_row));
        let cursor_dc = cursor_cm
            .get(self.cursor_col)
            .copied()
            .unwrap_or_else(|| cursor_cm.last().copied().unwrap_or(0));
        let cursor_chunks = compute_chunks(cursor_display.len(), text_width);
        let cursor_sub = find_sub_line(&cursor_chunks, cursor_dc);

        // Case 1: cursor above viewport
        if self.cursor_row < self.scroll_offset
            || (self.cursor_row == self.scroll_offset && cursor_sub < self.scroll_sub_line)
        {
            self.scroll_offset = self.cursor_row;
            self.scroll_sub_line = cursor_sub;
            return;
        }

        // Case 2: check if cursor is visible
        let mut vrow: usize = 0;

        if self.cursor_row == self.scroll_offset {
            // Same line — just check sub-line distance
            let cursor_vrow = cursor_sub - self.scroll_sub_line;
            if cursor_vrow < height {
                return;
            }
        } else {
            // First logical line: only count sub-lines from scroll_sub_line onward
            vrow += scroll_vl - self.scroll_sub_line;

            // Intermediate lines
            let limit = self.cursor_row.min(self.scroll_offset + height);
            for li in (self.scroll_offset + 1)..limit {
                vrow += visual_line_count(
                    expand_tabs(&self.line(li)).0.len(),
                    text_width,
                );
                if vrow >= height {
                    break;
                }
            }

            if vrow + cursor_sub < height {
                return;
            }
        }

        // Case 3: cursor not visible — place cursor near bottom.
        // We need (height - 1) visual rows above cursor's sub-line.
        let mut remaining = height - 1;

        if cursor_sub <= remaining {
            remaining -= cursor_sub;
        } else {
            // Line itself is taller than viewport at cursor's sub-line
            self.scroll_offset = self.cursor_row;
            self.scroll_sub_line = cursor_sub.saturating_sub(height - 1);
            return;
        }

        let mut new_scroll = self.cursor_row;
        let mut new_sub: usize = 0;

        for li in (0..self.cursor_row).rev() {
            if remaining == 0 {
                break;
            }
            let vl = visual_line_count(
                expand_tabs(&self.line(li)).0.len(),
                text_width,
            );
            if vl <= remaining {
                remaining -= vl;
                new_scroll = li;
                new_sub = 0;
            } else {
                // Partially fits — start from a sub-line within this line
                new_scroll = li;
                new_sub = vl - remaining;
                break;
            }
        }

        self.scroll_offset = new_scroll;
        self.scroll_sub_line = new_sub;
    }
}

impl Component for Buffer {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }

    fn ensure_schema(&self, ctx: &Context) {
        let Some(conn) = ctx.db else { return };
        let _ = conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS buffer_undo_state (
                root_path        TEXT NOT NULL,
                file_path        TEXT NOT NULL,
                chain_id         TEXT NOT NULL,
                content_hash     INTEGER NOT NULL,
                undo_cursor      INTEGER,
                distance_from_save INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (root_path, file_path)
            );

            CREATE TABLE IF NOT EXISTS undo_entries (
                seq         INTEGER PRIMARY KEY AUTOINCREMENT,
                root_path   TEXT NOT NULL,
                file_path   TEXT NOT NULL,
                entry_data  BLOB NOT NULL,
                FOREIGN KEY (root_path, file_path)
                    REFERENCES buffer_undo_state(root_path, file_path) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_undo_entries_file
            ON undo_entries(root_path, file_path, seq);",
        );
    }

    fn panel_claims(&self) -> &[PanelClaim] {
        &[PanelClaim {
            slot: PanelSlot::Main,
            priority: 10,
        }]
    }

    fn tab(&self) -> Option<TabDescriptor> {
        Some(TabDescriptor {
            label: self.filename().to_string(),
            dirty: self.dirty,
            path: self.path.clone(),
            preview: self.preview,
        })
    }

    fn handle_action(&mut self, action: Action, ctx: &mut Context) -> Vec<Effect> {
        // Clear kill accumulator for non-KillLine actions
        if !matches!(action, Action::KillLine) {
            self.kill_accumulator = None;
        }

        match action {
            Action::InsertChar(c) => {
                self.clear_mark();
                self.insert_char(c);
                vec![]
            }
            Action::MoveUp => {
                self.move_up();
                vec![]
            }
            Action::MoveDown => {
                self.move_down();
                vec![]
            }
            Action::MoveLeft => {
                self.move_left();
                vec![]
            }
            Action::MoveRight => {
                self.move_right();
                vec![]
            }
            Action::LineStart => {
                self.move_to_line_start();
                vec![]
            }
            Action::LineEnd => {
                self.move_to_line_end();
                vec![]
            }
            Action::PageUp => {
                self.page_up(ctx.viewport_height);
                vec![]
            }
            Action::PageDown => {
                self.page_down(ctx.viewport_height);
                vec![]
            }
            Action::FileStart => {
                self.move_to_file_start();
                vec![]
            }
            Action::FileEnd => {
                self.move_to_file_end();
                vec![]
            }
            Action::InsertNewline => {
                self.clear_mark();
                self.insert_newline();
                vec![]
            }
            Action::DeleteBackward => {
                self.clear_mark();
                self.delete_char_backward();
                vec![]
            }
            Action::DeleteForward => {
                self.clear_mark();
                self.delete_char_forward();
                vec![]
            }
            Action::InsertTab => {
                self.clear_mark();
                self.insert_char('\t');
                vec![]
            }
            Action::KillLine => {
                if let Some(killed) = self.kill_line() {
                    let acc = self.kill_accumulator.get_or_insert_with(String::new);
                    acc.push_str(&killed);
                    ctx.clipboard.set_text(&acc);
                    vec![]
                } else {
                    vec![]
                }
            }
            Action::Undo => {
                self.undo();
                vec![]
            }
            Action::Save => {
                if self.disk_modified {
                    vec![Effect::ConfirmAction {
                        prompt: format!(
                            "{} changed on disk; save anyway? (yes/no)",
                            self.filename()
                        ),
                        action: Action::SaveForce,
                    }]
                } else {
                    match self.save(ctx) {
                        Ok(()) => {
                            let name = self.filename().to_string();
                            vec![Effect::SetMessage(format!("Saved {name}."))]
                        }
                        Err(e) => vec![Effect::SetMessage(format!("Save failed: {e}"))],
                    }
                }
            }
            Action::SaveForce => match self.save(ctx) {
                Ok(()) => {
                    let name = self.filename().to_string();
                    vec![Effect::SetMessage(format!("Saved {name}."))]
                }
                Err(e) => vec![Effect::SetMessage(format!("Save failed: {e}"))],
            },
            Action::Tick => self.handle_tick(ctx),
            Action::SetMark => {
                self.set_mark();
                vec![Effect::SetMessage("Mark set".into())]
            }
            Action::KillRegion => {
                if let Some(text) = self.kill_region() {
                    ctx.clipboard.set_text(&text);
                    vec![]
                } else {
                    vec![Effect::SetMessage("No region".into())]
                }
            }
            Action::Yank => {
                if let Some(text) = ctx.clipboard.get_text() {
                    self.clear_mark();
                    self.yank_text(&text);
                }
                vec![]
            }
            Action::OpenFileSearch => {
                let selected_text = self.selected_text();
                self.clear_mark();
                vec![Effect::Emit(Event::FileSearchOpened { selected_text })]
            }
            Action::Abort => {
                self.clear_mark();
                vec![]
            }
            _ => vec![],
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> Vec<Effect> {
        match event {
            Event::Resume => {
                self.handle_notification(ctx);
            }
            Event::GoToPosition { path, row, col } => {
                if self.path.as_deref() == Some(path.as_path()) {
                    self.cursor_row = (*row).min(self.line_count().saturating_sub(1));
                    self.cursor_col = (*col).min(self.line_len(self.cursor_row));
                    self.clear_mark();
                }
            }
            Event::PreviewFile { path, row, col, match_len } => {
                if self.path.as_deref() == Some(path.as_path()) {
                    let r = (*row).min(self.line_count().saturating_sub(1));
                    self.cursor_row = r;
                    self.cursor_col = (*col).min(self.line_len(r));
                    self.scroll_offset = r.saturating_sub(ctx.viewport_height / 2);
                    self.highlight_match(*row, *col, *match_len);
                    return vec![Effect::ActivateBuffer(path.clone())];
                }
            }
            Event::ConfirmSearch { path, row, col } => {
                if self.path.as_deref() == Some(path.as_path()) {
                    self.preview = false;
                    let r = (*row).min(self.line_count().saturating_sub(1));
                    self.cursor_row = r;
                    self.cursor_col = (*col).min(self.line_len(r));
                    self.clear_mark();
                    return vec![
                        Effect::ActivateBuffer(path.clone()),
                        Effect::FocusPanel(PanelSlot::Main),
                    ];
                }
            }
            _ => {}
        }
        vec![]
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, ctx: &DrawContext) {
        let height = area.height as usize;
        let gutter_width: usize = 2;
        let text_width = (area.width as usize).saturating_sub(gutter_width);
        self.text_width = text_width;

        self.adjust_scroll(text_width, height);

        let total_lines = self.line_count();
        let gutter_style = ctx.theme.get("editor.gutter").to_style();
        let text_style = ctx.theme.get("editor.text").to_style();
        let sel_style = ctx.theme.get("editor.selection").to_style();
        let sel_range = self.selection_range();

        let is_theme = self.path.as_ref()
            .and_then(|p| p.file_name())
            .map_or(false, |n| n == "theme.toml");

        let color_defs = if is_theme {
            let all_lines: Vec<String> = (0..total_lines).map(|i| self.line(i)).collect();
            Some(parse_color_defs(all_lines.iter().map(|s| s.as_str())))
        } else {
            None
        };

        // Track current TOML section for theme files
        let mut current_section = String::new();

        let mut display_lines: Vec<Line> = Vec::with_capacity(height);
        let mut cursor_pos: Option<(u16, u16)> = None;
        let mut screen_row: usize = 0;
        let mut line_idx = self.scroll_offset;

        // Pre-scan to find the section header for the scroll_offset line
        if is_theme {
            for i in 0..self.scroll_offset.min(total_lines) {
                let l = self.line(i);
                let t = l.trim();
                if t.starts_with('[') && !t.starts_with("[[") {
                    if let Some(end) = t.find(']') {
                        current_section = t[1..end].to_string();
                    }
                }
            }
        }

        while screen_row < height && line_idx < total_lines {
            let raw = self.line(line_idx);
            let (display, char_map) = expand_tabs(&raw);
            let chunks = compute_chunks(display.len(), text_width);

            // Selection display-column range for this line
            let sel_dcols = match sel_range {
                Some(((sr, sc), (er, ec))) if line_idx >= sr && line_idx <= er => {
                    let sd = if line_idx == sr {
                        char_map.get(sc).copied().unwrap_or(display.len())
                    } else {
                        0
                    };
                    let ed = if line_idx == er {
                        char_map.get(ec).copied().unwrap_or(display.len())
                    } else {
                        display.len()
                    };
                    Some((sd, ed))
                }
                _ => None,
            };

            // Cursor display column
            let cursor_dcol = if line_idx == self.cursor_row {
                Some(
                    char_map
                        .get(self.cursor_col)
                        .copied()
                        .unwrap_or_else(|| char_map.last().copied().unwrap_or(0)),
                )
            } else {
                None
            };

            // Skip sub-lines for partial-line scroll on the first visible line
            let skip = if line_idx == self.scroll_offset { self.scroll_sub_line } else { 0 };

            // Color hint for gutter preview (first chunk only)
            let color_hint = if is_theme {
                // Track section headers
                let trimmed = raw.trim();
                if trimmed.starts_with('[') && !trimmed.starts_with("[[") {
                    if let Some(end) = trimmed.find(']') {
                        current_section = trimmed[1..end].to_string();
                    }
                }
                color_defs.as_ref().and_then(|defs| {
                    evaluate_theme_line(&raw, &current_section, defs)
                })
            } else {
                scan_hex_color(&raw).map(|c| ElementStyle {
                    fg: c,
                    bg: Color::Reset,
                    bold: false,
                    reversed: false,
                })
            };

            for (chunk_i, &(cs, ce)) in chunks.iter().enumerate() {
                if chunk_i < skip {
                    continue;
                }
                if screen_row >= height {
                    break;
                }
                let is_last = chunk_i == chunks.len() - 1;
                let chunk_text = &display[cs..ce];
                let mut spans: Vec<Span> = Vec::new();

                // Gutter — show color preview on first visible chunk
                if chunk_i == skip {
                    if let Some(ref hint) = color_hint {
                        if hint.bg != Color::Reset {
                            spans.push(Span::styled("A ", hint.to_style()));
                        } else {
                            let block_style = ratatui::style::Style::default().bg(hint.fg);
                            spans.push(Span::styled("  ", block_style));
                        }
                    } else {
                        spans.push(Span::styled("  ", gutter_style));
                    }
                } else {
                    spans.push(Span::styled("  ", gutter_style));
                }

                // Content with optional selection highlighting
                if let Some((ss, se)) = sel_dcols {
                    let rel_s = ss.clamp(cs, ce) - cs;
                    let rel_e = se.clamp(cs, ce) - cs;

                    if rel_e > rel_s {
                        if rel_s > 0 {
                            spans.push(Span::styled(
                                chars_to_string(&chunk_text[..rel_s]),
                                text_style,
                            ));
                        }
                        spans.push(Span::styled(
                            chars_to_string(&chunk_text[rel_s..rel_e]),
                            sel_style,
                        ));
                        if rel_e < chunk_text.len() {
                            spans.push(Span::styled(
                                chars_to_string(&chunk_text[rel_e..]),
                                text_style,
                            ));
                        }
                    } else if !chunk_text.is_empty() {
                        spans.push(Span::styled(chars_to_string(chunk_text), text_style));
                    }

                    // Pad selection to line edge on last chunk when selection continues
                    if is_last {
                        if let Some(((_, _), (er, _))) = sel_range {
                            if line_idx < er {
                                let content_len = ce - cs;
                                let pad = text_width.saturating_sub(content_len);
                                if pad > 0 {
                                    spans.push(Span::styled(" ".repeat(pad), sel_style));
                                }
                            }
                        }
                    }
                } else if !chunk_text.is_empty() {
                    spans.push(Span::styled(chars_to_string(chunk_text), text_style));
                }

                // Continuation indicator
                if !is_last {
                    spans.push(Span::styled("\\", gutter_style));
                }

                // Track cursor screen position
                if let Some(dc) = cursor_dcol {
                    if (dc >= cs && dc < ce) || (is_last && dc >= cs) {
                        let cx = gutter_width as u16 + (dc - cs) as u16;
                        cursor_pos = Some((area.x + cx, area.y + screen_row as u16));
                    }
                }

                display_lines.push(Line::from(spans));
                screen_row += 1;
            }

            line_idx += 1;
        }

        // Fill remaining rows with ~
        while screen_row < height {
            display_lines.push(Line::from(vec![Span::styled("~ ", gutter_style)]));
            screen_row += 1;
        }

        let paragraph = Paragraph::new(display_lines).style(text_style);
        frame.render_widget(paragraph, area);

        self.cursor_screen_pos = cursor_pos;
    }

    fn cursor_position(&self) -> Option<(usize, usize)> {
        Some((self.cursor_row, self.cursor_col))
    }

    fn cursor_screen_pos(&self) -> Option<(u16, u16)> {
        self.cursor_screen_pos
    }

    fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    fn set_scroll_offset(&mut self, offset: usize) {
        self.scroll_offset = offset;
    }

    fn status_info(&self) -> Option<(&str, usize, usize)> {
        Some((self.filename(), self.cursor_row + 1, self.cursor_col + 1))
    }

    fn save_session(&self, ctx: &mut Context) {
        let Some(conn) = ctx.db else { return };
        let Some(ref path) = self.path else { return };
        let root_str = ctx.root.to_string_lossy();
        let file_str = path.to_string_lossy();
        let _ = conn.execute(
            "UPDATE buffers SET cursor_row = ?1, cursor_col = ?2, scroll_offset = ?3
             WHERE root_path = ?4 AND file_path = ?5",
            rusqlite::params![
                self.cursor_row as i64,
                self.cursor_col as i64,
                self.scroll_offset as i64,
                root_str,
                file_str,
            ],
        );
    }

    fn restore_session(&mut self, ctx: &mut Context) {
        let Some(conn) = ctx.db else { return };
        let Some(ref path) = self.path else { return };
        let root_str = ctx.root.to_string_lossy();
        let file_str = path.to_string_lossy();

        // Load cursor/scroll from buffers table (independent of undo state)
        let cursor_data: Option<(i64, i64, i64)> = conn
            .query_row(
                "SELECT cursor_row, cursor_col, scroll_offset
                 FROM buffers WHERE root_path = ?1 AND file_path = ?2",
                rusqlite::params![root_str, file_str],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();

        // Restore undo state
        let undo_row: Option<(i64, Option<i64>, i32, String)> = conn
            .query_row(
                "SELECT content_hash, undo_cursor, distance_from_save, chain_id
                 FROM buffer_undo_state WHERE root_path = ?1 AND file_path = ?2",
                rusqlite::params![root_str, file_str],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .ok();

        if let Some((stored_hash, undo_cursor_raw, distance_from_save, chain_id)) = undo_row {
            if self.content_hash() == stored_hash as u64 {
                let loaded = Self::load_entries_after(conn, &root_str, &file_str, 0);
                if !loaded.is_empty() {
                    let max_seq = loaded.last().unwrap().0;
                    let entries: Vec<UndoEntry> = loaded.into_iter().map(|(_, e)| e).collect();

                    self.chain_id = Some(chain_id);
                    self.last_seen_seq = max_seq;

                    self.restore_undo(
                        entries,
                        undo_cursor_raw.map(|v| v as usize),
                        distance_from_save,
                    );
                }
            }
        }

        // Apply cursor/scroll after undo replay (line_count/line_len are now correct)
        if let Some((row, col, scroll)) = cursor_data {
            let row = row as usize;
            let col = col as usize;
            self.cursor_row = row.min(self.line_count().saturating_sub(1));
            self.cursor_col = col.min(self.line_len(self.cursor_row));
            self.scroll_offset = scroll as usize;
        }
    }

    fn needs_flush(&self) -> bool {
        self.has_unpersisted_undo()
    }

    fn flush(&mut self, ctx: &mut Context) {
        self.flush_undo_to_db(ctx);
    }

    fn notify_hash(&self) -> Option<String> {
        self.path.as_ref().map(|p| Self::notify_hash_for_path(p))
    }
}

// ---------------------------------------------------------------------------
// BufferFactory
// ---------------------------------------------------------------------------

pub struct BufferFactory {
    preview_path: Option<PathBuf>,
}

impl BufferFactory {
    pub fn new() -> Self {
        Self { preview_path: None }
    }
}

impl Component for BufferFactory {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }

    fn panel_claims(&self) -> &[PanelClaim] {
        &[]
    }

    fn handle_action(&mut self, _action: Action, _ctx: &mut Context) -> Vec<Effect> {
        vec![]
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> Vec<Effect> {
        let mut effects = Vec::new();
        match event {
            Event::OpenFile(path) => {
                let path_str = path.to_string_lossy();
                match Buffer::from_file_with_waker(&path_str, ctx.waker.clone()) {
                    Ok(buf) => effects.push(Effect::Spawn(Box::new(buf))),
                    Err(e) => effects.push(Effect::SetMessage(format!("Open failed: {e}"))),
                }
            }
            Event::PreviewFile { path, row, col, match_len } => {
                if self.preview_path.as_ref() == Some(path) {
                    return effects; // existing preview buffer handles repositioning
                }
                if self.preview_path.is_some() {
                    effects.push(Effect::KillPreview);
                }
                let path_str = path.to_string_lossy();
                match Buffer::from_file_with_waker(&path_str, ctx.waker.clone()) {
                    Ok(mut buf) => {
                        buf.preview = true;
                        let r = (*row).min(buf.line_count().saturating_sub(1));
                        buf.cursor_row = r;
                        buf.cursor_col = (*col).min(buf.line_len(r));
                        buf.scroll_offset = r.saturating_sub(ctx.viewport_height / 2);
                        buf.highlight_match(*row, *col, *match_len);
                        self.preview_path = Some(path.clone());
                        effects.push(Effect::Spawn(Box::new(buf)));
                    }
                    Err(e) => effects.push(Effect::SetMessage(format!("Preview failed: {e}"))),
                }
            }
            Event::PreviewClosed => {
                if self.preview_path.take().is_some() {
                    effects.push(Effect::KillPreview);
                }
            }
            Event::ConfirmSearch { path, row, col } => {
                if self.preview_path.as_ref() == Some(path) {
                    self.preview_path = None;
                    // Preview buffer promotes itself via Buffer.handle_event
                } else {
                    // No preview for this path — ensure buffer exists
                    self.preview_path = None;
                    effects.push(Effect::Emit(Event::OpenFile(path.clone())));
                    effects.push(Effect::Emit(Event::GoToPosition {
                        path: path.clone(), row: *row, col: *col,
                    }));
                    effects.push(Effect::FocusPanel(PanelSlot::Main));
                }
            }
            _ => {}
        }
        effects
    }

    fn draw(&mut self, _frame: &mut Frame, _area: Rect, _ctx: &DrawContext) {}

    fn save_session(&self, _ctx: &mut Context) {}

    fn restore_session(&mut self, _ctx: &mut Context) {}
}
