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
    // Lifecycle
    Quit,
    Abort,

    // Tab management
    TabNext,
    TabPrev,
    KillBuffer,

    // Save variants
    Save,
    SaveAll,
    SaveNoFormat,

    // Cursor
    CursorUp,
    CursorDown,
    CursorLeft,
    CursorRight,
    CursorLineStart,
    CursorLineEnd,
    CursorPageUp,
    CursorPageDown,
    CursorFileStart,
    CursorFileEnd,
    CursorWordLeft,
    CursorWordRight,

    // Editing
    InsertNewline,
    DeleteBack,
    DeleteForward,
    InsertChar(char),

    // Mark / region / kill ring (M7).
    SetMark,
    KillRegion,
    KillLine,
    Yank,

    // Undo / redo (M8).
    Undo,
    Redo,

    // Navigation (M10).
    JumpBack,
    JumpForward,
    MatchBracket,
}

/// Two-level key → command binding set. `direct` maps single keys to
/// commands; `chords` maps a prefix key to a nested table mapping
/// the second key to a command. Disjoint: a key in `direct` shadows
/// any chord entry with the same prefix (legacy behaviour).
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    direct: HashMap<KeyEvent, Command>,
    chords: HashMap<KeyEvent, HashMap<KeyEvent, Command>>,
}

impl Keymap {
    pub fn empty() -> Self {
        Self {
            direct: HashMap::new(),
            chords: HashMap::new(),
        }
    }

    /// Bind a single key to a command. Panics on invalid key string —
    /// only called from the baked-in `default_keymap` where the
    /// strings are static.
    pub fn bind(&mut self, key: &str, cmd: Command) {
        let ev = parse_key(key).unwrap_or_else(|e| panic!("invalid default key `{key}`: {e}"));
        self.direct.insert(ev, cmd);
    }

    /// Bind a two-key chord (`prefix` then `second`) to a command.
    /// Panics on invalid strings — caller beware, only for the baked
    /// defaults.
    pub fn bind_chord(&mut self, prefix: &str, second: &str, cmd: Command) {
        let p = parse_key(prefix)
            .unwrap_or_else(|e| panic!("invalid chord prefix `{prefix}`: {e}"));
        let s = parse_key(second)
            .unwrap_or_else(|e| panic!("invalid chord second `{second}`: {e}"));
        self.chords.entry(p).or_default().insert(s, cmd);
    }

    /// Insert an already-parsed direct binding. Used by the config
    /// loader where key strings may come from user input.
    pub fn insert_direct(&mut self, key: KeyEvent, cmd: Command) {
        self.direct.insert(key, cmd);
    }

    /// Insert an already-parsed chord binding.
    pub fn insert_chord(&mut self, prefix: KeyEvent, second: KeyEvent, cmd: Command) {
        self.chords.entry(prefix).or_default().insert(second, cmd);
    }

    pub fn lookup_direct(&self, key: &KeyEvent) -> Option<Command> {
        self.direct.get(key).copied()
    }

    pub fn lookup_chord(&self, prefix: &KeyEvent, second: &KeyEvent) -> Option<Command> {
        self.chords.get(prefix)?.get(second).copied()
    }

    /// Does `key` begin a chord? A direct binding for the same key
    /// shadows its chord table — `is_prefix` returns false then, so
    /// dispatch routes through `lookup_direct`.
    pub fn is_prefix(&self, key: &KeyEvent) -> bool {
        !self.direct.contains_key(key) && self.chords.contains_key(key)
    }

    /// Backward-compat shim for M5 callers that only used direct
    /// bindings (dispatch fallback path, tests that pre-date chords).
    pub fn lookup(&self, key: &KeyEvent) -> Option<Command> {
        self.lookup_direct(key)
    }

    pub fn is_empty(&self) -> bool {
        self.direct.is_empty() && self.chords.is_empty()
    }
}

/// Runtime-only chord state. Carries a pending prefix between
/// dispatch ticks. Not a drv source — lives in the `run` frame,
/// threaded through `dispatch_key` by `&mut`.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChordState {
    pub pending: Option<KeyEvent>,
}

