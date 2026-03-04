use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rusqlite::Connection;

use crate::buffer::Buffer;
use crate::config::{Action, KeyCombo, Keymap, KeymapLookup};
use crate::file_browser::FileBrowser;
use crate::session::{self, BufferState, SessionData};
use crate::theme::Theme;

pub enum InputResult {
    Continue,
    Quit,
}

#[derive(Default)]
enum ChordState {
    #[default]
    None,
    Pending(KeyCombo),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Editor,
    Browser,
}

pub enum PendingAction {
    KillBuffer,
}

pub struct Modal {
    pub prompt: String,
    pub input: String,
    pub action: PendingAction,
}

pub struct Editor {
    buffers: Vec<Buffer>,
    active_tab: usize,
    pub message: Option<String>,
    chord: ChordState,
    keymap: Keymap,
    pub focus: Focus,
    pub file_browser: FileBrowser,
    pub show_side_panel: bool,
    pub debug: bool,
    pub theme: Theme,
    pub viewport_height: usize,
    debug_flash: Option<(String, Instant)>,
    pub modal: Option<Modal>,
    last_persist: Instant,
    saved_paths: Vec<PathBuf>,
}

impl Editor {
    pub fn new(buffer: Option<Buffer>, keymap: Keymap, root: PathBuf, theme: Theme) -> Self {
        let (buffers, focus) = match buffer {
            Some(b) => (vec![b], Focus::Editor),
            None => (Vec::new(), Focus::Browser),
        };
        Self {
            buffers,
            active_tab: 0,
            message: None,
            chord: ChordState::None,
            keymap,
            focus,
            file_browser: FileBrowser::new(root),
            show_side_panel: true,
            debug: false,
            theme,
            viewport_height: 24,
            debug_flash: None,
            modal: None,
            last_persist: Instant::now(),
            saved_paths: Vec::new(),
        }
    }

    pub fn active_buffer(&self) -> Option<&Buffer> {
        self.buffers.get(self.active_tab)
    }

    pub fn active_buffer_mut(&mut self) -> Option<&mut Buffer> {
        self.buffers.get_mut(self.active_tab)
    }

    pub fn buffers(&self) -> &[Buffer] {
        &self.buffers
    }

    pub fn active_tab(&self) -> usize {
        self.active_tab
    }

    pub fn handle_key_event(&mut self, key: KeyEvent) -> InputResult {
        let combo = KeyCombo::from_key_event(&key);

        // Debug flash (before chord resolution so Pending state is visible)
        if self.debug {
            let display = if let ChordState::Pending(prefix) = &self.chord {
                format!("{} -> {}", prefix.display_name(), combo.display_name())
            } else {
                combo.display_name()
            };
            self.debug_flash = Some((display, Instant::now()));
        }

        // Handle modal input — intercepts all keys before normal dispatch
        if self.modal.is_some() {
            let is_abort = matches!(key.code, KeyCode::Esc)
                || (key.code == KeyCode::Char('g')
                    && key.modifiers.contains(KeyModifiers::CONTROL));
            if is_abort {
                self.modal = None;
                self.message = Some("Aborted.".into());
            } else if key.code == KeyCode::Enter {
                let confirmed = self.modal.as_ref().unwrap().input == "yes";
                if confirmed {
                    let action = &self.modal.as_ref().unwrap().action;
                    match action {
                        PendingAction::KillBuffer => self.kill_current_buffer(),
                    }
                } else {
                    self.message = Some("Aborted.".into());
                }
                self.modal = None;
            } else if key.code == KeyCode::Backspace {
                if let Some(ref mut modal) = self.modal {
                    modal.input.pop();
                }
            } else if let KeyCode::Char(c) = key.code {
                let has_ctrl_alt =
                    key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                if !has_ctrl_alt {
                    if let Some(ref mut modal) = self.modal {
                        modal.input.push(c);
                    }
                }
            }
            return InputResult::Continue;
        }

        // Handle chord state first
        if let ChordState::Pending(prefix) = self.chord {
            self.chord = ChordState::None;
            if let Some(action) = self.keymap.lookup_chord(&prefix, &combo) {
                return self.execute_action(action);
            }
            self.message = Some("Unknown chord.".into());
            return InputResult::Continue;
        }

        let context = match self.focus {
            Focus::Browser => Some("browser"),
            Focus::Editor => None,
        };

        match self.keymap.lookup(&combo, context) {
            KeymapLookup::Action(action) => self.execute_action(action),
            KeymapLookup::ChordPrefix => {
                self.chord = ChordState::Pending(combo);
                self.message = None;
                InputResult::Continue
            }
            KeymapLookup::Unbound => {
                // Printable character fallback: insert if no ctrl/alt modifier
                // and only when editor is focused
                if self.focus == Focus::Editor && !self.buffers.is_empty() {
                    let has_ctrl_alt = key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                    if let KeyCode::Char(c) = key.code {
                        if !has_ctrl_alt {
                            self.active_buffer_mut().unwrap().insert_char(c);
                        }
                    }
                }
                InputResult::Continue
            }
        }
    }

