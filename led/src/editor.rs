use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::buffer::Buffer;
use crate::config::{Action, KeyCombo, Keymap, KeymapLookup};
use crate::file_browser::FileBrowser;

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
    Prompt { label: String, input: String, action: PromptAction },
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
    pub buffer: Buffer,
    pub scroll_offset: usize,
    pub message: Option<String>,
    chord: ChordState,
    mode: Mode,
    keymap: Keymap,
    pub focus: Focus,
    pub file_browser: FileBrowser,
    pub show_side_panel: bool,
}

impl Editor {
    pub fn new(buffer: Buffer, keymap: Keymap, root: PathBuf) -> Self {
        Self {
            buffer,
            scroll_offset: 0,
            message: None,
            chord: ChordState::None,
            mode: Mode::Normal,
            keymap,
            focus: Focus::Editor,
            file_browser: FileBrowser::new(root),
            show_side_panel: true,
        }
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
                let mode = std::mem::replace(
                    &mut self.mode,
                    Mode::Normal,
                );
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
                if self.focus == Focus::Editor {
                    let has_ctrl_alt = key.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT,
                    );
                    if let KeyCode::Char(c) = key.code {
                        if !has_ctrl_alt {
                            self.buffer.insert_char(c);
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
                        Focus::Browser => Focus::Editor,
                    };
                }
            }
            Action::ToggleSidePanel => {
                self.show_side_panel = !self.show_side_panel;
                if !self.show_side_panel {
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

            // Shared movement (routed by focus)
            Action::MoveUp => {
                if self.focus == Focus::Browser {
                    self.file_browser.move_up();
                } else {
                    self.buffer.move_up();
                }
            }
            Action::MoveDown => {
                if self.focus == Focus::Browser {
                    self.file_browser.move_down();
                } else {
                    self.buffer.move_down();
                }
            }

            // Editor-only actions
            Action::MoveLeft => self.buffer.move_left(),
            Action::MoveRight => self.buffer.move_right(),
            Action::LineStart => self.buffer.move_to_line_start(),
            Action::LineEnd => self.buffer.move_to_line_end(),
            Action::InsertNewline => self.buffer.insert_newline(),
            Action::DeleteBackward => self.buffer.delete_char_backward(),
            Action::DeleteForward => self.buffer.delete_char_forward(),
            Action::InsertTab => self.buffer.insert_char('\t'),
            Action::KillLine => self.buffer.kill_line(),
            Action::Save => self.save_file(),
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

    fn save_file(&mut self) {
        match self.buffer.save() {
            Ok(()) => self.message = Some(format!("Saved {}.", self.buffer.filename())),
            Err(e) => self.message = Some(format!("Save failed: {e}")),
        }
    }

    fn open_file(&mut self, path: &str) {
        match Buffer::from_file(path) {
            Ok(buf) => {
                self.message = Some(format!("Opened {}.", buf.filename()));
                self.buffer = buf;
                self.scroll_offset = 0;
            }
            Err(e) => self.message = Some(format!("Open failed: {e}")),
        }
    }
}