/// Built-in keymap matching legacy `default_keys.toml` for every
/// command the rewrite implements so far. User config merges
/// overrides on top.
///
/// Bindings deliberately omitted from M6 because their feature isn't
/// implemented yet (tracked in `docs/rewrite/ROADMAP.md`):
///
/// - `tab = "insert_tab"` — M23 (auto-indent). We keep `tab =
///   "next_tab"` as a placeholder so tab switching still works.
/// - `ctrl+f`, `ctrl+r`, `ctrl+b`, `ctrl+t`, `alt+tab`, `alt+.`,
///   `alt+,`, `alt+]`, `alt+enter`, `alt+i`, `alt+o`, `ctrl+space`,
///   `ctrl+w`, `ctrl+y`, `ctrl+k`, `ctrl+/`, `ctrl+_`, `ctrl+7`,
///   `ctrl+z`, `ctrl+q`, `ctrl+x ctrl+f`, `ctrl+x ctrl+w`,
///   `ctrl+x ctrl+p`, `ctrl+x i`, `ctrl+x (`, `ctrl+x )`,
///   `ctrl+x e`, `ctrl+h e` — each lands in its feature milestone.
pub fn default_keymap() -> Keymap {
    let mut m = Keymap::empty();

    // Cursor movement (implemented by M2 / M6).
    m.bind("up", Command::CursorUp);
    m.bind("down", Command::CursorDown);
    m.bind("left", Command::CursorLeft);
    m.bind("right", Command::CursorRight);
    m.bind("home", Command::CursorLineStart);
    m.bind("end", Command::CursorLineEnd);
    m.bind("pageup", Command::CursorPageUp);
    m.bind("pagedown", Command::CursorPageDown);
    m.bind("ctrl+a", Command::CursorLineStart);
    m.bind("ctrl+e", Command::CursorLineEnd);
    m.bind("alt+v", Command::CursorPageUp);
    m.bind("ctrl+v", Command::CursorPageDown);
    m.bind("ctrl+home", Command::CursorFileStart);
    m.bind("ctrl+end", Command::CursorFileEnd);
    m.bind("alt+<", Command::CursorFileStart);
    m.bind("alt+>", Command::CursorFileEnd);

    // Navigation (M10). Legacy binds alt+b/f + alt+left/right to jump
    // back/forward, NOT to word motion — the CursorWordLeft /
    // CursorWordRight commands stay available for users who want to
    // bind them in their own keys.toml.
    m.bind("alt+b", Command::JumpBack);
    m.bind("alt+left", Command::JumpBack);
    m.bind("alt+f", Command::JumpForward);
    m.bind("alt+right", Command::JumpForward);
    m.bind("alt+]", Command::MatchBracket);

    // Tab management.
    m.bind("ctrl+left", Command::TabPrev);
    m.bind("ctrl+right", Command::TabNext);
    // Placeholder until insert_tab (M23). Tab cycling is a convenient
    // alias even after auto-indent lands.
    m.bind("tab", Command::TabNext);
    m.bind("shift+tab", Command::TabPrev);
    m.bind("backtab", Command::TabPrev);

    // Editing.
    m.bind("enter", Command::InsertNewline);
    m.bind("backspace", Command::DeleteBack);
    m.bind("delete", Command::DeleteForward);
    m.bind("ctrl+d", Command::DeleteForward);

    // Abort (modal overlays override behaviour in later milestones).
    m.bind("esc", Command::Abort);
    m.bind("ctrl+g", Command::Abort);

    // Mark / region / kill ring.
    m.bind("ctrl+space", Command::SetMark);
    m.bind("ctrl+w", Command::KillRegion);
    m.bind("ctrl+k", Command::KillLine);
    m.bind("ctrl+y", Command::Yank);

    // Undo — legacy binds three aliases because `ctrl+/` emits
    // different byte sequences on different terminals. Redo is
    // deliberately unbound by default (Emacs tradition; rebind via
    // keys.toml).
    m.bind("ctrl+/", Command::Undo);
    m.bind("ctrl+_", Command::Undo);
    m.bind("ctrl+7", Command::Undo);

    // File-write + buffer-management chords (ctrl+x prefix).
    m.bind_chord("ctrl+x", "ctrl+s", Command::Save);
    m.bind_chord("ctrl+x", "ctrl+c", Command::Quit);
    m.bind_chord("ctrl+x", "ctrl+a", Command::SaveAll);
    m.bind_chord("ctrl+x", "ctrl+d", Command::SaveNoFormat);
    m.bind_chord("ctrl+x", "k", Command::KillBuffer);

    m
}

// ── Key-string parsing ─────────────────────────────────────────────────

