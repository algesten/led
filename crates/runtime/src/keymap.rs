//! Key → `Command` mapping.
//!
//! Immutable for the process lifetime in M5; the keymap is built
//! once in `main.rs` (optionally by merging a TOML file onto
//! [`default_keymap`]) and passed to [`crate::run`] by reference.
//! Dispatch consults it on every keystroke — the hot path is a single
//! `HashMap::get`.
//!
//! The vocabulary is intentionally plain: `KeyEvent` keys,
//! [`Command`] values, string parsers on the side for config I/O and
//! diagnostics. Future modal editing / chord bindings / hot reload
//! slot in above this layer without touching the fundamental types.

use led_driver_terminal_core::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;

/// Every dispatch-level action the runtime knows about.
///
/// `InsertChar(char)` is the one variant that is not bindable from
/// config — it's produced by the printable-char fallback inside
/// `dispatch_key` when no binding matches and the key is a printable
/// character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Quit,
    TabNext,
    TabPrev,
    Save,
    CursorUp,
    CursorDown,
    CursorLeft,
    CursorRight,
    CursorLineStart,
    CursorLineEnd,
    CursorPageUp,
    CursorPageDown,
    InsertNewline,
    DeleteBack,
    DeleteForward,
    InsertChar(char),
}

/// Key → command binding set. Immutable during a run.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    bindings: HashMap<KeyEvent, Command>,
}

impl Keymap {
    pub fn empty() -> Self {
        Self {
            bindings: HashMap::new(),
        }
    }

    /// Bind a key string to a command. Panics on invalid key string —
    /// only called from the baked-in `default_keymap` where the
    /// strings are static.
    pub fn bind(&mut self, key: &str, cmd: Command) {
        let ev = parse_key(key).unwrap_or_else(|e| panic!("invalid default key `{key}`: {e}"));
        self.bindings.insert(ev, cmd);
    }

    /// Insert an already-parsed key → command binding. Used by the
    /// config loader where key strings may come from user input.
    pub fn insert(&mut self, key: KeyEvent, cmd: Command) {
        self.bindings.insert(key, cmd);
    }

    pub fn lookup(&self, key: &KeyEvent) -> Option<Command> {
        self.bindings.get(key).copied()
    }

    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}

/// Built-in keymap reproducing M1–M4 behaviour. The binary starts
/// from this; user config merges overrides on top.
pub fn default_keymap() -> Keymap {
    let mut m = Keymap::empty();
    m.bind("ctrl-c", Command::Quit);
    m.bind("ctrl-s", Command::Save);
    m.bind("tab", Command::TabNext);
    m.bind("shift-tab", Command::TabPrev);
    m.bind("backtab", Command::TabPrev);
    m.bind("up", Command::CursorUp);
    m.bind("down", Command::CursorDown);
    m.bind("left", Command::CursorLeft);
    m.bind("right", Command::CursorRight);
    m.bind("home", Command::CursorLineStart);
    m.bind("end", Command::CursorLineEnd);
    m.bind("pageup", Command::CursorPageUp);
    m.bind("pagedown", Command::CursorPageDown);
    m.bind("enter", Command::InsertNewline);
    m.bind("backspace", Command::DeleteBack);
    m.bind("delete", Command::DeleteForward);
    m
}

// ── Key-string parsing ─────────────────────────────────────────────────

/// Parse a dash-separated key string into a [`KeyEvent`].
///
/// Modifiers are case-insensitive and may appear in any order. The
/// final segment is the key code: a named key (`tab`, `pageup`, …),
/// a single printable character, or `f<N>` for function keys.
///
/// Uppercase ASCII letters (`"A"`) implicitly add the Shift modifier;
/// this matches what terminals emit.
pub fn parse_key(s: &str) -> Result<KeyEvent, String> {
    if s.is_empty() {
        return Err("empty key string".into());
    }
    let mut modifiers = KeyModifiers::NONE;
    let mut code: Option<KeyCode> = None;
    let parts: Vec<&str> = s.split('-').collect();
    let (tail, head) = parts
        .split_last()
        .expect("split returns at least one element");

    for part in head {
        let mods_for_part = match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "c" => KeyModifiers::CONTROL,
            "alt" | "meta" | "a" | "m" => KeyModifiers::ALT,
            "shift" | "s" => KeyModifiers::SHIFT,
            other => return Err(format!("unknown modifier `{other}` in `{s}`")),
        };
        modifiers = modifiers | mods_for_part;
    }

    // The final segment is the key name. Handle named keys, then
    // single characters.
    match tail.to_ascii_lowercase().as_str() {
        "tab" => code = Some(KeyCode::Tab),
        "backtab" => code = Some(KeyCode::BackTab),
        "enter" | "return" => code = Some(KeyCode::Enter),
        "backspace" => code = Some(KeyCode::Backspace),
        "delete" | "del" => code = Some(KeyCode::Delete),
        "esc" | "escape" => code = Some(KeyCode::Esc),
        "left" => code = Some(KeyCode::Left),
        "right" => code = Some(KeyCode::Right),
        "up" => code = Some(KeyCode::Up),
        "down" => code = Some(KeyCode::Down),
        "home" => code = Some(KeyCode::Home),
        "end" => code = Some(KeyCode::End),
        "pageup" | "pgup" => code = Some(KeyCode::PageUp),
        "pagedown" | "pgdn" => code = Some(KeyCode::PageDown),
        "space" | "spc" => code = Some(KeyCode::Char(' ')),
        fn_key if fn_key.starts_with('f') && fn_key.len() > 1 => {
            if let Ok(n) = fn_key[1..].parse::<u8>() {
                if (1..=24).contains(&n) {
                    code = Some(KeyCode::F(n));
                }
            }
        }
        _ => {}
    }

    if code.is_none() {
        // Try: single printable character (ASCII or a single Unicode
        // scalar). An uppercase ASCII letter implicitly adds SHIFT.
        let mut chars = tail.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => {
                if c.is_ascii_uppercase() {
                    modifiers = modifiers | KeyModifiers::SHIFT;
                    code = Some(KeyCode::Char(c.to_ascii_lowercase()));
                } else {
                    code = Some(KeyCode::Char(c));
                }
            }
            _ => return Err(format!("unknown key code `{tail}` in `{s}`")),
        }
    }

    let code = code.expect("set above or early-returned");
    Ok(KeyEvent { code, modifiers })
}