    fn execute_action(&mut self, action: Action) -> InputResult {
        match action {
            // Global actions
            Action::ToggleFocus => {
                if self.show_side_panel {
                    self.focus = match self.focus {
                        Focus::Editor => Focus::Browser,
                        Focus::Browser if !self.buffers.is_empty() => Focus::Editor,
                        _ => self.focus,
                    };
                }
            }
            Action::ToggleSidePanel => {
                self.show_side_panel = !self.show_side_panel;
                if !self.show_side_panel && !self.buffers.is_empty() {
                    self.focus = Focus::Editor;
                }
            }
            Action::Quit => return InputResult::Quit,

            // Browser-specific actions (bound via [browser] context)
            Action::ExpandDir => self.file_browser.expand_selected(),
            Action::CollapseDir => self.file_browser.collapse_selected(),
            Action::OpenSelected => {
                if let Some(path) = self.file_browser.open_selected() {
                    self.open_file(&path.to_string_lossy());
                    self.focus = Focus::Editor;
                }
            }
            Action::OpenSelectedBg => {
                if let Some(path) = self.file_browser.open_selected() {
                    self.open_file(&path.to_string_lossy());
                }
            }

            // Tab switching
            Action::PrevTab => {
                if self.buffers.len() > 1 {
                    if self.active_tab == 0 {
                        self.active_tab = self.buffers.len() - 1;
                    } else {
                        self.active_tab -= 1;
                    }
                    self.sync_browser_selection();
                }
            }
            Action::NextTab => {
                if self.buffers.len() > 1 {
                    self.active_tab = (self.active_tab + 1) % self.buffers.len();
                    self.sync_browser_selection();
                }
            }

            Action::KillBuffer => {
                if !self.buffers.is_empty() {
                    if self.buffers[self.active_tab].dirty {
                        self.modal = Some(Modal {
                            prompt: "Buffer modified; kill anyway? (yes/no)".into(),
                            input: String::new(),
                            action: PendingAction::KillBuffer,
                        });
                    } else {
                        self.kill_current_buffer();
                    }
                }
            }

            Action::Abort => {
                if self.modal.is_some() {
                    self.modal = None;
                    self.message = Some("Aborted.".into());
                } else {
                    self.message = None;
                }
            }

            // Shared movement (routed by focus)
            Action::MoveUp => {
                if self.focus == Focus::Browser {
                    self.file_browser.move_up();
                } else if let Some(buf) = self.active_buffer_mut() {
                    buf.move_up();
                }
            }
            Action::MoveDown => {
                if self.focus == Focus::Browser {
                    self.file_browser.move_down();
                } else if let Some(buf) = self.active_buffer_mut() {
                    buf.move_down();
                }
            }
            Action::PageUp => {
                let vh = self.viewport_height;
                if self.focus == Focus::Browser {
                    self.file_browser.page_up(vh);
                } else if let Some(buf) = self.active_buffer_mut() {
                    buf.page_up(vh);
                }
            }
            Action::PageDown => {
                let vh = self.viewport_height;
                if self.focus == Focus::Browser {
                    self.file_browser.page_down(vh);
                } else if let Some(buf) = self.active_buffer_mut() {
                    buf.page_down(vh);
                }
            }

            // Editor-only actions (require an active buffer)
            Action::MoveLeft
            | Action::MoveRight
            | Action::LineStart
            | Action::LineEnd
            | Action::FileStart
            | Action::FileEnd
            | Action::InsertNewline
            | Action::DeleteBackward
            | Action::DeleteForward
            | Action::InsertTab
            | Action::KillLine
            | Action::Save
            | Action::Undo => {
                if let Some(buf) = self.active_buffer_mut() {
                    match action {
                        Action::MoveLeft => buf.move_left(),
                        Action::MoveRight => buf.move_right(),
                        Action::LineStart => buf.move_to_line_start(),
                        Action::LineEnd => buf.move_to_line_end(),
                        Action::FileStart => buf.move_to_file_start(),
                        Action::FileEnd => buf.move_to_file_end(),
                        Action::InsertNewline => buf.insert_newline(),
                        Action::DeleteBackward => buf.delete_char_backward(),
                        Action::DeleteForward => buf.delete_char_forward(),
                        Action::InsertTab => buf.insert_char('\t'),
                        Action::KillLine => buf.kill_line(),
                        Action::Undo => buf.undo(),
                        Action::Save => match buf.save() {
                            Ok(()) => {
                                let name = buf.filename().to_string();
                                if let Some(p) = buf.path.clone() {
                                    self.saved_paths.push(p);
                                }
                                self.message = Some(format!("Saved {name}."));
                            }
                            Err(e) => self.message = Some(format!("Save failed: {e}")),
                        },
                        _ => unreachable!(),
                    }
                }
            }
        }
        InputResult::Continue
    }

