use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::buffer::Buffer;
use crate::config::{Action, KeyCombo, Keymap, KeymapLookup};

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

pub struct Editor {
    pub buffer: Buffer,
    pub scroll_offset: usize,
    pub message: Option<String>,
    chord: ChordState,
    mode: Mode,
    keymap: Keymap,
}

impl Editor {
    pub fn new(buffer: Buffer, keymap: Keymap) -> Self {
        Self {
            buffer,
            scroll_offset: 0,
            message: None,
            chord: ChordState::None,
            mode: Mode::Normal,
            keymap,
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

        match self.keymap.lookup(&combo) {
            KeymapLookup::Action(action) => self.execute_action(action),
            KeymapLookup::ChordPrefix => {
                self.chord = ChordState::Pending(combo);
                self.message = None;
                InputResult::Continue
            }
            KeymapLookup::Unbound => {
                // Printable character fallback: insert if no ctrl/alt modifier
                let has_ctrl_alt = key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT,
                );
                if let KeyCode::Char(c) = key.code {
                    if !has_ctrl_alt {
                        self.buffer.insert_char(c);
                    }
                }
                InputResult::Continue
            }
        }
    }

    fn execute_action(&mut self, action: Action) -> InputResult {
        match action {
            Action::MoveUp => self.buffer.move_up(),
            Action::MoveDown => self.buffer.move_down(),
            Action::MoveLeft => self.buffer.move_left(),
            Action::MoveRight => self.buffer.move_right(),
            Action::LineStart => self.buffer.move_to_line_start(),
            Action::LineEnd => self.buffer.move_to_line_end(),
            Action::InsertNewline => self.buffer.insert_newline(),
            Action::DeleteBackward => self.buffer.delete_char_backward(),
            Action::DeleteForward => self.buffer.delete_char_forward(),
            Action::InsertTab => self.buffer.insert_char('\t'),
            Action::KillLine => self.buffer.kill_line(),
            Action::Save => {
                self.save_file();
                return InputResult::Continue;
            }
            Action::OpenFile => {
                self.mode = Mode::Prompt {
                    label: "Open file: ".into(),
                    input: String::new(),
                    action: PromptAction::OpenFile,
                };
                return InputResult::Continue;
            }
            Action::Quit => return InputResult::Quit,
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
