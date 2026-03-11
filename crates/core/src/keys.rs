use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

use crate::Action;

// ============================================================================
// KeyCombo
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl KeyCombo {
    pub fn from_key_event(event: KeyEvent) -> Self {
        let KeyEvent {
            code, modifiers, ..
        } = event;

        // Strip SHIFT from character keys — the character itself already includes shift
        let (code, shift) = match code {
            KeyCode::Char(c) => (KeyCode::Char(c), false),
            _ => (code, modifiers.contains(KeyModifiers::SHIFT)),
        };

        KeyCombo {
            code,
            ctrl: modifiers.contains(KeyModifiers::CONTROL),
            alt: modifiers.contains(KeyModifiers::ALT),
            shift,
        }
    }

    pub fn display_name(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl-");
        }
        if self.alt {
            s.push_str("Alt-");
        }
        if self.shift {
            s.push_str("Shift-");
        }
        match self.code {
            KeyCode::Char(' ') => s.push_str("Space"),
            KeyCode::Char(c) => {
                for ch in c.to_uppercase() {
                    s.push(ch);
                }
            }
            KeyCode::Up => s.push('\u{2191}'),
            KeyCode::Down => s.push('\u{2193}'),
            KeyCode::Left => s.push('\u{2190}'),
            KeyCode::Right => s.push('\u{2192}'),
            KeyCode::Home => s.push_str("Home"),
            KeyCode::End => s.push_str("End"),
            KeyCode::PageUp => s.push_str("PageUp"),
            KeyCode::PageDown => s.push_str("PageDown"),
            KeyCode::Enter => s.push_str("Enter"),
            KeyCode::Backspace => s.push_str("Backspace"),
            KeyCode::Delete => s.push_str("Delete"),
            KeyCode::Tab => s.push_str("Tab"),
            KeyCode::Esc => s.push_str("Esc"),
            other => {
                let d = format!("{other:?}");
                s.push_str(&d);
            }
        }
        s
    }
}

// ============================================================================
// Keymap
// ============================================================================

#[derive(Debug, Clone)]
pub enum KeymapLookup {
    Action(Action),
    ChordPrefix,
    Unbound,
}

#[derive(Debug, Clone)]
pub struct Keymap {
    direct: HashMap<KeyCombo, Action>,
    chords: HashMap<KeyCombo, HashMap<KeyCombo, Action>>,
    contexts: HashMap<String, HashMap<KeyCombo, Action>>,
}

impl Keymap {
    pub fn lookup(&self, combo: &KeyCombo, context: Option<&str>) -> KeymapLookup {
        // Check context-specific bindings first
        if let Some(ctx) = context {
            if let Some(ctx_map) = self.contexts.get(ctx) {
                if let Some(action) = ctx_map.get(combo) {
                    return KeymapLookup::Action(action.clone());
                }
            }
        }
        // Fall back to global
        if let Some(action) = self.direct.get(combo) {
            KeymapLookup::Action(action.clone())
        } else if self.chords.contains_key(combo) {
            KeymapLookup::ChordPrefix
        } else {
            KeymapLookup::Unbound
        }
    }

    pub fn lookup_chord(&self, prefix: &KeyCombo, combo: &KeyCombo) -> Option<Action> {
        self.chords.get(prefix).and_then(|m| m.get(combo)).cloned()
    }
}

// ============================================================================
// TOML deserialization types
// ============================================================================

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum KeyBinding {
    Action(String),
    Chord(HashMap<String, String>),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Keys {
    #[serde(default)]
    pub keys: HashMap<String, KeyBinding>,
    #[serde(default)]
    pub browser: HashMap<String, String>,
    #[serde(default)]
    pub file_search: HashMap<String, String>,
}

// ============================================================================
// Parsing
// ============================================================================

impl Keys {
    pub fn into_keymap(self) -> Result<Keymap, String> {
        let mut direct = HashMap::new();
        let mut chords: HashMap<KeyCombo, HashMap<KeyCombo, Action>> = HashMap::new();

        for (key_str, value) in &self.keys {
            let combo = parse_key_combo(key_str)?;
            match value {
                KeyBinding::Action(action_str) => {
                    let action = parse_action(action_str)?;
                    direct.insert(combo, action);
                }
                KeyBinding::Chord(sub) => {
                    let chord_map = parse_flat_table(sub)?;
                    chords.insert(combo, chord_map);
                }
            }
        }

        let mut contexts = HashMap::new();
        if !self.browser.is_empty() {
            contexts.insert("browser".to_string(), parse_flat_table(&self.browser)?);
        }
        if !self.file_search.is_empty() {
            contexts.insert(
                "file_search".to_string(),
                parse_flat_table(&self.file_search)?,
            );
        }

        Ok(Keymap {
            direct,
            chords,
            contexts,
        })
    }
}

fn parse_flat_table(table: &HashMap<String, String>) -> Result<HashMap<KeyCombo, Action>, String> {
    let mut map = HashMap::new();
    for (key_str, action_str) in table {
        let combo = parse_key_combo(key_str)?;
        let action = parse_action(action_str)?;
        map.insert(combo, action);
    }
    Ok(map)
}

fn parse_action(s: &str) -> Result<Action, String> {
    let json = toml::Value::String(s.to_string());
    Action::deserialize(json).map_err(|e| format!("unknown action \"{s}\": {e}"))
}

pub fn parse_key_combo(s: &str) -> Result<KeyCombo, String> {
    let parts: Vec<&str> = s.split('+').collect();
    let (mod_parts, key_parts) = parts.split_at(parts.len() - 1);
    let key_part = key_parts[0];

    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;

    for &m in mod_parts {
        match m.to_lowercase().as_str() {
            "ctrl" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            other => return Err(format!("unknown modifier: {other}")),
        }
    }

    let code = match key_part.to_lowercase().as_str() {
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "enter" => KeyCode::Enter,
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "tab" => KeyCode::Tab,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        s if s.len() == 1 => KeyCode::Char(s.chars().next().unwrap()),
        other => return Err(format!("unknown key: {other}")),
    };

    Ok(KeyCombo {
        code,
        ctrl,
        alt,
        shift,
    })
}