    fn kill_current_buffer(&mut self) {
        if !self.buffers.is_empty() {
            let name = self.buffers[self.active_tab].filename().to_string();
            self.buffers.remove(self.active_tab);
            if self.buffers.is_empty() {
                self.active_tab = 0;
                self.focus = Focus::Browser;
            } else if self.active_tab >= self.buffers.len() {
                self.active_tab = self.buffers.len() - 1;
            }
            self.message = Some(format!("Killed {name}."));
            self.sync_browser_selection();
        }
    }

    fn sync_browser_selection(&mut self) {
        if let Some(buf) = self.active_buffer() {
            if let Some(path) = buf.path.clone() {
                self.file_browser.reveal(&path);
            }
        }
    }

    fn open_file(&mut self, path: &str) {
        // If the file is already open, switch to that tab
        if let Some(idx) = self
            .buffers
            .iter()
            .position(|b| b.path.as_ref().map(|p| p.as_path()) == Some(std::path::Path::new(path)))
        {
            self.active_tab = idx;
            let name = self.active_buffer().unwrap().filename().to_string();
            self.message = Some(format!("Switched to {name}."));
            return;
        }

        match Buffer::from_file(path) {
            Ok(buf) => {
                self.message = Some(format!("Opened {}.", buf.filename()));
                self.buffers.push(buf);
                self.active_tab = self.buffers.len() - 1;
            }
            Err(e) => self.message = Some(format!("Open failed: {e}")),
        }
    }

    pub fn capture_session(&self) -> SessionData {
        let buffers = self
            .buffers
            .iter()
            .filter_map(|b| {
                let path = b.path.as_ref()?;
                Some(BufferState {
                    file_path: path.clone(),
                    cursor_row: b.cursor_row,
                    cursor_col: b.cursor_col,
                    scroll_offset: b.scroll_offset,
                })
            })
            .collect();

        SessionData {
            buffers,
            active_tab: self.active_tab,
            focus_is_editor: self.focus == Focus::Editor,
            show_side_panel: self.show_side_panel,
            browser_selected: self.file_browser.selected,
            browser_expanded_dirs: self.file_browser.expanded_dirs().clone(),
        }
    }

    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    pub fn debug_flash_text(&self) -> Option<&str> {
        if let Some((ref text, instant)) = self.debug_flash {
            if instant.elapsed().as_millis() < 500 {
                return Some(text);
            }
        }
        None
    }

