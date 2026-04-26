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
    /// POSIX-stop the process (SIGTSTP). `fg` resumes in place
    /// with a full redraw. Default binding: `ctrl+z`.
    Suspend,

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

    // Tiered issue navigation (M20a) — Alt-./Alt-, cycles
    // LSP errors → warnings → git hunks, staying inside the
    // first non-empty tier.
    NextIssue,
    PrevIssue,

    // File browser (M11).
    ExpandDir,
    CollapseDir,
    CollapseAll,
    OpenSelected,
    OpenSelectedBg,
    ToggleSidePanel,
    ToggleFocus,

    // Find-file / save-as overlay (M12).
    FindFile,
    SaveAs,
    /// `Tab` inside the find-file overlay: complete to the single
    /// match, descend into a dir, or extend input to the longest
    /// common prefix across multiple matches. Only reachable via the
    /// `[find_file]` keymap context — outside that context `Tab` is
    /// reserved for `InsertTab` (M23).
    FindFileTabComplete,

    // In-buffer incremental search (M13). `InBufferSearch` both
    // starts a fresh isearch and advances to the next match when
    // already active — see `docs/spec/search.md`.
    InBufferSearch,

    // Project-wide file search (M14). `OpenFileSearch` opens the
    // sidebar overlay; `CloseFileSearch` exits. Toggles flip the
    // three mode switches shown in the header; `ReplaceAll` is the
    // bulk-replace commit.
    OpenFileSearch,
    CloseFileSearch,
    ToggleSearchCase,
    ToggleSearchRegex,
    ToggleSearchReplace,
    ReplaceAll,

    // LSP extras (M18).
    /// `textDocument/definition` for the identifier at the
    /// cursor; jumps the active tab (opens one if needed) to
    /// the response location. Records a jump-list entry so
    /// `JumpBack` round-trips.
    LspGotoDefinition,
    /// Open the rename overlay seeded with the identifier under
    /// the cursor. Typing edits the new name; Enter submits,
    /// Esc aborts.
    LspRename,
    /// Request `textDocument/codeAction` for the cursor (or
    /// mark..cursor selection); response opens a picker overlay.
    LspCodeAction,
    /// Toggle LSP inlay-hint rendering. When on, the runtime
    /// requests hints for visible buffers and stashes them
    /// per-buffer for the painter.
    LspToggleInlayHints,
    /// Explicit `textDocument/formatting` request. Applies the
    /// returned edits to the active buffer but does NOT save.
    /// `Save` (ctrl+x ctrl+s) invokes format first then saves.
    LspFormat,
    /// Outline navigation (legacy orphan). Bound by default
    /// to `alt+o`; no handler yet — stage 7 reserves the key
    /// so pressing it doesn't fall through to `InsertChar('o')`.
    /// Full outline (via `textDocument/documentSymbol`) lands
    /// in a later polish pass.
    Outline,
}

/// Two-level key → command binding set. `direct` maps single keys to
/// commands; `chords` maps a prefix key to a nested table mapping
/// the second key to a command. Disjoint: a key in `direct` shadows
/// any chord entry with the same prefix (legacy behaviour).
///
/// `browser_direct` is the context overlay active when focus is on
/// the file-browser sidebar (M11). `find_file_direct` is the overlay
/// active while the find-file / save-as modal is open (M12). Both
/// shadow global `direct` and never carry chords — matches legacy.
///
/// Lookup order is conditional: at most one overlay is active at a
/// time (browser focus and find-file are mutually exclusive — the
/// overlay runs with editor focus).
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    direct: HashMap<KeyEvent, Command>,
    chords: HashMap<KeyEvent, HashMap<KeyEvent, Command>>,
    browser_direct: HashMap<KeyEvent, Command>,
    find_file_direct: HashMap<KeyEvent, Command>,
    file_search_direct: HashMap<KeyEvent, Command>,
}

