use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::buffer::Buffer;
use crate::config::{Action, KeyCombo, Keymap, KeymapLookup};
use crate::file_browser::FileBrowser;
use crate::session::{BufferState, SessionData};

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

enum Mode {
    Normal,
    Prompt {
        label: String,
        input: String,
        action: PromptAction,
    },
}

enum PromptAction {
    OpenFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Editor,
    Browser,
}

pub struct Editor {
    buffers: Vec<Buffer>,
    active_tab: usize,
    pub message: Option<String>,
    chord: ChordState,
    mode: Mode,
    keymap: Keymap,
    pub focus: Focus,
    pub file_browser: FileBrowser,
    pub show_side_panel: bool,
    pub debug: bool,
}

impl Editor {
    pub fn new(buffer: Option<Buffer>, keymap: Keymap, root: PathBuf) -> Self {
        let (buffers, focus) = match buffer {
            Some(b) => (vec![b], Focus::Editor),
            None => (Vec::new(), Focus::Browser),
        };
        Self {
            buffers,
            active_tab: 0,
            message: None,
            chord: ChordState::None,
            mode: Mode::Normal,
            keymap,
            focus,
            file_browser: FileBrowser::new(root),
            show_side_panel: true,
            debug: false,
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

    pub fn is_chord_pending(&self) -> bool {
        matches!(self.chord, ChordState::Pending(_))
    }

    pub fn prompt_display(&self) -> Option<(&str, &str)> {
        match &self.mode {
            Mode::Prompt { label, input, .. } => Some((label.as_str(), input.as_str())),
            _ => None,
        }
    }

    pub fn handle_key_event(&mut self, key: KeyEvent) -> InputResult {
        match &self.mode {
            Mode::Prompt { .. } => self.handle_prompt_key(key),
            Mode::Normal => self.handle_normal_key(key),
        }
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) -> InputResult {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.message = Some("Cancelled.".into());
            }
            KeyCode::Enter => {
                // Extract prompt state
                let mode = std::mem::replace(&mut self.mode, Mode::Normal);
                if let Mode::Prompt { input, action, .. } = mode {
                    self.execute_prompt(action, &input);
                }
            }
            KeyCode::Backspace => {
                if let Mode::Prompt { input, .. } = &mut self.mode {
                    input.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Mode::Prompt { input, .. } = &mut self.mode {
                    input.push(c);
                }
            }
            _ => {}
        }
        InputResult::Continue
    }

    fn execute_prompt(&mut self, action: PromptAction, input: &str) {
        match action {
            PromptAction::OpenFile => self.open_file(input),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> InputResult {
        let combo = KeyCombo::from_key_event(&key);

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

            // Tab switching
            Action::PrevTab => {
                if self.buffers.len() > 1 {
                    if self.active_tab == 0 {
                        self.active_tab = self.buffers.len() - 1;
                    } else {
                        self.active_tab -= 1;
                    }
                }
            }
            Action::NextTab => {
                if self.buffers.len() > 1 {
                    self.active_tab = (self.active_tab + 1) % self.buffers.len();
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

            // Editor-only actions (require an active buffer)
            Action::MoveLeft
            | Action::MoveRight
            | Action::LineStart
            | Action::LineEnd
            | Action::InsertNewline
            | Action::DeleteBackward
            | Action::DeleteForward
            | Action::InsertTab
            | Action::KillLine
            | Action::Save => {
                if let Some(buf) = self.active_buffer_mut() {
                    match action {
                        Action::MoveLeft => buf.move_left(),
                        Action::MoveRight => buf.move_right(),
                        Action::LineStart => buf.move_to_line_start(),
                        Action::LineEnd => buf.move_to_line_end(),
                        Action::InsertNewline => buf.insert_newline(),
                        Action::DeleteBackward => buf.delete_char_backward(),
                        Action::DeleteForward => buf.delete_char_forward(),
                        Action::InsertTab => buf.insert_char('\t'),
                        Action::KillLine => buf.kill_line(),
                        Action::Save => match buf.save() {
                            Ok(()) => {
                                let name = buf.filename().to_string();
                                self.message = Some(format!("Saved {name}."));
                            }
                            Err(e) => self.message = Some(format!("Save failed: {e}")),
                        },
                        _ => unreachable!(),
                    }
                }
            }
            Action::OpenFile => {
                self.mode = Mode::Prompt {
                    label: "Open file: ".into(),
                    input: String::new(),
                    action: PromptAction::OpenFile,
                };
            }
        }
        InputResult::Continue
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

    pub fn restore_session(&mut self, session: SessionData) {
        self.buffers.clear();
        for bs in &session.buffers {
            let path_str = bs.file_path.to_string_lossy();
            if let Ok(mut buf) = Buffer::from_file(&path_str) {
                // Clamp cursor to valid ranges
                buf.cursor_row = buf.cursor_row.min(buf.lines.len().saturating_sub(1));
                buf.cursor_row = bs.cursor_row.min(buf.lines.len().saturating_sub(1));
                let line_len = buf.lines[buf.cursor_row].len();
                buf.cursor_col = bs.cursor_col.min(line_len);
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
