//! Dispatch-level [`Command`] vocabulary — the shared ABI shape
//! used across `state-kbd-macro` (records `Vec<Command>`),
//! `runtime::keymap` (maps key events to commands), and
//! `runtime::dispatch` (matches on variants).
//!
//! Lives in a standalone leaf crate so that `led-core` stays
//! primitives-only. Snake-case string parsing (`parse_command`) is
//! keymap-config territory and lives in `runtime::keymap`.

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
    /// `Tab` outside of any overlay (M23). Replaces the active line's
    /// leading whitespace with the language-aware indent suggestion;
    /// when no syntax tree is available, falls back to inserting
    /// spaces up to the next 4-column tab stop. Default binding:
    /// `tab`.
    InsertTab,
    /// Reflow the paragraph (or doc-comment block) at the cursor
    /// using the bundled dprint markdown engine (M23). Default
    /// binding: `ctrl+q`. Inside the file browser the same chord
    /// rebinds to `CollapseAll`.
    ReflowParagraph,
    /// Sort the import block at the active buffer's top via tree-
    /// sitter (M23). Languages without an `imports.scm` (or before
    /// the parse has landed) get an "Imports already sorted"
    /// alert. Default binding: `ctrl+x i`.
    SortImports,

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
