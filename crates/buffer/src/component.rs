use std::collections::HashMap;
use std::path::PathBuf;

use led_core::lsp_types::DiagnosticSeverity;
use led_core::{
    Action, BLANK_STYLE, Component, Context, DrawContext, Effect, ElementStyle, Event, PanelClaim,
    PanelSlot, TabDescriptor, Theme,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::color_hint::{evaluate_theme_line, parse_color_defs, scan_hex_color};
use crate::syntax::HighlightSpan;
use crate::wrap::{chars_to_string, compute_chunks, expand_tabs, find_sub_line, visual_line_count};
use crate::{Buffer, UndoEntry};

fn resolve_capture_style(theme: &Theme, capture_name: &str, text_style: Style) -> Style {
    let blank = BLANK_STYLE.to_style();
    let key = format!("syntax.{capture_name}");
    let s = theme.get(&key).to_style();
    if s != blank {
        return s;
    }
    if let Some(dot) = capture_name.find('.') {
        let parent_key = format!("syntax.{}", &capture_name[..dot]);
        let s = theme.get(&parent_key).to_style();
        if s != blank {
            return s;
        }
    }
    text_style
}

impl Buffer {
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

    fn do_save_session(&self, ctx: &mut Context) {
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

    fn do_restore_session(&mut self, ctx: &mut Context) {
        let Some(conn) = ctx.db else { return };
        let Some(ref path) = self.path else { return };
        let root_str = ctx.root.to_string_lossy();
        let file_str = path.to_string_lossy();

        let cursor_data: Option<(i64, i64, i64)> = conn
            .query_row(
                "SELECT cursor_row, cursor_col, scroll_offset
                 FROM buffers WHERE root_path = ?1 AND file_path = ?2",
                rusqlite::params![root_str, file_str],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();

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

        if let Some((row, col, scroll)) = cursor_data {
            let row = row as usize;
            let col = col as usize;
            self.cursor_row = row.min(self.line_count().saturating_sub(1));
            self.cursor_col = col.min(self.line_len(self.cursor_row));
            self.scroll_offset = scroll as usize;
        }
    }

    fn draw_status_bar(&self, frame: &mut Frame, area: Rect, ctx: &mut DrawContext) {
        let style = ctx.theme.get("status_bar.style").to_style();

        if let Some(ref isearch) = self.isearch {
            let prompt = if isearch.failed {
                format!("Failing search: {}", isearch.query)
            } else {
                format!("Search: {}", isearch.query)
            };
            let padding = (area.width as usize).saturating_sub(prompt.len() + 1);
            let bar = format!(" {prompt}{:padding$}", "");
            let paragraph = Paragraph::new(bar).style(style);
            frame.render_widget(paragraph, area);
            let cursor_x = area.x + 1 + prompt.len() as u16;
            ctx.cursor_pos = Some((cursor_x, area.y));
            return;
        }

        // Check for diagnostic on current line
        let diag_msg = self.diagnostics.iter().find_map(|d| {
            if d.range.start.row <= self.cursor_row && d.range.end.row >= self.cursor_row {
                Some(d.message.as_str())
            } else {
                None
            }
        });

        if let Some(msg) = diag_msg {
            let truncated: String = msg.chars().take(area.width as usize - 2).collect();
            let padding = (area.width as usize).saturating_sub(truncated.len() + 2);
            let bar = format!(" {truncated}{:padding$} ", "");
            let diag_style = style.fg(Color::Yellow);
            let paragraph = Paragraph::new(bar).style(diag_style);
            frame.render_widget(paragraph, area);
            return;
        }

        let filename = self.filename();
        let modified = if self.dirty { " \u{25cf}" } else { "" };
        let branch = ctx.file_statuses.branch.as_deref().unwrap_or("");
        let branch_display = if branch.is_empty() {
            String::new()
        } else {
            format!(" ({branch})")
        };
        // Build LSP status string (placed after branch, left-aligned).
        let lsp_str = if let Some(lsp) = ctx.lsp_status {
            let tick = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let spinner_char = |offset: u128| -> char {
                const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
                FRAMES[((tick + offset) / 80) as usize % FRAMES.len()]
            };
            let spinner = if lsp.busy {
                format!("{} ", spinner_char(0))
            } else {
                String::new()
            };
            let detail = lsp
                .detail
                .as_ref()
                .map(|d| {
                    if lsp.busy {
                        format!("  {} {d}", spinner_char(400))
                    } else {
                        format!("  {d}")
                    }
                })
                .unwrap_or_default();
            format!("  {spinner}{}{detail}", lsp.server_name)
        } else {
            String::new()
        };

        let left = format!(" {filename}{modified}{branch_display}{lsp_str}");
        let pos = format!("L{}:C{} ", self.cursor_row + 1, self.cursor_col + 1);
        let padding = (area.width as usize).saturating_sub(left.len() + pos.len());
        let bar = format!("{left}{:padding$}{pos}", "");
        let paragraph = Paragraph::new(bar).style(style);
        frame.render_widget(paragraph, area);
    }

    fn goto_next_diagnostic(&mut self) {
        if self.diagnostics.is_empty() {
            return;
        }
        // Find the next diagnostic after the cursor
        let after = self
            .diagnostics
            .iter()
            .find(|d| {
                d.range.start.row > self.cursor_row
                    || (d.range.start.row == self.cursor_row && d.range.start.col > self.cursor_col)
            })
            .or_else(|| self.diagnostics.first()); // wrap around
        if let Some(d) = after {
            self.cursor_row = d.range.start.row.min(self.line_count().saturating_sub(1));
            self.cursor_col = d.range.start.col.min(self.line_len(self.cursor_row));
        }
    }

    fn goto_prev_diagnostic(&mut self) {
        if self.diagnostics.is_empty() {
            return;
        }
        let before = self
            .diagnostics
            .iter()
            .rev()
            .find(|d| {
                d.range.start.row < self.cursor_row
                    || (d.range.start.row == self.cursor_row && d.range.start.col < self.cursor_col)
            })
            .or_else(|| self.diagnostics.last()); // wrap around
        if let Some(d) = before {
            self.cursor_row = d.range.start.row.min(self.line_count().saturating_sub(1));
            self.cursor_col = d.range.start.col.min(self.line_len(self.cursor_row));
        }
    }

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
                vrow += visual_line_count(expand_tabs(&self.line(li)).0.len(), text_width);
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
            let vl = visual_line_count(expand_tabs(&self.line(li)).0.len(), text_width);
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
    fn panel_claims(&self) -> &[PanelClaim] {
        if self.isearch.is_some() {
            &self.claims_with_status
        } else {
            &self.claims
        }
    }

    fn tab(&self) -> Option<TabDescriptor> {
        Some(TabDescriptor {
            label: self.filename().to_string(),
            dirty: self.dirty,
            path: self.path.clone(),
            preview: self.preview,
            read_only: self.read_only,
        })
    }

    fn cursor_position(&self) -> Option<(usize, usize, usize)> {
        Some((self.cursor_row, self.cursor_col, self.scroll_offset))
    }

    fn context_name(&self) -> Option<&str> {
        if self.isearch.is_some() {
            Some("isearch")
        } else {
            None
        }
    }

    fn handle_action(&mut self, action: Action, ctx: &mut Context) -> Vec<Effect> {
        // Gate mutating actions on read-only buffers
        if self.read_only {
            match action {
                Action::InsertChar(_)
                | Action::InsertNewline
                | Action::DeleteBackward
                | Action::DeleteForward
                | Action::InsertTab
                | Action::Yank
                | Action::Save
                | Action::SaveForce
                | Action::LspFormat => {
                    return vec![Effect::SetMessage("Buffer is read-only".into())];
                }
                _ => {}
            }
        }

        // Promote preview buffer on first edit (consumes the keystroke)
        if self.preview {
            match action {
                Action::InsertChar(_)
                | Action::InsertNewline
                | Action::DeleteBackward
                | Action::DeleteForward
                | Action::InsertTab
                | Action::KillLine
                | Action::KillRegion
                | Action::Yank => {
                    self.preview = false;
                    return vec![Effect::Emit(Event::PreviewPromoted)];
                }
                _ => {}
            }
        }

        // Intercept actions during incremental search
        if self.isearch.is_some() {
            match action {
                Action::InsertChar(c) => {
                    self.isearch.as_mut().unwrap().query.push(c);
                    self.update_search();
                    return vec![];
                }
                Action::DeleteBackward => {
                    let empty = {
                        let is = self.isearch.as_mut().unwrap();
                        is.query.pop();
                        is.query.is_empty()
                    };
                    if empty {
                        // Restore origin when query becomes empty
                        let is = self.isearch.as_ref().unwrap();
                        self.cursor_row = is.origin.0;
                        self.cursor_col = is.origin.1;
                        let is = self.isearch.as_mut().unwrap();
                        is.matches.clear();
                        is.match_idx = None;
                        is.failed = false;
                    } else {
                        self.update_search();
                    }
                    return vec![];
                }
                Action::InBufferSearch => {
                    self.search_next();
                    return vec![];
                }
                Action::Abort => {
                    self.search_cancel();
                    return vec![];
                }
                Action::InsertNewline => {
                    self.search_accept();
                    return vec![];
                }
                Action::MoveUp
                | Action::MoveDown
                | Action::MoveLeft
                | Action::MoveRight
                | Action::LineStart
                | Action::LineEnd
                | Action::PageUp
                | Action::PageDown
                | Action::FileStart
                | Action::FileEnd => {
                    self.search_accept();
                    // Fall through to normal handling below
                }
                // Lifecycle actions: pass through without exiting isearch
                Action::Tick
                | Action::Flush
                | Action::SaveSession
                | Action::RestoreSession
                | Action::FocusGained
                | Action::FocusLost => {
                    // Fall through to normal handling below
                }
                _ => {
                    self.search_accept();
                    // Fall through to normal handling below
                }
            }
        }

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
                if self.read_only {
                    let col = self.cursor_col;
                    let len = self.current_line_len();
                    if col < len {
                        let start = self.char_idx(self.cursor_row, col);
                        let end = self.char_idx(self.cursor_row, len);
                        let text = self.rope.slice(start..end).to_string();
                        ctx.clipboard.set_text(&text);
                    }
                    vec![]
                } else if let Some(killed) = self.kill_line() {
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
                            let mut effects = vec![Effect::SetMessage(format!("Saved {name}."))];
                            if let Some(ref path) = self.path {
                                effects.push(Effect::Emit(Event::FileSaved(path.clone())));
                            }
                            effects
                        }
                        Err(e) => vec![Effect::SetMessage(format!("Save failed: {e}"))],
                    }
                }
            }
            Action::SaveForce => match self.save(ctx) {
                Ok(()) => {
                    let name = self.filename().to_string();
                    let mut effects = vec![Effect::SetMessage(format!("Saved {name}."))];
                    if let Some(ref path) = self.path {
                        effects.push(Effect::Emit(Event::FileSaved(path.clone())));
                    }
                    effects
                }
                Err(e) => vec![Effect::SetMessage(format!("Save failed: {e}"))],
            },
            Action::Tick => {
                let mut effects = self.handle_tick(ctx);
                // Request inlay hints when viewport changes
                if self.inlay_hints_enabled {
                    let start_row = self.scroll_offset;
                    let end_row = start_row + ctx.viewport_height + 10;
                    let new_range = (start_row, end_row);
                    let should_request = match self.last_hint_range {
                        Some((prev_start, prev_end)) => {
                            (start_row as isize - prev_start as isize).unsigned_abs() >= 5
                                || (end_row as isize - prev_end as isize).unsigned_abs() >= 5
                        }
                        None => true,
                    };
                    if should_request {
                        self.last_hint_range = Some(new_range);
                        if let Some(ref path) = self.path {
                            effects.push(Effect::Emit(Event::LspInlayHints {
                                path: path.clone(),
                                start_row,
                                end_row,
                            }));
                        }
                    }
                }
                effects
            }
            Action::SetMark => {
                self.set_mark();
                vec![Effect::SetMessage("Mark set".into())]
            }
            Action::KillRegion => {
                if self.read_only {
                    if let Some(text) = self.selected_text() {
                        ctx.clipboard.set_text(&text);
                        self.clear_mark();
                        vec![]
                    } else {
                        vec![Effect::SetMessage("No region".into())]
                    }
                } else if let Some(text) = self.kill_region() {
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
            Action::InBufferSearch => {
                self.start_search();
                vec![]
            }
            Action::Abort => {
                self.clear_mark();
                vec![]
            }
            Action::FocusGained => {
                if self.preview_highlight {
                    self.preview_highlight = false;
                    self.clear_mark();
                }
                vec![]
            }
            Action::FocusLost => vec![],
            Action::RestoreSession => {
                self.ensure_schema(ctx);
                self.do_restore_session(ctx);
                vec![]
            }
            Action::SaveSession => {
                self.do_save_session(ctx);
                vec![]
            }
            Action::Flush => {
                if self.has_unpersisted_undo() {
                    self.flush_undo_to_db(ctx);
                }
                vec![]
            }
            Action::LspGotoDefinition => {
                if let Some(ref path) = self.path {
                    vec![
                        Effect::Emit(Event::RecordJump {
                            path: path.clone(),
                            row: self.cursor_row,
                            col: self.cursor_col,
                            scroll_offset: self.scroll_offset,
                        }),
                        Effect::SetMessage("LSP: goto definition...".into()),
                        Effect::Emit(Event::LspGotoDefinition {
                            path: path.clone(),
                            row: self.cursor_row,
                            col: self.cursor_col,
                        }),
                    ]
                } else {
                    vec![]
                }
            }
            Action::LspRename => {
                if let Some(ref path) = self.path {
                    let initial = self.word_at_cursor().unwrap_or_default();
                    vec![Effect::PromptRename {
                        prompt: "Rename to:".into(),
                        initial: initial.clone(),
                        path: path.clone(),
                        row: self.cursor_row,
                        col: self.cursor_col,
                    }]
                } else {
                    vec![]
                }
            }
            Action::LspCodeAction => {
                if let Some(ref path) = self.path {
                    let (start_row, start_col, end_row, end_col) =
                        if let Some(((sr, sc), (er, ec))) = self.selection_range() {
                            (sr, sc, er, ec)
                        } else {
                            (
                                self.cursor_row,
                                self.cursor_col,
                                self.cursor_row,
                                self.cursor_col,
                            )
                        };
                    vec![Effect::Emit(Event::LspCodeAction {
                        path: path.clone(),
                        start_row,
                        start_col,
                        end_row,
                        end_col,
                    })]
                } else {
                    vec![]
                }
            }
            Action::LspFormat => {
                if let Some(ref path) = self.path {
                    vec![Effect::Emit(Event::LspFormat { path: path.clone() })]
                } else {
                    vec![]
                }
            }
            Action::LspNextDiagnostic => {
                self.goto_next_diagnostic();
                vec![]
            }
            Action::LspPrevDiagnostic => {
                self.goto_prev_diagnostic();
                vec![]
            }
            Action::LspToggleInlayHints => {
                self.inlay_hints_enabled = !self.inlay_hints_enabled;
                if self.inlay_hints_enabled {
                    if let Some(ref path) = self.path {
                        let start_row = self.scroll_offset;
                        let end_row = start_row + ctx.viewport_height + 10;
                        vec![
                            Effect::SetMessage("Inlay hints enabled".into()),
                            Effect::Emit(Event::LspInlayHints {
                                path: path.clone(),
                                start_row,
                                end_row,
                            }),
                        ]
                    } else {
                        vec![]
                    }
                } else {
                    self.inlay_hints.clear();
                    self.last_hint_range = None;
                    vec![Effect::SetMessage("Inlay hints disabled".into())]
                }
            }
            _ => vec![],
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> Vec<Effect> {
        match event {
            Event::Resume => {
                // Force a tick to check for disk changes after resume
                return self.handle_tick(ctx);
            }
            Event::GoToPosition {
                path,
                row,
                col,
                scroll_offset,
            } => {
                if self.path.as_deref() == Some(path.as_path()) {
                    self.cursor_row = (*row).min(self.line_count().saturating_sub(1));
                    self.cursor_col = (*col).min(self.line_len(self.cursor_row));
                    if let Some(offset) = scroll_offset {
                        self.scroll_offset = (*offset).min(self.line_count().saturating_sub(1));
                    }
                    self.clear_mark();
                }
            }
            Event::PreviewFile {
                path,
                row,
                col,
                match_len,
            } => {
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
            Event::SetDiagnostics { path, diagnostics } => {
                if self.path.as_deref() == Some(path.as_path()) {
                    self.diagnostics = diagnostics.clone();
                }
            }
            Event::ApplyEdits { path, edits } => {
                if self.path.as_deref() == Some(path.as_path()) && !self.read_only {
                    self.apply_text_edits(edits.clone());
                }
            }
            Event::SetInlayHints { path, hints } => {
                if self.path.as_deref() == Some(path.as_path()) && self.inlay_hints_enabled {
                    self.inlay_hints = hints.clone();
                }
            }
            Event::TabActivated { path } => {
                if self.preview && path.as_ref() != self.path.as_ref() {
                    return vec![Effect::Emit(Event::PreviewClosed)];
                }
            }
            _ => {}
        }
        vec![]
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, ctx: &mut DrawContext) {
        if ctx.slot == PanelSlot::StatusBar {
            self.draw_status_bar(frame, area, ctx);
            return;
        }

        let height = area.height as usize;
        let gutter_width: usize = 2;
        let text_width = (area.width as usize).saturating_sub(gutter_width);
        self.text_width = text_width;

        self.adjust_scroll(text_width, height);

        let total_lines = self.line_count();
        let gutter_style = ctx.theme.get("editor.gutter").to_style();
        let text_style = ctx.theme.get("editor.text").to_style();
        let sel_style = if self.preview_highlight {
            ctx.theme.get("file_search.search_current").to_style()
        } else {
            ctx.theme.get("editor.selection").to_style()
        };
        let search_match_style = ctx.theme.get("editor.search_match").to_style();
        let search_current_style = ctx.theme.get("editor.search_current").to_style();
        let sel_range = self.selection_range();

        // Pre-compute search match display ranges for visible lines
        let search_info: Option<(Vec<&(usize, usize, usize)>, Option<usize>)> =
            self.isearch.as_ref().and_then(|is| {
                if is.matches.is_empty() {
                    return None;
                }
                let visible: Vec<&(usize, usize, usize)> = is
                    .matches
                    .iter()
                    .filter(|(r, _, _)| {
                        *r >= self.scroll_offset && *r < self.scroll_offset + height
                    })
                    .collect();
                if visible.is_empty() {
                    return None;
                }
                Some((visible, is.match_idx))
            });

        // Pre-compute syntax highlights for visible lines
        let end_line = (self.scroll_offset + height).min(total_lines);
        let raw_highlights = if let Some(ref syntax) = self.syntax {
            syntax.highlights_for_lines(&self.rope, self.scroll_offset, end_line)
        } else {
            Vec::new()
        };
        let mut hl_map: HashMap<usize, Vec<&HighlightSpan>> = HashMap::new();
        for (line, span) in &raw_highlights {
            hl_map.entry(*line).or_default().push(span);
        }

        let is_theme = self
            .path
            .as_ref()
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
            let skip = if line_idx == self.scroll_offset {
                self.scroll_sub_line
            } else {
                0
            };

            // Color hint for gutter preview (first chunk only)
            let color_hint = if is_theme {
                // Track section headers
                let trimmed = raw.trim();
                if trimmed.starts_with('[') && !trimmed.starts_with("[[") {
                    if let Some(end) = trimmed.find(']') {
                        current_section = trimmed[1..end].to_string();
                    }
                }
                color_defs
                    .as_ref()
                    .and_then(|defs| evaluate_theme_line(&raw, &current_section, defs))
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

                // Gutter — left column: diagnostic or git status, right column: color preview
                if chunk_i == skip {
                    // Left gutter: diagnostic severity overrides git status
                    let diag_severity = self.diagnostics.iter().find_map(|d| {
                        if d.range.start.row <= line_idx && d.range.end.row >= line_idx {
                            Some(d.severity)
                        } else {
                            None
                        }
                    });
                    let left = if let Some(sev) = diag_severity {
                        let key = match sev {
                            DiagnosticSeverity::Error => "diagnostics.error",
                            DiagnosticSeverity::Warning => "diagnostics.warning",
                            DiagnosticSeverity::Info => "diagnostics.info",
                            DiagnosticSeverity::Hint => "diagnostics.hint",
                        };
                        let fg_color = ctx.theme.get(key).to_style().fg.unwrap_or(match sev {
                            DiagnosticSeverity::Error => Color::Red,
                            DiagnosticSeverity::Warning => Color::Yellow,
                            DiagnosticSeverity::Info => Color::Blue,
                            DiagnosticSeverity::Hint => Color::Gray,
                        });
                        Span::styled("▎", Style::default().fg(fg_color))
                    } else {
                        let line_kind = self
                            .path
                            .as_ref()
                            .and_then(|p| ctx.file_statuses.line_status_at(p, line_idx));
                        if let Some(kind) = line_kind {
                            let key = led_core::file_status::line_status_theme(kind);
                            let fg_color = ctx.theme.get(key).to_style().fg.unwrap_or(Color::Reset);
                            Span::styled("▎", Style::default().fg(fg_color))
                        } else {
                            Span::styled(" ", gutter_style)
                        }
                    };
                    // Right gutter: color preview
                    let right = if let Some(ref hint) = color_hint {
                        if hint.bg != Color::Reset {
                            Span::styled("A", hint.to_style())
                        } else {
                            let block_style = ratatui::style::Style::default().bg(hint.fg);
                            Span::styled(" ", block_style)
                        }
                    } else {
                        Span::styled(" ", gutter_style)
                    };
                    spans.push(left);
                    spans.push(right);
                } else {
                    spans.push(Span::styled("  ", gutter_style));
                }

                // Content with syntax highlighting + selection overlay
                if !chunk_text.is_empty() {
                    let chunk_len = ce - cs;
                    let mut col_styles = vec![text_style; chunk_len];

                    // Apply syntax highlighting — sort by span size descending
                    // so inner (more specific) captures overwrite outer ones.
                    if let Some(line_hl) = hl_map.get(&line_idx) {
                        let mut sorted_hl: Vec<_> = line_hl.iter().collect();
                        sorted_hl.sort_by_key(|hs| std::cmp::Reverse(hs.char_end - hs.char_start));
                        for hs in sorted_hl {
                            let ds = char_map
                                .get(hs.char_start)
                                .copied()
                                .unwrap_or(display.len());
                            let de = char_map.get(hs.char_end).copied().unwrap_or(display.len());
                            let style =
                                resolve_capture_style(ctx.theme, hs.capture_name, text_style);
                            for i in ds.max(cs)..de.min(ce) {
                                col_styles[i - cs] = style;
                            }
                        }
                    }

                    // Apply selection overlay
                    if let Some((ss, se)) = sel_dcols {
                        for i in ss.max(cs)..se.min(ce) {
                            col_styles[i - cs] = sel_style;
                        }
                    }

                    // Apply diagnostic underlines
                    for diag in &self.diagnostics {
                        if diag.range.start.row <= line_idx && diag.range.end.row >= line_idx {
                            let ds = if diag.range.start.row == line_idx {
                                char_map.get(diag.range.start.col).copied().unwrap_or(0)
                            } else {
                                0
                            };
                            let de = if diag.range.end.row == line_idx {
                                char_map
                                    .get(diag.range.end.col)
                                    .copied()
                                    .unwrap_or(display.len())
                            } else {
                                display.len()
                            };
                            let fg_color = match diag.severity {
                                DiagnosticSeverity::Error => Color::Red,
                                DiagnosticSeverity::Warning => Color::Yellow,
                                DiagnosticSeverity::Info => Color::Blue,
                                DiagnosticSeverity::Hint => Color::Gray,
                            };
                            for i in ds.max(cs)..de.min(ce) {
                                col_styles[i - cs] = col_styles[i - cs]
                                    .add_modifier(Modifier::UNDERLINED)
                                    .fg(fg_color);
                            }
                        }
                    }

                    // Apply search match overlay
                    if let Some((ref visible, _current_idx)) = search_info {
                        let current_match = self
                            .isearch
                            .as_ref()
                            .and_then(|is| is.match_idx.map(|i| &is.matches[i]));
                        for &(mr, mc, mlen) in visible.iter() {
                            if *mr != line_idx {
                                continue;
                            }
                            let ms = char_map.get(*mc).copied().unwrap_or(display.len());
                            let me = char_map.get(mc + mlen).copied().unwrap_or(display.len());
                            let is_current =
                                current_match.map_or(false, |cm| cm.0 == *mr && cm.1 == *mc);
                            let style = if is_current {
                                search_current_style
                            } else {
                                search_match_style
                            };
                            for i in ms.max(cs)..me.min(ce) {
                                col_styles[i - cs] = style;
                            }
                        }
                    }

                    // Group consecutive same-style columns into spans
                    let mut pos = 0;
                    while pos < chunk_len {
                        let style = col_styles[pos];
                        let mut end = pos + 1;
                        while end < chunk_len && col_styles[end] == style {
                            end += 1;
                        }
                        spans.push(Span::styled(chars_to_string(&chunk_text[pos..end]), style));
                        pos = end;
                    }
                }

                // Pad selection to line edge on last chunk when selection continues
                if let Some((_, _)) = sel_dcols {
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
                }

                // Inlay hints as ghost text at end of line (only on last chunk)
                if is_last && self.inlay_hints_enabled {
                    let hint_style = ctx.theme.get("editor.inlay_hint").to_style();
                    let hint_style = if hint_style == BLANK_STYLE.to_style() {
                        Style::default().fg(Color::DarkGray)
                    } else {
                        hint_style
                    };
                    for hint in &self.inlay_hints {
                        if hint.position.row == line_idx {
                            spans.push(Span::styled(format!(" {}", hint.label), hint_style));
                        }
                    }
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
        ctx.cursor_pos = cursor_pos;
    }
}

// ---------------------------------------------------------------------------
// BufferFactory
// ---------------------------------------------------------------------------

fn is_read_only_path(path: &std::path::Path) -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let read_only_prefixes = [
        home.join(".cargo/registry"),
        home.join(".rustup/toolchains"),
    ];
    read_only_prefixes
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

pub struct BufferFactory {
    preview_path: Option<PathBuf>,
}

impl BufferFactory {
    pub fn new() -> Self {
        Self { preview_path: None }
    }
}

impl Component for BufferFactory {
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
                    Ok(mut buf) => {
                        buf.read_only = is_read_only_path(path);
                        effects.push(Effect::Spawn(Box::new(buf)));
                    }
                    Err(e) => effects.push(Effect::SetMessage(format!("Open failed: {e}"))),
                }
            }
            Event::OpenDefinition(path) => {
                if self.preview_path.is_some() {
                    effects.push(Effect::KillPreview);
                }
                let path_str = path.to_string_lossy();
                match Buffer::from_file_with_waker(&path_str, ctx.waker.clone()) {
                    Ok(mut buf) => {
                        buf.preview = true;
                        buf.read_only = is_read_only_path(path);
                        self.preview_path = Some(path.clone());
                        effects.push(Effect::Spawn(Box::new(buf)));
                    }
                    Err(e) => effects.push(Effect::SetMessage(format!("Open failed: {e}"))),
                }
            }
            Event::PreviewFile {
                path,
                row,
                col,
                match_len,
            } => {
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
                        path: path.clone(),
                        row: *row,
                        col: *col,
                        scroll_offset: None,
                    }));
                    effects.push(Effect::FocusPanel(PanelSlot::Main));
                }
            }
            Event::PreviewPromoted => {
                self.preview_path = None;
            }
            _ => {}
        }
        effects
    }

    fn draw(&mut self, _frame: &mut Frame, _area: Rect, _ctx: &mut DrawContext) {}
}