impl Keymap {
    pub fn empty() -> Self {
        Self {
            direct: HashMap::new(),
            chords: HashMap::new(),
            browser_direct: HashMap::new(),
            find_file_direct: HashMap::new(),
            file_search_direct: HashMap::new(),
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

    /// Drop a direct binding (if any). Used by the config loader
    /// when a user chord table shadows a default direct binding —
    /// the key should behave as a chord prefix only.
    pub fn remove_direct(&mut self, key: &KeyEvent) {
        self.direct.remove(key);
    }

    /// Insert an already-parsed chord binding.
    pub fn insert_chord(&mut self, prefix: KeyEvent, second: KeyEvent, cmd: Command) {
        self.chords.entry(prefix).or_default().insert(second, cmd);
    }

    pub fn lookup_direct(&self, key: &KeyEvent) -> Option<Command> {
        self.direct.get(key).copied()
    }

    /// Browser-context lookup. Returns `Some` only when the key is
    /// explicitly bound in the browser overlay.
    pub fn lookup_browser(&self, key: &KeyEvent) -> Option<Command> {
        self.browser_direct.get(key).copied()
    }

    /// Bind a key in the browser-context overlay. Panics on invalid
    /// key string; only called from `default_keymap` with static
    /// strings.
    pub fn bind_browser(&mut self, key: &str, cmd: Command) {
        let ev = parse_key(key).unwrap_or_else(|e| panic!("invalid default key `{key}`: {e}"));
        self.browser_direct.insert(ev, cmd);
    }

    pub fn insert_browser(&mut self, key: KeyEvent, cmd: Command) {
        self.browser_direct.insert(key, cmd);
    }

    /// Find-file overlay lookup. Returns `Some` only when the key is
    /// explicitly bound in the find-file context.
    pub fn lookup_find_file(&self, key: &KeyEvent) -> Option<Command> {
        self.find_file_direct.get(key).copied()
    }

    /// Bind a key in the find-file context overlay. Panics on invalid
    /// string; only called from `default_keymap` with static strings.
    pub fn bind_find_file(&mut self, key: &str, cmd: Command) {
        let ev = parse_key(key).unwrap_or_else(|e| panic!("invalid default key `{key}`: {e}"));
        self.find_file_direct.insert(ev, cmd);
    }

    pub fn insert_find_file(&mut self, key: KeyEvent, cmd: Command) {
        self.find_file_direct.insert(key, cmd);
    }

    /// File-search overlay lookup. Returns `Some` only when the key is
    /// explicitly bound in the file-search context.
    pub fn lookup_file_search(&self, key: &KeyEvent) -> Option<Command> {
        self.file_search_direct.get(key).copied()
    }

    /// Bind a key in the file-search overlay context. Panics on
    /// invalid string; only called from `default_keymap` with static
    /// strings.
    pub fn bind_file_search(&mut self, key: &str, cmd: Command) {
        let ev = parse_key(key).unwrap_or_else(|e| panic!("invalid default key `{key}`: {e}"));
        self.file_search_direct.insert(ev, cmd);
    }

    pub fn insert_file_search(&mut self, key: KeyEvent, cmd: Command) {
        self.file_search_direct.insert(key, cmd);
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

    /// Iterate all file-search context bindings. Used by the config
    /// loader to project user overrides into the overlay map.
    pub fn file_search_iter(&self) -> impl Iterator<Item = (&KeyEvent, &Command)> {
        self.file_search_direct.iter()
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

    // Issue navigation (M20a). The cycle stays inside the
    // first non-empty tier of `IssueCategory::NAV_LEVELS`.
    m.bind("alt+.", Command::NextIssue);
    m.bind("alt+,", Command::PrevIssue);

    // File browser (M11).
    m.bind("ctrl+b", Command::ToggleSidePanel);
    m.bind("alt+tab", Command::ToggleFocus);
    m.bind_browser("up", Command::CursorUp);
    m.bind_browser("down", Command::CursorDown);
    m.bind_browser("pageup", Command::CursorPageUp);
    m.bind_browser("pagedown", Command::CursorPageDown);
    m.bind_browser("ctrl+home", Command::CursorFileStart);
    m.bind_browser("ctrl+end", Command::CursorFileEnd);
    m.bind_browser("left", Command::CollapseDir);
    m.bind_browser("right", Command::ExpandDir);
    m.bind_browser("enter", Command::OpenSelected);
    m.bind_browser("alt+enter", Command::OpenSelectedBg);
    m.bind_browser("ctrl+q", Command::CollapseAll);

    // Tab management. `tab` itself is reserved for `insert_tab`
    // (M23); legacy never cycles buffers with plain Tab in the
    // editor. Shift-Tab / BackTab stay bound to `prev_tab` because
    // most terminals emit one of those encodings for Shift-Tab.
    m.bind("ctrl+left", Command::TabPrev);
    m.bind("ctrl+right", Command::TabNext);
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

    // Suspend (SIGTSTP). Single-key `ctrl+z` — matches legacy.
    m.bind("ctrl+z", Command::Suspend);

    // File-write + buffer-management chords (ctrl+x prefix).
    m.bind_chord("ctrl+x", "ctrl+s", Command::Save);
    m.bind_chord("ctrl+x", "ctrl+c", Command::Quit);
    m.bind_chord("ctrl+x", "ctrl+a", Command::SaveAll);
    m.bind_chord("ctrl+x", "ctrl+d", Command::SaveNoFormat);
    m.bind_chord("ctrl+x", "k", Command::KillBuffer);

    // Find-file / save-as (M12).
    m.bind_chord("ctrl+x", "ctrl+f", Command::FindFile);
    m.bind_chord("ctrl+x", "ctrl+w", Command::SaveAs);
    // Tab completion inside the overlay. Outside the overlay `Tab`
    // is unbound (reserved for `InsertTab` in M23).
    m.bind_find_file("tab", Command::FindFileTabComplete);

    // In-buffer isearch (M13). Same binding starts and advances.
    m.bind("ctrl+s", Command::InBufferSearch);

    // Project-wide file search (M14). Ctrl+f toggles the overlay;
    // toggles fire while the overlay has focus.
    m.bind("ctrl+f", Command::OpenFileSearch);
    m.bind("alt+1", Command::ToggleSearchCase);
    m.bind("alt+2", Command::ToggleSearchRegex);
    m.bind("alt+3", Command::ToggleSearchReplace);
    // `alt+enter` globally is LSP goto-definition (M18). Inside
    // the file-search overlay it maps to `ReplaceAll` via the
    // context overlay below — matches legacy's context-keyed
    // override.
    m.bind_file_search("alt+enter", Command::ReplaceAll);
    // Tab inside the overlay cycles SearchInput → ReplaceInput →
    // result rows (same direction as Down-arrow). Outside the
    // overlay, `tab` stays unbound (reserved for M23 `insert_tab`).
    m.bind_file_search("tab", Command::CursorDown);

    // LSP extras (M18). The browser overlay also claims
    // `alt+enter` for "open selected in background" — it sits
    // in `browser_direct` and its lookup runs before global
    // `direct`, so browser context wins there without an
    // explicit shadow here.
    m.bind("alt+enter", Command::LspGotoDefinition);
    m.bind("ctrl+r", Command::LspRename);
    m.bind("alt+i", Command::LspCodeAction);
    m.bind("ctrl+t", Command::LspToggleInlayHints);
    m.bind("alt+o", Command::Outline);

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
        "suspend" => Ok(Command::Suspend),
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
        "next_issue" => Ok(Command::NextIssue),
        "prev_issue" => Ok(Command::PrevIssue),
        "expand_dir" => Ok(Command::ExpandDir),
        "collapse_dir" => Ok(Command::CollapseDir),
        "collapse_all" => Ok(Command::CollapseAll),
        "open_selected" => Ok(Command::OpenSelected),
        "open_selected_bg" => Ok(Command::OpenSelectedBg),
        "toggle_side_panel" => Ok(Command::ToggleSidePanel),
        "toggle_focus" => Ok(Command::ToggleFocus),
        "find_file" => Ok(Command::FindFile),
        "save_as" => Ok(Command::SaveAs),
        "find_file_tab_complete" => Ok(Command::FindFileTabComplete),
        "in_buffer_search" => Ok(Command::InBufferSearch),
        "open_file_search" => Ok(Command::OpenFileSearch),
        "close_file_search" => Ok(Command::CloseFileSearch),
        "toggle_search_case" => Ok(Command::ToggleSearchCase),
        "toggle_search_regex" => Ok(Command::ToggleSearchRegex),
        "toggle_search_replace" => Ok(Command::ToggleSearchReplace),
        "replace_all" => Ok(Command::ReplaceAll),
        "lsp_goto_definition" => Ok(Command::LspGotoDefinition),
        "lsp_rename" => Ok(Command::LspRename),
        "lsp_code_action" => Ok(Command::LspCodeAction),
        "lsp_toggle_inlay_hints" => Ok(Command::LspToggleInlayHints),
        "lsp_format" => Ok(Command::LspFormat),
        "outline" => Ok(Command::Outline),
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
            ("suspend", Command::Suspend),
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
        // Plain ctrl+c is UNBOUND at the root (legacy parity).
        assert_eq!(m.lookup_direct(&parse_key("ctrl+c").unwrap()), None);
        // Plain ctrl+s launches in-buffer isearch (M13). Saving
        // uses the chord `ctrl+x ctrl+s`.
        assert_eq!(
            m.lookup_direct(&parse_key("ctrl+s").unwrap()),
            Some(Command::InBufferSearch),
        );
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
