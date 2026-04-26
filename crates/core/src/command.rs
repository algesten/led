//! Dispatch-level [`Command`] vocabulary and the snake-case
//! [`parse_command`] used by config loaders.
//!
//! Lives in `led-core` (instead of `led-runtime::keymap`) so that
//! crates which need to store / serialize `Vec<Command>` (e.g. the
//! keyboard-macro state crate) can depend only on `led-core`,
//! avoiding a cyclic dep on `led-runtime`. The runtime re-exports
//! both items from `runtime::keymap` so existing call sites are
//! unaffected.

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

    // Keyboard macros (M22).
    /// Begin recording. Default binding: `ctrl+x (`. Clears
    /// any in-progress recording and flips
    /// `KbdMacroState.recording` to true. Re-issuing while
    /// already recording resets `current` and stays in record
    /// mode (legacy parity).
    KbdMacroStart,
    /// End recording. Default binding: `ctrl+x )`. Moves
    /// `KbdMacroState.current` into `last`. Issuing while not
    /// recording surfaces a "Not defining kbd macro" alert.
    KbdMacroEnd,
    /// Replay the last successfully recorded macro. Default
    /// binding: `ctrl+x e`. Honours the chord-prefix digit
    /// count via `KbdMacroState.execute_count`. Bare `e` after
    /// a successful execute also routes here (repeat-mode
    /// latch in `ChordState.macro_repeat`).
    KbdMacroExecute,
    /// Headless / harness wait primitive. Not bound by default;
    /// reachable from a recorded macro that captured one (rare).
    /// Excluded from `should_record` so a macro replay doesn't
    /// stack waits. Currently a no-op in `run_command` — the
    /// goldens harness handles waits at the script-step level
    /// (`goldens/src/scenario.rs::ScriptStep::Wait`); a future
    /// `led-test-clock`-aware impl can hang behaviour off this
    /// arm without changing the variant.
    Wait(u64),
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
        "kbd_macro_start" => Ok(Command::KbdMacroStart),
        "kbd_macro_end" => Ok(Command::KbdMacroEnd),
        "kbd_macro_execute" => Ok(Command::KbdMacroExecute),
        // `wait` deliberately omitted from the parser — Wait(u64) carries
        // a payload, so user keymaps can't bind it via plain string lookup.
        // Tests and a future harness-only path may construct it directly.
        other => Err(format!("unknown command `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            ("kbd_macro_start", Command::KbdMacroStart),
            ("kbd_macro_end", Command::KbdMacroEnd),
            ("kbd_macro_execute", Command::KbdMacroExecute),
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
}