    pub fn needs_redraw_in(&self) -> Option<Duration> {
        if let Some((_, instant)) = &self.debug_flash {
            let elapsed = instant.elapsed();
            let deadline = Duration::from_millis(500);
            if elapsed < deadline {
                return Some(deadline - elapsed);
            }
        }
        None
    }

    pub fn needs_persist_in(&self) -> Option<Duration> {
        let has_unpersisted = self.buffers.iter().any(|b| b.has_unpersisted_undo())
            || !self.saved_paths.is_empty();
        if !has_unpersisted {
            return None;
        }
        let elapsed = self.last_persist.elapsed();
        let deadline = Duration::from_millis(200);
        if elapsed >= deadline {
            Some(Duration::ZERO)
        } else {
            Some(deadline - elapsed)
        }
    }

    pub fn needs_persist(&self) -> bool {
        let has_unpersisted = self.buffers.iter().any(|b| b.has_unpersisted_undo())
            || !self.saved_paths.is_empty();
        has_unpersisted && self.last_persist.elapsed() >= Duration::from_millis(200)
    }

    pub fn flush_to_db(&mut self, conn: &Connection, root: &Path) {
        let root_str = root.to_string_lossy();

        // Process saves: clear undo from DB for saved buffers
        for path in self.saved_paths.drain(..) {
            let file_str = path.to_string_lossy();
            session::clear_undo(conn, &root_str, &file_str);
        }

        // Flush unpersisted undo entries
        for buf in &mut self.buffers {
            if !buf.has_unpersisted_undo() {
                continue;
            }
            let Some(path) = buf.path.clone() else { continue };
            let file_str = path.to_string_lossy().into_owned();
            let entries = buf.drain_unpersisted_undo();
            if entries.is_empty() {
                continue;
            }
            let (undo_cursor, distance_from_save) = buf.undo_metadata();
            let content_hash = buf.content_hash();
            session::flush_undo_entries(
                conn,
                &root_str,
                &file_str,
                &entries,
                undo_cursor,
                distance_from_save,
                content_hash,
            );
        }

        self.last_persist = Instant::now();
    }

    pub fn restore_session(&mut self, session: SessionData, conn: Option<&Connection>, root: &Path) {
        self.buffers.clear();
        let root_str = root.to_string_lossy();
        for bs in &session.buffers {
            let path_str = bs.file_path.to_string_lossy();
            if let Ok(mut buf) = Buffer::from_file(&path_str) {
                // Try to restore undo state
                if let Some(conn) = conn {
                    if let Some((entries, undo_cursor, distance_from_save, stored_hash)) =
                        session::load_undo(conn, &root_str, &path_str)
                    {
                        let current_hash = buf.content_hash();
                        if current_hash == stored_hash {
                            buf.restore_undo(entries, undo_cursor, distance_from_save);
                        }
                    }
                }
                // Clamp cursor to valid ranges
                buf.cursor_row = bs.cursor_row.min(buf.line_count().saturating_sub(1));
                buf.cursor_col = bs.cursor_col.min(buf.line_len(buf.cursor_row));
                buf.scroll_offset = bs.scroll_offset;
                self.buffers.push(buf);
            }
        }

        if self.buffers.is_empty() {
            self.active_tab = 0;
        } else {
            self.active_tab = session.active_tab.min(self.buffers.len() - 1);
        }

        self.show_side_panel = session.show_side_panel;

        self.focus = if session.focus_is_editor && !self.buffers.is_empty() {
            Focus::Editor
        } else {
            Focus::Browser
        };

        self.file_browser.set_expanded_dirs(session.browser_expanded_dirs);
        self.file_browser.selected = session
            .browser_selected
            .min(self.file_browser.entries.len().saturating_sub(1));
    }
}