/// Render a [`KeyEvent`] back to the canonical string form so parser
/// errors and diagnostics round-trip with user input.
///
/// Canonical convention: `shift-` is folded into the char case for
/// ASCII alphabetic keys — so `{Char('k'), CTRL|SHIFT}` displays as
/// `ctrl-K`, not `ctrl-shift-k`. The parser accepts both forms and
/// produces the same `KeyEvent`.
pub fn key_string(k: &KeyEvent) -> String {
    let mut out = String::new();
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        out.push_str("ctrl-");
    }
    if k.modifiers.contains(KeyModifiers::ALT) {
        out.push_str("alt-");
    }
    match k.code {
        KeyCode::Char(c)
            if c.is_ascii_alphabetic() && k.modifiers.contains(KeyModifiers::SHIFT) =>
        {
            out.push(c.to_ascii_uppercase());
        }
        _ => {
            if k.modifiers.contains(KeyModifiers::SHIFT) {
                out.push_str("shift-");
            }
            out.push_str(&code_string(&k.code));
        }
    }
    out
}

fn code_string(c: &KeyCode) -> String {
    match c {
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(ch) => ch.to_string(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "backtab".into(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::F(n) => format!("f{n}"),
    }
}

// ── Command-string parsing ─────────────────────────────────────────────

/// Parse a dot-separated command string into a [`Command`].
///
/// Unknown strings are a parse error at config load time. `InsertChar`
/// is deliberately not reachable via this parser — it exists only as
/// the fallback path in dispatch.
pub fn parse_command(s: &str) -> Result<Command, String> {
    match s {
        "quit" => Ok(Command::Quit),
        "save" => Ok(Command::Save),
        "tab.next" => Ok(Command::TabNext),
        "tab.prev" => Ok(Command::TabPrev),
        "cursor.up" => Ok(Command::CursorUp),
        "cursor.down" => Ok(Command::CursorDown),
        "cursor.left" => Ok(Command::CursorLeft),
        "cursor.right" => Ok(Command::CursorRight),
        "cursor.line-start" => Ok(Command::CursorLineStart),
        "cursor.line-end" => Ok(Command::CursorLineEnd),
        "cursor.page-up" => Ok(Command::CursorPageUp),
        "cursor.page-down" => Ok(Command::CursorPageDown),
        "edit.insert-newline" => Ok(Command::InsertNewline),
        "edit.delete-back" => Ok(Command::DeleteBack),
        "edit.delete-forward" => Ok(Command::DeleteForward),
        other => Err(format!("unknown command `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(mods: KeyModifiers, code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
        }
    }

    // ── Key-string parsing ─────────────────────────────────────────────

    #[test]
    fn parse_simple_named_keys() {
        assert_eq!(parse_key("tab").unwrap(), ev(KeyModifiers::NONE, KeyCode::Tab));
        assert_eq!(parse_key("enter").unwrap(), ev(KeyModifiers::NONE, KeyCode::Enter));
        assert_eq!(parse_key("esc").unwrap(), ev(KeyModifiers::NONE, KeyCode::Esc));
        assert_eq!(parse_key("up").unwrap(), ev(KeyModifiers::NONE, KeyCode::Up));
        assert_eq!(parse_key("pageup").unwrap(), ev(KeyModifiers::NONE, KeyCode::PageUp));
    }

    #[test]
    fn parse_with_modifiers() {
        assert_eq!(
            parse_key("ctrl-s").unwrap(),
            ev(KeyModifiers::CONTROL, KeyCode::Char('s'))
        );
        assert_eq!(
            parse_key("shift-tab").unwrap(),
            ev(KeyModifiers::SHIFT, KeyCode::Tab)
        );
        assert_eq!(
            parse_key("ctrl-shift-k").unwrap(),
            ev(
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                KeyCode::Char('k')
            )
        );
        // Modifier order doesn't matter.
        assert_eq!(
            parse_key("shift-ctrl-k").unwrap(),
            ev(
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                KeyCode::Char('k')
            )
        );
    }

    #[test]
    fn parse_uppercase_letter_adds_shift() {
        assert_eq!(
            parse_key("A").unwrap(),
            ev(KeyModifiers::SHIFT, KeyCode::Char('a'))
        );
        assert_eq!(
            parse_key("ctrl-A").unwrap(),
            ev(
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                KeyCode::Char('a')
            )
        );
    }

    #[test]
    fn parse_function_keys() {
        assert_eq!(parse_key("f1").unwrap(), ev(KeyModifiers::NONE, KeyCode::F(1)));
        assert_eq!(parse_key("f12").unwrap(), ev(KeyModifiers::NONE, KeyCode::F(12)));
        assert_eq!(
            parse_key("ctrl-f5").unwrap(),
            ev(KeyModifiers::CONTROL, KeyCode::F(5))
        );
    }

    #[test]
    fn parse_space() {
        assert_eq!(
            parse_key("space").unwrap(),
            ev(KeyModifiers::NONE, KeyCode::Char(' '))
        );
    }

    #[test]
    fn parse_errors_are_actionable() {
        let e = parse_key("").unwrap_err();
        assert!(e.contains("empty"));
        let e = parse_key("qux-x").unwrap_err();
        assert!(e.contains("unknown modifier"));
        let e = parse_key("shift-qux").unwrap_err();
        assert!(e.contains("unknown key"));
    }

    #[test]
    fn key_string_round_trips() {
        let cases = [
            "tab",
            "shift-tab",
            "ctrl-s",
            "ctrl-shift-k",
            "enter",
            "backspace",
            "pageup",
            "f1",
            "ctrl-f12",
            "space",
            "a",
            "z",
        ];
        for c in cases {
            let k = parse_key(c).unwrap_or_else(|e| panic!("parse {c}: {e}"));
            let back = key_string(&k);
            assert_eq!(parse_key(&back).unwrap(), k, "round-trip {c} → {back}");
        }
    }

    // ── Command parsing ────────────────────────────────────────────────

    #[test]
    fn parse_all_known_commands() {
        let cases = [
            ("quit", Command::Quit),
            ("save", Command::Save),
            ("tab.next", Command::TabNext),
            ("tab.prev", Command::TabPrev),
            ("cursor.up", Command::CursorUp),
            ("cursor.down", Command::CursorDown),
            ("cursor.left", Command::CursorLeft),
            ("cursor.right", Command::CursorRight),
            ("cursor.line-start", Command::CursorLineStart),
            ("cursor.line-end", Command::CursorLineEnd),
            ("cursor.page-up", Command::CursorPageUp),
            ("cursor.page-down", Command::CursorPageDown),
            ("edit.insert-newline", Command::InsertNewline),
            ("edit.delete-back", Command::DeleteBack),
            ("edit.delete-forward", Command::DeleteForward),
        ];
        for (s, expected) in cases {
            assert_eq!(parse_command(s).unwrap(), expected, "command `{s}`");
        }
    }

    #[test]
    fn parse_command_rejects_unknown() {
        let err = parse_command("explode").unwrap_err();
        assert!(err.contains("unknown command"));
    }

    // ── Keymap ─────────────────────────────────────────────────────────

    #[test]
    fn default_keymap_contains_core_bindings() {
        let m = default_keymap();
        assert_eq!(m.lookup(&parse_key("ctrl-c").unwrap()), Some(Command::Quit));
        assert_eq!(m.lookup(&parse_key("ctrl-s").unwrap()), Some(Command::Save));
        assert_eq!(
            m.lookup(&parse_key("tab").unwrap()),
            Some(Command::TabNext)
        );
        assert_eq!(
            m.lookup(&parse_key("shift-tab").unwrap()),
            Some(Command::TabPrev)
        );
        assert_eq!(
            m.lookup(&parse_key("up").unwrap()),
            Some(Command::CursorUp)
        );
        assert_eq!(
            m.lookup(&parse_key("enter").unwrap()),
            Some(Command::InsertNewline)
        );
    }

    #[test]
    fn keymap_insert_overrides_existing() {
        let mut m = default_keymap();
        // User rebinds Ctrl-C to quit (no-op — already Quit), then
        // rebinds to Save for the sake of the test.
        m.insert(parse_key("ctrl-c").unwrap(), Command::Save);
        assert_eq!(m.lookup(&parse_key("ctrl-c").unwrap()), Some(Command::Save));
    }

    #[test]
    fn keymap_unknown_key_yields_none() {
        let m = default_keymap();
        assert_eq!(m.lookup(&parse_key("f7").unwrap()), None);
    }
}