/// Parse a `+`-separated key string into a [`KeyEvent`].
///
/// Modifiers are case-insensitive and may appear in any order. The
/// final segment is the key code: a named key (`tab`, `pageup`, …),
/// a single printable character, or `f<N>` for function keys.
///
/// Uppercase ASCII letters (`"A"`) implicitly add the Shift modifier;
/// this matches what terminals emit.
///
/// Legacy led uses `+` as the separator, so we do too. `-` is also
/// accepted for dash-style config files — segments are split on
/// either character.
pub fn parse_key(s: &str) -> Result<KeyEvent, String> {
    if s.is_empty() {
        return Err("empty key string".into());
    }
    let mut modifiers = KeyModifiers::NONE;
    let mut code: Option<KeyCode> = None;
    let parts: Vec<&str> = s.split(['+', '-']).collect();
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
            if let Ok(n) = fn_key[1..].parse::<u8>()
                && (1..=24).contains(&n)
            {
                code = Some(KeyCode::F(n));
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
        out.push_str("ctrl+");
    }
    if k.modifiers.contains(KeyModifiers::ALT) {
        out.push_str("alt+");
    }
    match k.code {
        KeyCode::Char(c)
            if c.is_ascii_alphabetic() && k.modifiers.contains(KeyModifiers::SHIFT) =>
        {
            out.push(c.to_ascii_uppercase());
        }
        _ => {
            if k.modifiers.contains(KeyModifiers::SHIFT) {
                out.push_str("shift+");
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

/// Parse a snake-case command string into a [`Command`].
///
/// Names match legacy led's Action enum: `move_up`, `line_start`,
/// `delete_backward`, etc. — so user `keys.toml` files port over
/// unchanged. Unknown strings are a parse error at config load time.
/// `InsertChar` is deliberately not reachable via this parser — it
/// exists only as the fallback path in dispatch.
pub fn parse_command(s: &str) -> Result<Command, String> {
    match s {
        "quit" => Ok(Command::Quit),
        "abort" => Ok(Command::Abort),
        "save" => Ok(Command::Save),
        "save_all" => Ok(Command::SaveAll),
        "save_no_format" => Ok(Command::SaveNoFormat),
        "next_tab" => Ok(Command::TabNext),
        "prev_tab" => Ok(Command::TabPrev),
        "kill_buffer" => Ok(Command::KillBuffer),
        "move_up" => Ok(Command::CursorUp),
        "move_down" => Ok(Command::CursorDown),
        "move_left" => Ok(Command::CursorLeft),
        "move_right" => Ok(Command::CursorRight),
        "line_start" => Ok(Command::CursorLineStart),
        "line_end" => Ok(Command::CursorLineEnd),
        "page_up" => Ok(Command::CursorPageUp),
        "page_down" => Ok(Command::CursorPageDown),
        "file_start" => Ok(Command::CursorFileStart),
        "file_end" => Ok(Command::CursorFileEnd),
        "word_left" => Ok(Command::CursorWordLeft),
        "word_right" => Ok(Command::CursorWordRight),
        "insert_newline" => Ok(Command::InsertNewline),
        "delete_backward" => Ok(Command::DeleteBack),
        "delete_forward" => Ok(Command::DeleteForward),
        "set_mark" => Ok(Command::SetMark),
        "kill_region" => Ok(Command::KillRegion),
        "kill_line" => Ok(Command::KillLine),
        "yank" => Ok(Command::Yank),
        "undo" => Ok(Command::Undo),
        "redo" => Ok(Command::Redo),
        "jump_back" => Ok(Command::JumpBack),
        "jump_forward" => Ok(Command::JumpForward),
        "match_bracket" => Ok(Command::MatchBracket),
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
            ("next_tab", Command::TabNext),
            ("prev_tab", Command::TabPrev),
            ("move_up", Command::CursorUp),
            ("move_down", Command::CursorDown),
            ("move_left", Command::CursorLeft),
            ("move_right", Command::CursorRight),
            ("line_start", Command::CursorLineStart),
            ("line_end", Command::CursorLineEnd),
            ("page_up", Command::CursorPageUp),
            ("page_down", Command::CursorPageDown),
            ("insert_newline", Command::InsertNewline),
            ("delete_backward", Command::DeleteBack),
            ("delete_forward", Command::DeleteForward),
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
        // Direct bindings.
        assert_eq!(
            m.lookup_direct(&parse_key("up").unwrap()),
            Some(Command::CursorUp)
        );
        assert_eq!(
            m.lookup_direct(&parse_key("enter").unwrap()),
            Some(Command::InsertNewline)
        );
        assert_eq!(
            m.lookup_direct(&parse_key("esc").unwrap()),
            Some(Command::Abort)
        );
        // Chord bindings — save + quit moved under ctrl+x.
        assert_eq!(
            m.lookup_chord(&parse_key("ctrl+x").unwrap(), &parse_key("ctrl+s").unwrap()),
            Some(Command::Save)
        );
        assert_eq!(
            m.lookup_chord(&parse_key("ctrl+x").unwrap(), &parse_key("ctrl+c").unwrap()),
            Some(Command::Quit)
        );
        assert_eq!(
            m.lookup_chord(&parse_key("ctrl+x").unwrap(), &parse_key("k").unwrap()),
            Some(Command::KillBuffer)
        );
        // ctrl+x is a prefix, not a direct binding.
        assert!(m.is_prefix(&parse_key("ctrl+x").unwrap()));
        assert_eq!(m.lookup_direct(&parse_key("ctrl+x").unwrap()), None);
        // Plain ctrl+c / ctrl+s are UNBOUND at the root (legacy parity).
        assert_eq!(m.lookup_direct(&parse_key("ctrl+c").unwrap()), None);
        assert_eq!(m.lookup_direct(&parse_key("ctrl+s").unwrap()), None);
    }

    #[test]
    fn keymap_insert_overrides_existing() {
        let mut m = default_keymap();
        // Rebind tab (was TabNext in M5 default) to Save for the test.
        m.insert_direct(parse_key("tab").unwrap(), Command::Save);
        assert_eq!(
            m.lookup_direct(&parse_key("tab").unwrap()),
            Some(Command::Save)
        );
    }

    #[test]
    fn keymap_unknown_key_yields_none() {
        let m = default_keymap();
        assert_eq!(m.lookup_direct(&parse_key("f7").unwrap()), None);
    }
}
