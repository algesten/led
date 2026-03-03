use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyModifiers};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Action
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    LineStart,
    LineEnd,
    InsertNewline,
    DeleteBackward,
    DeleteForward,
    InsertTab,
    KillLine,
    Save,
    Quit,
    ToggleFocus,
    ToggleSidePanel,
    ExpandDir,
    CollapseDir,
    OpenSelected,
    OpenSelectedBg,
    PrevTab,
    NextTab,
}

// ---------------------------------------------------------------------------
// KeyCombo
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyCombo {
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    /// Build a KeyCombo from a crossterm KeyEvent, stripping SHIFT for
    /// character keys (crossterm reports 'A' with SHIFT for uppercase).
    pub fn from_key_event(event: &crossterm::event::KeyEvent) -> Self {
        let mut mods = event.modifiers;
        if matches!(event.code, KeyCode::Char(_)) {
            mods.remove(KeyModifiers::SHIFT);
        }
        Self {
            code: event.code,
            modifiers: mods,
        }
    }
}

// ---------------------------------------------------------------------------
// Keymap
// ---------------------------------------------------------------------------

pub enum KeymapLookup {
    Action(Action),
    ChordPrefix,
    Unbound,
}

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
                if let Some(&action) = ctx_map.get(combo) {
                    return KeymapLookup::Action(action);
                }
            }
        }
        // Fall back to global
        if let Some(&action) = self.direct.get(combo) {
            KeymapLookup::Action(action)
        } else if self.chords.contains_key(combo) {
            KeymapLookup::ChordPrefix
        } else {
            KeymapLookup::Unbound
        }
    }

    pub fn lookup_chord(&self, prefix: &KeyCombo, combo: &KeyCombo) -> Option<Action> {
        self.chords.get(prefix).and_then(|m| m.get(combo)).copied()
    }
}

// ---------------------------------------------------------------------------
// Parsing key combo strings ("ctrl+a", "up", "enter", etc.)
// ---------------------------------------------------------------------------

fn parse_key_combo(s: &str) -> Result<KeyCombo, String> {
    let parts: Vec<&str> = s.split('+').collect();
    let mut modifiers = KeyModifiers::NONE;
    let key_part;

    // All parts except the last are modifiers
    let (mod_parts, key_parts) = parts.split_at(parts.len() - 1);
    key_part = key_parts[0];

    for &m in mod_parts {
        match m.to_lowercase().as_str() {
            "ctrl" => modifiers |= KeyModifiers::CONTROL,
            "alt" => modifiers |= KeyModifiers::ALT,
            "shift" => modifiers |= KeyModifiers::SHIFT,
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
        "enter" => KeyCode::Enter,
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "tab" => KeyCode::Tab,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        s if s.len() == 1 => KeyCode::Char(s.chars().next().unwrap()),
        other => return Err(format!("unknown key: {other}")),
    };

    Ok(KeyCombo::new(code, modifiers))
}

// ---------------------------------------------------------------------------
// Default config
// ---------------------------------------------------------------------------

pub const DEFAULT_KEYS_TOML: &str = r#"# led keybindings
# Format: "key" = "action"
# Modifiers: ctrl, alt, shift (e.g. "ctrl+a")
# Sub-tables define chord prefixes (e.g. [keys."ctrl+x"])

[keys]
"ctrl+a" = "line_start"
"ctrl+e" = "line_end"
"ctrl+k" = "kill_line"
"up" = "move_up"
"down" = "move_down"
"left" = "move_left"
"right" = "move_right"
"home" = "line_start"
"end" = "line_end"
"enter" = "insert_newline"
"backspace" = "delete_backward"
"delete" = "delete_forward"
"tab" = "insert_tab"
"ctrl+o" = "toggle_focus"
"ctrl+b" = "toggle_side_panel"
"ctrl+left" = "prev_tab"
"ctrl+right" = "next_tab"

[keys."ctrl+x"]
"ctrl+c" = "quit"
"ctrl+s" = "save"

[browser]
"left" = "collapse_dir"
"right" = "expand_dir"
"enter" = "open_selected"
"alt+enter" = "open_selected_bg"
"#;

// ---------------------------------------------------------------------------
// TOML → Keymap conversion
// ---------------------------------------------------------------------------

fn parse_flat_table(table: &toml::map::Map<String, toml::Value>) -> Result<HashMap<KeyCombo, Action>, String> {
    let mut map = HashMap::new();
    for (key_str, val) in table {
        let combo = parse_key_combo(key_str)?;
        let action_str = val.as_str()
            .ok_or(format!("expected string action for key \"{key_str}\""))?;
        let action: Action = Action::deserialize(val.clone())
            .map_err(|e| format!("unknown action \"{action_str}\": {e}"))?;
        map.insert(combo, action);
    }
    Ok(map)
}

fn toml_to_keymap(toml_str: &str) -> Result<Keymap, String> {
    let doc: toml::Value = toml::from_str(toml_str).map_err(|e| format!("TOML parse error: {e}"))?;
    let doc_table = doc.as_table().ok_or("expected top-level table")?;

    let keys_table = doc
        .get("keys")
        .and_then(|v| v.as_table())
        .ok_or("missing [keys] table")?;

    let mut direct = HashMap::new();
    let mut chords: HashMap<KeyCombo, HashMap<KeyCombo, Action>> = HashMap::new();

    for (key_str, value) in keys_table {
        let combo = parse_key_combo(key_str)?;

        match value {
            toml::Value::String(action_str) => {
                let action: Action = Action::deserialize(value.clone())
                    .map_err(|e| format!("unknown action \"{action_str}\": {e}"))?;
                direct.insert(combo, action);
            }
            toml::Value::Table(sub) => {
                let chord_map = parse_flat_table(sub)?;
                chords.insert(combo, chord_map);
            }
            _ => return Err(format!("unexpected value type for key \"{key_str}\"")),
        }
    }

    // Parse context sections (any top-level table other than "keys")
    let mut contexts = HashMap::new();
    for (section, value) in doc_table {
        if section == "keys" {
            continue;
        }
        if let Some(table) = value.as_table() {
            contexts.insert(section.clone(), parse_flat_table(table)?);
        }
    }

    Ok(Keymap { direct, chords, contexts })
}

// ---------------------------------------------------------------------------
// Load or create config
// ---------------------------------------------------------------------------

pub fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|d| d.join(".config").join("led").join("keys.toml"))
}

pub fn load_or_create_config() -> Result<Keymap, String> {
    let path = config_path().ok_or("could not determine config directory")?;

    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create config dir: {e}"))?;
        }
        fs::write(&path, DEFAULT_KEYS_TOML)
            .map_err(|e| format!("failed to write default config: {e}"))?;
        return toml_to_keymap(DEFAULT_KEYS_TOML);
    }

    let content = fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    toml_to_keymap(&content)
}

pub fn reset_config() -> Result<(), String> {
    let path = config_path().ok_or("could not determine config directory")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create config dir: {e}"))?;
    }
    fs::write(&path, DEFAULT_KEYS_TOML)
        .map_err(|e| format!("failed to write default config: {e}"))?;
    Ok(())
}

pub fn reload_keymap() -> Option<Keymap> {
    let path = config_path()?;
    let content = fs::read_to_string(&path).ok()?;
    toml_to_keymap(&content).ok()
}

pub fn default_keymap() -> Keymap {
    toml_to_keymap(DEFAULT_KEYS_TOML).expect("default config should always parse")
}
