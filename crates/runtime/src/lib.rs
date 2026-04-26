//! The runtime: event enum, dispatch, query layer, trace, main loop.
//!
//! Each driver is strictly isolated — it knows only its own atom + its
//! own ABI types. This crate is where they're **combined**:
//!
//! - [`query`] defines the cross-atom lenses + memos that produce
//!   `LoadAction`s (for `FileReadDriver::execute`) and `Frame`s (for
//!   `paint`).
//! - [`dispatch`] mutates driver atoms in response to input events.
//! - [`run`] is the main loop: ingest → query → execute → render.
//! - [`spawn_drivers`] wires up the desktop `*-native` workers.
//!
//! A mobile runtime would replace this crate — same `*-core` crates
//! underneath, different wiring + different native workers.

pub mod config;
pub mod diag_offer;
pub mod theme;
pub mod dispatch;
pub mod keymap;
pub mod query;
pub mod trace;

use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use led_driver_buffers_core::{BufferStore, FileReadDriver, FileWriteDriver, LoadState};
use led_driver_buffers_native::{FileReadNative, FileWriteNative};
use led_driver_clipboard_core::{
    ClipboardAction, ClipboardDriver, ClipboardResult,
};
use led_driver_clipboard_native::ClipboardNative;
use led_core::{CanonPath, Notifier, PathChain};
use led_driver_terminal_core::{Dims, Frame, KeyEvent, TermEvent, Terminal, TerminalInputDriver};
use led_driver_terminal_native::{TerminalInputNative, TerminalOutputDriver};
use led_driver_file_search_core::{FileSearchCmd, FileSearchDriver};
use led_driver_file_search_native::FileSearchNative;
use led_driver_find_file_core::FindFileDriver;
use led_driver_find_file_native::FindFileNative;
use led_driver_fs_list_core::FsListDriver;
use led_driver_fs_list_native::FsListNative;
use led_driver_syntax_core::{SyntaxCmd, SyntaxDriver};
use led_driver_syntax_native::SyntaxNative;
use led_driver_lsp_core::{LspCmd, LspDriver, LspEvent};
use led_driver_lsp_native::LspNative;
use led_driver_git_core::{GitCmd, GitDriver, GitEvent};
use led_driver_git_native::GitNative;
use led_driver_session_core::{SessionCmd, SessionDriver, SessionEvent};
use led_driver_session_native::SessionNative;
use led_state_alerts::AlertState;
use led_state_browser::{BrowserUi, FsTree};
use led_state_buffer_edits::{BufferEdits, EditGroup, EditedBuffer};
use led_state_clipboard::ClipboardState;
use led_state_file_search::FileSearchState;
use led_state_find_file::FindFileState;
use led_state_isearch::IsearchState;
use led_state_jumps::JumpListState;
use led_state_kbd_macro::KbdMacroState;
use led_state_kill_ring::KillRing;
use led_state_diagnostics::{
    BufferDiagnostics, DiagnosticsStates, LspServerStatus, LspStatuses,
};
use led_state_git::GitState;
use led_state_lifecycle::{LifecycleState, Phase};
use led_state_session::{SessionBuffer, SessionData, SessionState};
use led_state_syntax::{Language, SyntaxState, SyntaxStates};
use led_state_tabs::{TabId, Tabs};

/// Wake channel: drivers signal on their own, the main loop blocks on
/// `rx.recv_timeout(deadline)` so we wake on any event or when the
/// nearest timer (info-alert expiry, future M-?? timers) fires.
pub struct Wake {
    pub notifier: Notifier,
    pub rx: std::sync::mpsc::Receiver<()>,
}

impl Wake {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            notifier: Notifier::new(tx),
            rx,
        }
    }
}

impl Default for Wake {
    fn default() -> Self {
        Self::new()
    }
}

/// How long a transient info alert (`Saved foo.rs`) stays on screen
/// before the tick loop clears it. Matches legacy.
const INFO_TTL: Duration = Duration::from_secs(2);

pub use config::{load_keymap, ConfigError};
pub use theme::{load_theme, LoadedTheme, ThemeError};
pub use dispatch::{DispatchOutcome, Dispatcher};
pub use keymap::{default_keymap, parse_command, parse_key, ChordState, Command, Keymap};
pub use query::{
    body_model, clipboard_action, file_list_action, file_load_action, file_save_action,
    find_file_action, render_frame, side_panel_model, status_bar_model, tab_bar_model,
    AlertsInput, BrowserUiInput, ClipboardStateInput, EditedBuffersInput, FindFileInput,
    FsTreeInput, PendingSavesInput, StoreLoadedInput, TabsActiveInput, TabsOpenInput,
    TerminalDimsInput,
};
pub use trace::{SharedTrace, Trace};

/// Top-level events consumed by the main loop.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Key(KeyEvent),
    Resize(Dims),
    Quit,
}

/// Bundle of sync driver handles + their native backing workers.
///
/// Drop order matters: struct fields drop in declaration order, so the
/// sync driver drops first (closing its command `Sender`), then the
/// native marker drops (no-op — the worker self-exits on hangup).
pub struct Drivers {
    pub file: FileReadDriver,
    pub file_write: FileWriteDriver,
    pub input: TerminalInputDriver,
    pub output: TerminalOutputDriver,
    pub clipboard: ClipboardDriver,
    pub fs_list: FsListDriver,
    pub find_file: FindFileDriver,
    pub file_search: FileSearchDriver,
    pub syntax: SyntaxDriver,
    pub lsp: LspDriver,
    pub git: GitDriver,
    pub session: SessionDriver,

    // Held only for lifetime management; detached on drop.
    _file_native: FileReadNative,
    _file_write_native: FileWriteNative,
    _input_native: TerminalInputNative,
    _clipboard_native: ClipboardNative,
    _fs_list_native: FsListNative,
    _find_file_native: FindFileNative,
    _file_search_native: FileSearchNative,
    _syntax_native: SyntaxNative,
    _lsp_native: LspNative,
    _git_native: GitNative,
    _session_native: SessionNative,
}

/// Allocator for fresh `TabId`s. Counter only; ids are never reused.
#[derive(Debug, Default)]
pub struct TabIdGen(u64);

impl TabIdGen {
    pub fn issue(&mut self) -> TabId {
        self.0 += 1;
        TabId(self.0)
    }
}

/// Every mutable state atom the main loop touches, bundled.
///
/// Per course-correction #4: groups the nine per-domain state
/// structs so the main loop signature stops growing with each new
/// milestone. Rust allows disjoint-field `&mut` borrows at compile
/// time, so dispatch + memo call sites still extract the atoms
/// they actually need without runtime cost.
#[derive(Default)]
pub struct Atoms {
    pub tabs: Tabs,
    pub edits: BufferEdits,
    pub store: BufferStore,
    pub terminal: Terminal,
    pub kill_ring: KillRing,
    pub clip: ClipboardState,
    pub alerts: AlertState,
    pub jumps: JumpListState,
    /// M22 — keyboard-macro state. Recording flag, in-progress
    /// `current` buffer, last completed macro (Arc-wrapped),
    /// recursion depth, pending iteration count. User-decision
    /// source; mutated only by `dispatch`. Not persisted across
    /// restarts (legacy parity, `docs/spec/macros.md` § "Session
    /// persistence").
    pub kbd_macro: KbdMacroState,
    pub browser: BrowserUi,
    pub fs: FsTree,
    /// `Some` while the find-file / save-as modal is active. See
    /// [`led_state_find_file::FindFileState`].
    pub find_file: Option<FindFileState>,
    /// `Some` while in-buffer isearch is active. See
    /// [`led_state_isearch::IsearchState`].
    pub isearch: Option<IsearchState>,
    /// `Some` while the project-wide file-search overlay is active.
    /// See [`led_state_file_search::FileSearchState`].
    pub file_search: Option<FileSearchState>,
    /// Per-buffer tree-sitter state. A buffer gains an entry when a
    /// load completes and the path's extension matches a known
    /// language; otherwise the buffer has no syntax highlighting.
    pub syntax: SyntaxStates,
    /// Per-buffer LSP diagnostics. Populated when a version-
    /// matched `LspEvent::Diagnostics` arrives; cleared on stale
    /// delivery (no-smear rule — see `feedback_lsp_no_smear.md`).
    pub diagnostics: DiagnosticsStates,
    /// Per-buffer tracker of the last `(version, saved_version)`
    /// we told the LSP driver about. The execute phase emits
    /// another `BufferChanged` when EITHER coordinate advanced —
    /// version for edits, saved_version for saves. Tracking
    /// saved_version separately is necessary because the
    /// keyboard sequence "type … type … save" leaves the version
    /// already matching `last` by save time (typing already sent
    /// didChange for every keystroke); without the second gate
    /// we'd never emit `BufferChanged{is_save=true}` and
    /// rust-analyzer would never get `didSave`, so cargo check
    /// wouldn't run.
    pub lsp_notified: std::collections::HashMap<
        CanonPath,
        (/* version */ u64, /* saved_version */ u64),
    >,
    /// `Some(sum)` holds Σ(version + saved_version) at the last
    /// `RequestDiagnostics` emission; `None` means we've never
    /// fired one. Combined flag+sum because the two cases the
    /// runtime needs to distinguish collapse naturally: "has the
    /// sum moved?" → `memo(edits) != *lsp_requested_state_sum`,
    /// where the `None` case handles the first-ever emission
    /// regardless of the sum's raw value. The per-tick current
    /// sum is derived by the `buffer_state_sum` memo.
    ///
    /// Driver-outbound bookkeeping: tracks a side-effect the
    /// runtime emitted, not a user decision or external fact.
    /// Same category as `lsp_notified` below — kept as a field
    /// because we can't derive "what did I tell the driver" from
    /// observations of current atom state.
    pub lsp_requested_state_sum: Option<u64>,
    /// `true` once `LspCmd::Init` has been emitted. Same
    /// category as `lsp_notified` / `lsp_requested_state_sum`:
    /// driver-outbound bookkeeping.
    pub lsp_init_sent: bool,
    /// Per-server LSP progress / ready status. Painter consumes
    /// via the status-bar model so the user sees when
    /// rust-analyzer is mid-indexing.
    pub lsp_status: LspStatuses,
    /// LSP completion popup. `session: Some` while a popup is
    /// live for some tab; dispatch intercepts navigation + commit
    /// keys. `seq_gen` is the monotonic request id — see
    /// [`led_state_completions::CompletionsState`].
    pub completions: led_state_completions::CompletionsState,
    /// LSP completions — driver-bookkeeping side: outboxes +
    /// `seq_gen`. Split from `completions` per arch guideline 1
    /// so popup-render memos don't recompute on every queued
    /// completion request.
    pub completions_pending: led_state_completions::CompletionsPending,
    /// LSP extras (M18) — user-decision side: which overlay is
    /// open (rename / code-action picker), the inlay-hints
    /// toggle. Mutated by dispatch from key events.
    pub lsp_extras: led_state_lsp::LspExtrasState,
    /// LSP extras (M18) — driver-bookkeeping side: outbound
    /// request outboxes, per-request `latest_*_seq` gates, the
    /// per-buffer inlay-hint cache. Mutated by ingest from
    /// `LspEvent::*` and by dispatch's `queue_*` helpers;
    /// drained by execute. Split from `lsp_extras` so its
    /// per-request churn doesn't invalidate memos that only
    /// read overlay state.
    pub lsp_pending: led_state_lsp::LspPending,
    /// Git state (M19): branch + per-file category map + per-
    /// buffer line statuses. Folded from `GitEvent::FileStatuses`
    /// and `GitEvent::LineStatuses` in the ingest phase; read by
    /// the browser / gutter / status-bar query memos.
    pub git: GitState,
    /// `true` once the initial workspace scan has been dispatched.
    /// Driver-outbound bookkeeping — same category as
    /// `lsp_init_sent`: guards the startup one-shot so we don't
    /// spam `GitScan` every tick when `fs.root` is `Some` but the
    /// driver has nothing new to do.
    pub git_scan_dispatched: bool,
    /// Set by the ingest phase on every successful save
    /// completion. Drained in the execute phase into a
    /// `GitCmd::ScanFiles` (a file save is the most common cause
    /// of git state changing, so rescanning is the right UX).
    pub git_scan_pending: bool,
    /// Whole-process lifecycle: `Phase` state machine plus the
    /// `force_redraw` repaint counter. Driven by the dispatch
    /// outcomes (`Quit` → Exiting, `Suspend` → Suspended → back
    /// to Running) and by the first-paint transition out of
    /// Starting.
    pub lifecycle: LifecycleState,
    /// Session persistence (M21). Folded from `SessionEvent::
    /// {Restored, Saved, Failed}` deliveries; consumed by the
    /// Quit gate (`Exiting && (saved || !primary)`) and the
    /// session-Save dispatch.
    pub session: SessionState,
    /// Driver-outbound bookkeeping: `true` once the runtime has
    /// dispatched `SessionCmd::Save` for the active Exiting
    /// transition, so we don't spam Save every tick while
    /// waiting for the `Saved` event. Same category as
    /// `lsp_init_sent`.
    pub session_save_dispatched: bool,
    /// Set by the Suspended → Running edge (M20) and the
    /// session-restore complete edge (M21) — anything that
    /// requires `Phase::Resuming` → `Phase::Running` to be
    /// re-evaluated next tick. Always derivable from atoms; we
    /// store it as a flag because we don't want to scan every
    /// tab every tick.
    pub resume_check_pending: bool,
    /// Per-buffer undo persistence bookkeeping. Tracks how far
    /// each path's `history.past` has been flushed to the
    /// session DB so subsequent flushes ship only the newly-
    /// finalised groups (mirrors legacy's incremental append).
    pub undo_persistence: std::collections::HashMap<CanonPath, UndoPersistTracker>,
    /// Per-buffer debounce state for the undo-flush dispatch.
    /// Each entry records the last buffer `version` we saw and
    /// the wall-clock instant we first saw it. The flush fires
    /// once 200ms have elapsed without the version moving —
    /// mirrors legacy's `Schedule::KeepExisting` 200ms timer
    /// (`docs/spec/persistence.md`). Without this debounce a
    /// freshly-typed character would fire `WorkspaceFlushUndo`
    /// on the very next tick, adding spurious trace lines to
    /// short scripts (delete_backward, insert_newline, …) that
    /// legacy goldens captured before the debounce fired.
    pub undo_flush_debounce: std::collections::HashMap<CanonPath, (u64, Instant)>,
    /// Symlink resolution chain for every path the user has
    /// opened, keyed by canonical path. Populated at tab-open
    /// time (main.rs CLI, find-file commit, browser open) so the
    /// load-completion handler can detect the language from the
    /// user-typed name even when canonicalization has stripped
    /// the informative extension. Mirrors legacy led's
    /// `PathChain` → `LanguageId::from_chain` routing.
    pub path_chains: std::collections::HashMap<CanonPath, PathChain>,
}

/// Per-buffer state tracking what we've shipped to the
/// `undo_entries` table. Mirrors legacy's `BufferState::
/// {chain_id, persisted_undo_len, last_seen_seq}` triple.
#[derive(Debug, Clone, Default)]
pub struct UndoPersistTracker {
    /// UUID-ish chain id we generate on first flush per
    /// buffer-session. Stable across the rest of the session;
    /// regenerated on session restore (with a new chain) when
    /// the disk content's hash differs from the persisted
    /// `content_hash`.
    pub chain_id: String,
    /// `past.len()` we've already flushed. Next flush ships
    /// `history.past_groups()[persisted_len..]`.
    pub persisted_len: usize,
    /// Latest `seq` returned by the session driver after a
    /// successful flush. Used by future cross-instance sync.
    pub last_seq: i64,
}

/// Run-time seam: the single thing the main loop sees. Owns nothing
/// — just bundles borrowed views of the atoms, drivers, config,
/// wake signal, trace sink, and stdout writer. Shrinks `run()` to
/// a one-arg function.
pub struct World<'a, W: Write> {
    pub atoms: &'a mut Atoms,
    pub drivers: &'a Drivers,
    pub keymap: &'a Keymap,
    pub theme: &'a led_driver_terminal_core::Theme,
    pub wake: &'a Wake,
    pub trace: &'a SharedTrace,
    pub stdout: &'a mut W,
    /// CLI-supplied `--config-dir` override. When `None`, the
    /// runtime falls back to `$XDG_CONFIG_HOME/led` then
    /// `$HOME/.config/led` (`config_dir_for_session`). The
    /// override is essential for the goldens harness so each
    /// scenario uses an isolated `<tmpdir>/config/` and acquires
    /// its own session flock; without it every test races on the
    /// developer's real `~/.config/led/db.sqlite`.
    pub cli_config_dir: Option<&'a std::path::Path>,
}

/// Run the main loop until dispatch signals quit.
pub fn run<W: Write>(world: &mut World<'_, W>) -> io::Result<()> {
    let Atoms {
        tabs,
        edits,
        kill_ring,
        clip,
        alerts,
        jumps,
        kbd_macro,
        browser,
        fs,
        store,
        terminal,
        find_file,
        isearch,
        file_search,
        syntax,
        path_chains,
        diagnostics,
        lsp_notified,
        lsp_requested_state_sum,
        lsp_init_sent,
        lsp_status,
        completions,
        completions_pending,
        lsp_extras,
        lsp_pending,
        git,
        git_scan_dispatched,
        git_scan_pending,
        lifecycle,
        session,
        session_save_dispatched,
        resume_check_pending,
        undo_persistence,
        undo_flush_debounce,
    } = &mut *world.atoms;
    let drivers = world.drivers;
    let wake = world.wake;
    let keymap = world.keymap;
    let theme = world.theme;
    let stdout = &mut *world.stdout;
    let cli_config_dir = world.cli_config_dir;
    // `world.trace` is wired into every driver at spawn time; the
    // main loop also emits a `WorkspaceClearUndo` on each save,
    // so it holds a direct handle.
    let trace = world.trace;
    let mut last_frame: Option<Frame> = None;
    let mut chord = ChordState::default();

    loop {
        // ── Ingest ──────────────────────────────────────────────
        // Clear expired info alerts at the top of each tick — one
        // `Instant::now()` compare per tick, zero allocs when no
        // alert is live. Find-file's transient "[No match]" hint
        // follows the same TTL discipline.
        let now = Instant::now();
        alerts.expire_info(now);
        if let Some(ff) = find_file.as_mut() {
            ff.input.expire_hint(now);
        }

        // Seed BufferEdits from newly-Ready loads. `seed_edit_from_load`
        // enforces the discipline that an existing edit entry wins
        // over a late-arriving load completion (course-correct #6).
        // `process` returns an empty Vec on idle ticks — no heap
        // alloc on the happy path.
        let file_completions = drivers.file.process(store);
        for completion in file_completions {
            // Language detection prefers the symlink chain stashed
            // at tab-open time: walking `user → intermediates →
            // resolved` matches legacy's rule that the user-typed
            // name wins. Falls back to the bare canonical-path
            // detector when no chain is recorded (e.g. an internal
            // open that didn't come through a UserPath).
            let detected = path_chains
                .get(&completion.path)
                .and_then(Language::from_chain)
                .or_else(|| Language::from_path(&completion.path));
            let inserted = seed_edit_from_load(
                edits,
                completion.path.clone(),
                completion.rope.clone(),
            );

            // M21 undo restore (legacy-shaped): if a stashed
            // UndoRestoreData exists for this path AND its
            // `content_hash` matches the freshly-loaded disk
            // content, install the entries into eb.history.past
            // and replay them forward onto eb.rope so the
            // dirty-edit state carries across the quit. Mismatch
            // → drop silently; the file changed externally
            // between sessions.
            if inserted {
                apply_pending_undo_restore(
                    &completion.path,
                    edits,
                    session,
                    undo_persistence,
                );
                // Persist ancestor reveal once on first materialization
                // of the ACTIVE tab, mirroring legacy `reveal_active_buffer`
                // (`led/src/model/action/helpers.rs:36`) which fires
                // from `Mut::ActivateBuffer` for the active path only.
                // Writing into `expanded_dirs` (rather than re-deriving
                // each tick) means a later collapse_dir / collapse_all
                // sticks: the user's intent wins because nothing
                // re-reveals on idle ticks. Background tabs that
                // materialize later don't yank the tree open.
                let is_active = tabs
                    .active
                    .and_then(|id| tabs.open.iter().find(|t| t.id == id))
                    .is_some_and(|t| t.path == completion.path);
                if is_active {
                    let ancestors = led_state_browser::ancestors_of(
                        fs,
                        &browser.expanded_dirs,
                        Some(&completion.path),
                    );
                    for p in ancestors {
                        browser.expanded_dirs.insert(p);
                    }
                }
            }
            if let Some(lang) = detected {
                syntax
                    .by_path
                    .entry(completion.path.clone())
                    .or_insert_with(|| SyntaxState::new(lang));
            }
            // Tell the LSP driver about the new buffer. The driver
            // ignores languages it doesn't have a registry entry
            // for, so sending unconditionally is fine.
            if inserted {
                let (version, saved, hash) = edits
                    .buffers
                    .get(&completion.path)
                    .map(|eb| {
                        (
                            eb.version,
                            eb.saved_version,
                            led_core::EphemeralContentHash::of_rope(&eb.rope).persist(),
                        )
                    })
                    .unwrap_or_default();
                drivers.lsp.execute(std::iter::once(&LspCmd::BufferOpened {
                    path: completion.path.clone(),
                    language: detected,
                    rope: completion.rope.clone(),
                    hash,
                }));
                lsp_notified.insert(completion.path.clone(), (version, saved));
            }

            // Apply pending cursor / scroll for any tab waiting
            // on this path. Three call sites stash a pending
            // cursor on tab-open: session restore (M21),
            // Alt-Enter goto-def into an unopened file, and
            // Alt-./Alt-, into an unopened file. Clamp to the
            // rope so a stale (line, col) from disk doesn't
            // land outside the buffer.
            for tab in tabs.open.iter_mut() {
                if tab.path != completion.path {
                    continue;
                }
                let rope = &completion.rope;
                let line_count = rope.len_lines();
                if let Some(pc) = tab.pending_cursor.take() {
                    let line = pc.line.min(line_count.saturating_sub(1));
                    let line_start = rope.line_to_char(line);
                    let line_end = if line + 1 < line_count {
                        rope.line_to_char(line + 1)
                    } else {
                        rope.len_chars()
                    };
                    let line_len = line_end.saturating_sub(line_start);
                    let col = pc.col.min(line_len);
                    tab.cursor = led_state_tabs::Cursor {
                        line,
                        col,
                        preferred_col: col,
                    };
                }
                if let Some(ps) = tab.pending_scroll.take() {
                    // Clamp scroll.top to the buffer's line
                    // count — a stale snapshot may overshoot.
                    let top = ps.top.min(line_count.saturating_sub(1));
                    tab.scroll = led_state_tabs::Scroll {
                        top,
                        top_sub_line: ps.top_sub_line,
                    };
                }
            }
            // The Phase::Resuming → Running transition checks
            // every tab; bookkeeping flag tells the loop to
            // re-evaluate now that one buffer just settled.
            *resume_check_pending = true;
        }

        // Phase::Resuming → Running once every tab with a
        // pending cursor has had it applied (i.e. nothing left
        // to wait for). Cheap O(tabs) scan, only runs when a
        // load just completed.
        if *resume_check_pending {
            *resume_check_pending = false;
            if matches!(lifecycle.phase, Phase::Resuming) {
                let still_pending = tabs
                    .open
                    .iter()
                    .any(|t| t.pending_cursor.is_some() || t.pending_scroll.is_some());
                if !still_pending {
                    lifecycle.phase = Phase::Running;
                }
            }
        }

        // Apply LSP driver completions. Each delivery carries a
        // `PersistedContentHash` stamped when the pull window
        // opened (pull) or when the push was cached (push); the
        // runtime accepts via `diag_offer::offer_diagnostics`
        // which handles two paths:
        //   * fast: stamped hash matches the buffer's current
        //     ephemeral hash verbatim (typical after save with no
        //     subsequent edits, and after undo rewinds to save).
        //   * replay: the history holds a save-point marker for
        //     the stamped hash — we transform diagnostic positions
        //     forward through the intervening edits (drop
        //     same-row diagnostics, shift structural), mirroring
        //     legacy `replay_diagnostics`.
        // Mismatches drop silently; a later pull re-fetches.
        // Empty accepted deliveries clear the atom for that path.
        for ev in drivers.lsp.process() {
            match ev {
                LspEvent::Diagnostics {
                    path,
                    hash,
                    diagnostics: diags,
                } => {
                    let Some(eb) = edits.buffers.get(&path) else {
                        continue;
                    };
                    let transformed = match diag_offer::offer_diagnostics(eb, hash, diags) {
                        diag_offer::OfferOutcome::Accept(d) => d,
                        diag_offer::OfferOutcome::Reject => continue,
                    };
                    let current_hash =
                        led_core::EphemeralContentHash::of_rope(&eb.rope).persist();
                    if transformed.is_empty() {
                        diagnostics.by_path.remove(&path);
                    } else {
                        diagnostics.by_path.insert(
                            path,
                            BufferDiagnostics::new(current_hash, transformed),
                        );
                    }
                }
                LspEvent::Ready { server } => {
                    let entry = lsp_status
                        .by_server
                        .entry(server)
                        .or_insert_with(LspServerStatus::default);
                    entry.ready = true;
                    entry.busy = false;
                    entry.detail = None;
                }
                LspEvent::Progress { server, busy, detail } => {
                    let entry = lsp_status
                        .by_server
                        .entry(server)
                        .or_insert_with(LspServerStatus::default);
                    entry.busy = busy;
                    entry.detail = detail;
                    if !busy {
                        // A progress cycle ended with no
                        // quiescence signal — treat as ready.
                        entry.ready = true;
                    }
                }
                LspEvent::Error { server, message } => {
                    // Surface as a warn alert keyed by server
                    // name so a repeat error replaces rather
                    // than stacks. Also clear progress so the
                    // status bar stops saying "indexing" when
                    // the server's actually dead.
                    alerts.set_warn(server.clone(), format!("LSP {server}: {message}"));
                    if let Some(entry) = lsp_status.by_server.get_mut(&server) {
                        entry.busy = false;
                        entry.detail = None;
                    }
                }
                LspEvent::Completion {
                    path,
                    seq,
                    items,
                    prefix_line,
                    prefix_start_col,
                } => {
                    // Stale-gate: the latest allocated `seq_gen`
                    // is the id we'd echo back on the next new
                    // request; any response older than that is
                    // discarded. Exact-match is the live session;
                    // we accept it and build / replace the
                    // session to match.
                    if seq != completions_pending.seq_gen {
                        continue;
                    }
                    // Find the tab id that corresponds to `path`.
                    // If the user switched tabs (or closed the
                    // tab) while the server was composing the
                    // response, silently drop.
                    let Some(tab) = tabs.open.iter().find(|t| t.path == path) else {
                        continue;
                    };
                    if items.is_empty() {
                        completions.dismiss();
                        continue;
                    }
                    // Resolve `prefix_start_col`: when the server
                    // gave us a `textEdit.range` we use that
                    // verbatim; otherwise backtrack through
                    // identifier characters from the cursor on
                    // `prefix_line` (matches legacy
                    // `convert_completion_response`). Keeps the
                    // popup useful for servers that return
                    // bare-label items (and our fake-lsp).
                    let prefix_start_col = match prefix_start_col {
                        Some(c) => c,
                        None => identifier_start_col(
                            edits,
                            &path,
                            prefix_line as usize,
                            tab.cursor.col,
                        ),
                    };
                    // Refilter against the user's current typed
                    // prefix so the popup shows relevance-ranked
                    // items on first paint — not the raw server
                    // list. Prefix extends from
                    // `prefix_start_col` to the cursor, scoped
                    // to the row the request was issued for.
                    let prefix = completion_prefix(
                        edits,
                        &path,
                        tab,
                        prefix_line as usize,
                        prefix_start_col as usize,
                    );
                    let filtered = led_state_completions::refilter(&items, &prefix);
                    if filtered.is_empty() {
                        completions.dismiss();
                        continue;
                    }
                    // Suppress redundant popup: one remaining
                    // candidate that the user has already typed
                    // verbatim. The popup would correctly display
                    // the match, but a committable item that
                    // changes nothing is UX noise.
                    if filtered.len() == 1
                        && led_state_completions::is_identity_match(
                            &items[filtered[0]],
                            &prefix,
                        )
                    {
                        completions.dismiss();
                        continue;
                    }
                    completions.session =
                        Some(led_state_completions::CompletionSession {
                            tab: tab.id,
                            path,
                            seq,
                            prefix_line,
                            prefix_start_col,
                            items,
                            filtered: std::sync::Arc::new(filtered),
                            selected: 0,
                            scroll: 0,
                        });
                }
                LspEvent::CompletionResolved { .. } => {
                    // Stage 5 handles the post-commit apply.
                }
                LspEvent::GotoDefinition { seq, location } => {
                    LspGotoApply {
                        tabs,
                        edits,
                        jumps,
                        alerts,
                        lsp_pending,
                        terminal,
                        browser,
                        path_chains,
                    }
                    .apply(seq, location);
                }
                LspEvent::Edits {
                    seq,
                    origin,
                    edits: file_edits,
                } => {
                    let _ = lsp_extras; // not needed by apply
                    LspEditApply {
                        edits,
                        tabs,
                        alerts,
                        lsp_pending,
                    }
                    .apply(seq, origin, &file_edits);
                }
                LspEvent::CodeActions {
                    path,
                    seq,
                    actions,
                } => {
                    if lsp_pending.latest_code_action_seq != Some(seq) {
                        // Stale response; drop.
                    } else if !actions.is_empty() {
                        dispatch::install_code_action_picker(
                            lsp_extras,
                            path,
                            seq,
                            actions,
                        );
                    }
                    // Empty list silently drops — matches legacy
                    // (`Mut::LspCodeActions` clears the picker
                    // without surfacing any alert when actions
                    // come back empty).
                }
                LspEvent::InlayHints {
                    path,
                    version,
                    hints,
                } => {
                    if !lsp_extras.inlay_hints_enabled {
                        continue;
                    }
                    // Only accept hints whose `version` matches
                    // the buffer's current version. Stale
                    // replies don't smear on a later rope.
                    let current_version = edits
                        .buffers
                        .get(&path)
                        .map(|eb| eb.version)
                        .unwrap_or(0);
                    if version != current_version {
                        continue;
                    }
                    lsp_pending.inlay_hints_by_path.insert(
                        path,
                        led_state_lsp::BufferInlayHints { version, hints },
                    );
                }
            }
        }

        // Apply write completions: round-trip the saved rope into
        // `BufferStore` as the new disk baseline, and bump
        // `saved_version` so `dirty()` becomes false (unless the
        // user has since edited past that version). Surfaces the
        // outcome via alerts: success → transient info; error →
        // persistent warn keyed by path.
        for done in drivers.file_write.process() {
            let basename = done
                .path
                .file_name()
                .map(|os| os.to_string_lossy().into_owned())
                .unwrap_or_else(|| done.path.display().to_string());
            match done.result {
                Ok(rope) => {
                    store
                        .loaded
                        .insert(done.path.clone(), LoadState::Ready(rope));
                    if let Some(eb) = edits.buffers.get_mut(&done.path) {
                        eb.saved_version = eb.saved_version.max(done.version);
                        // Anchor this save in the undo history so
                        // late-arriving LSP diagnostics stamped
                        // with this content hash can still replay
                        // forward through any edits the user has
                        // landed since. Direct port of legacy's
                        // post-save `insert_save_point(doc.content_hash())`.
                        let hash =
                            led_core::EphemeralContentHash::of_rope(&eb.rope).persist();
                        eb.history.insert_save_point(hash);
                        // Saved bytes are the new disk baseline —
                        // refresh the anchor used by undo flushes
                        // (legacy's `BufferState::content_hash`).
                        eb.disk_content_hash = hash;
                    }
                    alerts.clear_warn(&basename);
                    alerts.set_info(format!("Saved {basename}"), Instant::now(), INFO_TTL);
                    // Disk changed — ask git to rescan. The
                    // workspace-root gate lives in the execute
                    // phase; setting the flag here keeps the
                    // intent local to the save site.
                    *git_scan_pending = true;
                }
                Err(msg) => {
                    // Already traced inside FileWriteDriver::process.
                    // Buffer stays dirty so the user can retry.
                    alerts.set_warn(basename.clone(), format!("save {basename}: {msg}"));
                }
            }
        }

        // Apply fs-list completions: round-trip entries into the
        // `FsTree.dir_contents` cache and rebuild the flattened
        // browser view. Failures leave the dir unlisted; the user
        // can retry via CollapseAll-then-reopen.
        let fs_completions = drivers.fs_list.process();
        for done in fs_completions {
            if let Ok(entries) = done.result {
                fs.dir_contents
                    .insert(done.path, imbl::Vector::from_iter(entries));
            }
        }
        // Tree rebuild is no longer imperative — the
        // `browser_entries` memo derives from `fs.dir_contents`
        // fresh on next access.

        // Apply find-file completions. Late arrivals whose `dir` +
        // `prefix` no longer match the overlay's current input are
        // dropped — legacy's "expected_dir" discipline. Matching
        // completions replace `state.completions` wholesale.
        //
        // When the overlay is in arrow-follow mode (user engaged
        // arrow-navigation and then descended via Enter), auto-
        // select the first entry of the fresh listing so the next
        // Enter keeps drilling without requiring another Down.
        for done in drivers.find_file.process() {
            let Some(ff) = find_file.as_mut() else {
                continue;
            };
            let (dir_part, prefix) = led_state_find_file::split_input(&ff.input.text);
            if dir_part.is_empty() {
                continue;
            }
            let expected_dir = led_core::UserPath::new(led_state_find_file::expand_path(
                dir_part,
            ))
            .canonicalize();
            if done.dir != expected_dir || done.prefix != prefix {
                continue;
            }
            ff.completions = done.entries;
            auto_advance_arrow_follow(ff, tabs);
        }

        // Apply file-search completions. Late arrivals whose
        // `query` / `case_sensitive` / `use_regex` no longer match
        // the overlay's current state are dropped (same
        // "expected_dir" discipline as find-file). Matching
        // completions replace `results` + `flat_hits`; the
        // selection resets to the search input when the tree
        // shape changes so an out-of-bounds `Result(i)` doesn't
        // persist.
        for done in drivers.file_search.process() {
            let Some(fs_state) = file_search.as_mut() else {
                continue;
            };
            if done.query != fs_state.query.text
                || done.case_sensitive != fs_state.case_sensitive
                || done.use_regex != fs_state.use_regex
            {
                continue;
            }
            fs_state.results = done.groups;
            fs_state.flat_hits = done.flat;
            // New result set — all hits are pending again. Per-hit
            // replacement state from the previous search is no
            // longer meaningful (indices may point at different
            // hits now).
            fs_state.hit_replacements =
                vec![None; fs_state.flat_hits.len()];
            if let led_state_file_search::FileSearchSelection::Result(i) =
                fs_state.selection
                && i >= fs_state.flat_hits.len()
            {
                fs_state.selection =
                    led_state_file_search::FileSearchSelection::SearchInput;
            }
            fs_state.scroll_offset = 0;
        }

        // Apply replace-all completions. Combine the driver's
        // on-disk counts with the dispatch-side in-memory counts
        // (staged in `edits.pending_replace_in_memory`) and surface
        // a single `"Replaced N occurrence(s)"` alert (legacy
        // format, `Mut::FileSearchReplaceComplete` in
        // `led/src/model/mod.rs`).
        for done in drivers.file_search.process_replace() {
            let memory = std::mem::take(&mut edits.pending_replace_in_memory);
            let memory_total: usize = memory.iter().map(|m| m.count).sum();
            let total = done.total_replacements + memory_total;
            alerts.set_info(
                format!("Replaced {total} occurrence(s)"),
                Instant::now(),
                INFO_TTL,
            );
        }

        // Apply syntax parse completions. The runtime stores the
        // rope the parse was performed against as `tree_rope` —
        // the next dispatch ships that back to the worker, which
        // derives the edit purely from `(prev_rope, curr_rope)`.
        // No applied-ops counter to drift through undo/redo.
        for done in drivers.syntax.process() {
            let Some(state) = syntax.by_path.get_mut(&done.path) else {
                continue;
            };
            // Only clear `in_flight_version` if this completion
            // matches what we're waiting on. A stale `v1`
            // completion mustn't un-gate `v2` still in flight.
            if state.in_flight_version == Some(done.version) {
                state.in_flight_version = None;
            }
            let current_version = edits
                .buffers
                .get(&done.path)
                .map(|eb| eb.version)
                .unwrap_or(0);
            if done.version < state.version || done.version > current_version {
                continue;
            }
            state.language = done.language;
            state.tree = Some(done.tree);
            state.tree_rope = Some(done.tree_rope);
            state.tokens = done.tokens;
            state.version = done.version;
        }

        // Apply session driver events. The Init reply seeds
        // `session.last_saved` with whatever the DB held;
        // dispatch turns each restored tab into a new Tab with
        // `pending_cursor` set so the load-completion ingest
        // hook can land the cursor once the buffer materialises.
        // Saved flips `session.saved` so the Quit gate clears.
        // Failures degrade gracefully — surface a warn alert.
        let mut session_just_restored = false;
        for ev in drivers.session.process() {
            match ev {
                SessionEvent::Restored { primary, restored } => {
                    session.primary = primary;
                    session.init_done = true;
                    if let Some(data) = restored {
                        // Stash per-buffer undo restore data.
                        // The load-completion hook checks the
                        // disk hash before applying.
                        for sb in &data.buffers {
                            if let Some(undo) = &sb.undo {
                                session
                                    .pending_undo
                                    .insert(sb.path.clone(), undo.clone());
                            }
                        }
                        // CLI-arg buffers may already be loaded by
                        // the time the Init reply lands (the
                        // file-read driver runs ahead of the
                        // session worker). Walk the just-stashed
                        // pending_undo set and apply to any path
                        // whose buffer is already in `edits` —
                        // the load-completion handler only fires
                        // for first-time inserts, so we'd
                        // otherwise leak the chain.
                        let materialised: Vec<CanonPath> = session
                            .pending_undo
                            .keys()
                            .filter(|p| edits.buffers.contains_key(*p))
                            .cloned()
                            .collect();
                        for path in materialised {
                            apply_pending_undo_restore(
                                &path,
                                edits,
                                session,
                                undo_persistence,
                            );
                        }
                        // Materialise restored tabs. CLI arg
                        // tabs already in `tabs.open` get the
                        // saved cursor + scroll merged onto
                        // them as `pending_*`. New tabs spawn
                        // for paths not already open.
                        let mut new_tabs: imbl::Vector<led_state_tabs::Tab> =
                            tabs.open.clone();
                        for sb in &data.buffers {
                            if let Some(existing) = new_tabs
                                .iter_mut()
                                .find(|t| t.path == sb.path)
                            {
                                if existing.pending_cursor.is_none() {
                                    existing.pending_cursor = Some(sb.cursor);
                                }
                                if existing.pending_scroll.is_none() {
                                    existing.pending_scroll = Some(sb.scroll);
                                }
                                continue;
                            }
                            let id = TabId(
                                new_tabs
                                    .iter()
                                    .map(|t| t.id.0)
                                    .max()
                                    .unwrap_or(0)
                                    + 1,
                            );
                            let chain = led_core::UserPath::new(
                                sb.path.as_path(),
                            )
                            .resolve_chain();
                            path_chains.insert(sb.path.clone(), chain);
                            new_tabs.push_back(led_state_tabs::Tab {
                                id,
                                path: sb.path.clone(),
                                pending_cursor: Some(sb.cursor),
                                pending_scroll: Some(sb.scroll),
                                ..Default::default()
                            });
                        }
                        // Active tab: prefer whatever the user
                        // asked for via CLI args; otherwise
                        // honour the saved active index.
                        if tabs.active.is_none()
                            && let Some(t) =
                                new_tabs.get(data.active_tab_order)
                        {
                            tabs.active = Some(t.id);
                        }
                        tabs.open = new_tabs;
                        // Restore browser visibility + selection
                        // + jump list from the kv slot. Mirrors
                        // legacy's session_of consumer.
                        browser.visible = data.show_side_panel;
                        apply_session_kv(&data.kv, browser, jumps);
                        session.last_saved = Some(data);
                    } else {
                        session.last_saved = None;
                    }
                    session_just_restored = true;
                }
                SessionEvent::SessionSaved => {
                    session.saved = true;
                }
                SessionEvent::UndoFlushed {
                    path,
                    chain_id,
                    persisted_undo_len,
                    last_seq,
                } => {
                    // Confirm the optimistic advance: pin
                    // `persisted_len` to the value the driver
                    // actually inserted, and record `last_seq` for
                    // future cross-instance sync (M21+). If the
                    // tracker has already rotated to a new
                    // chain_id (post-save reset), ignore the
                    // ack — those entries belong to a chain that
                    // was just dropped from the DB.
                    if let Some(tracker) = undo_persistence.get_mut(&path)
                        && tracker.chain_id == chain_id
                    {
                        tracker.persisted_len = persisted_undo_len;
                        tracker.last_seq = last_seq;
                    }
                }
                SessionEvent::Failed { message } => {
                    alerts.set_warn(
                        "session".to_string(),
                        format!("session: {message}"),
                    );
                    // Don't keep retrying; mark saved so the
                    // Quit gate can still clear.
                    session.saved = true;
                    session.init_done = true;
                }
            }
        }
        if session_just_restored && !tabs.open.is_empty() {
            // We just synthesised tabs with pending cursors —
            // bookkeeping flag tells the loop to re-evaluate
            // `Phase::Resuming` after the current execute pass.
            *resume_check_pending = true;
            if matches!(lifecycle.phase, Phase::Starting) {
                lifecycle.phase = Phase::Resuming;
            }
        } else if session_just_restored {
            // Restored with empty session OR non-primary — no
            // tabs to wait for.
            if matches!(lifecycle.phase, Phase::Starting | Phase::Resuming) {
                lifecycle.phase = Phase::Running;
            }
        }

        // Apply git driver events. The driver emits a burst per
        // scan: one FileStatuses (always), one LineStatuses per
        // dirty path, then one empty LineStatuses per formerly-
        // dirty path that has since gone clean. FileStatuses
        // arrives first by construction so the reducer installs
        // the new map before per-path line entries land.
        for ev in drivers.git.process() {
            match ev {
                GitEvent::FileStatuses { statuses, branch } => {
                    git.branch = branch;
                    let mut imbl_map: imbl::HashMap<
                        CanonPath,
                        imbl::HashSet<led_core::IssueCategory>,
                    > = imbl::HashMap::default();
                    for (path, cats) in statuses {
                        let mut imbl_set: imbl::HashSet<led_core::IssueCategory> =
                            imbl::HashSet::default();
                        for c in cats {
                            imbl_set.insert(c);
                        }
                        imbl_map.insert(path, imbl_set);
                    }
                    git.file_statuses = imbl_map;
                }
                GitEvent::LineStatuses { path, statuses } => {
                    if statuses.is_empty() {
                        git.line_statuses.remove(&path);
                    } else {
                        git.line_statuses.insert(path, Arc::new(statuses));
                    }
                }
            }
        }

        // Apply clipboard completions: either paste the text at the
        // tab the yank was issued from, or on empty/error fall back
        // to the kill ring. Writes only clear the in-flight bit.
        for done in drivers.clipboard.process() {
            let content_cols = dispatch::editor_content_cols(terminal, browser);
            match done.result {
                Ok(ClipboardResult::Text(Some(text))) => {
                    if let Some(target) = clip.pending_yank.take() {
                        dispatch::apply_yank(tabs, edits, target, &text, content_cols);
                    }
                    clip.read_in_flight = false;
                }
                Ok(ClipboardResult::Text(None)) | Err(_) => {
                    // Empty clipboard or read failure — fall back to
                    // the kill ring's latest entry.
                    if let Some(target) = clip.pending_yank.take()
                        && let Some(fallback) = kill_ring.latest.clone()
                    {
                        dispatch::apply_yank(tabs, edits, target, &fallback, content_cols);
                    }
                    clip.read_in_flight = false;
                }
                Ok(ClipboardResult::Written) => {
                    // Nothing further to do.
                }
            }
        }

        drivers.input.process(terminal);

        // Drain one event at a time — the `VecDeque::pop_front` yields
        // each event by value, so the partial borrow of
        // `terminal.pending` is released before dispatch takes a full
        // `&Terminal`. No intermediate `Vec<Event>` per tick.
        while let Some(term_ev) = terminal.pending.pop_front() {
            let ev = match term_ev {
                TermEvent::Key(k) => Event::Key(k),
                TermEvent::Resize(d) => Event::Resize(d),
            };
            let mut dispatcher = Dispatcher {
                tabs,
                edits,
                kill_ring,
                clip,
                alerts,
                jumps,
                browser,
                fs,
                store,
                terminal,
                find_file,
                isearch,
                file_search,
                completions,
                completions_pending,
                lsp_extras,
                lsp_pending,
                diagnostics,
                lsp_status,
                git,
                path_chains,
                keymap,
                chord: &mut chord,
                kbd_macro,
            };
            match dispatcher.dispatch(ev) {
                DispatchOutcome::Continue => {}
                DispatchOutcome::Quit => {
                    // M21: don't break here. Set the phase and
                    // fall through to the execute pass, which
                    // dispatches SessionCmd::Save. The next
                    // iteration's gate (below the dispatch loop)
                    // breaks once session.saved flips.
                    lifecycle.phase = Phase::Exiting;
                    break;
                }
                DispatchOutcome::Suspend => {
                    // SIGTSTP round-trip. The helper leaves the
                    // alt-screen, raises SIGTSTP, and on `fg`
                    // re-enters + re-enables raw mode. Bumping
                    // `force_redraw` is the user-facing signal
                    // ("we got suspended, redraw"); invalidating
                    // the painter's internal mirror is what
                    // actually makes it repaint. Without the
                    // invalidate call, the cell-diff renderer
                    // compares the post-resume frame against its
                    // pre-suspend mirror, concludes nothing
                    // changed, and emits zero bytes — the screen
                    // stays at whatever the shell left behind.
                    lifecycle.phase = Phase::Suspended;
                    if let Err(e) =
                        led_driver_terminal_native::suspend_and_resume(stdout)
                    {
                        // Terminal restoration failed — rare but
                        // not fatal. Surface as a warn alert so
                        // the user sees something went sideways;
                        // the editor itself keeps running.
                        alerts.set_warn(
                            "suspend".to_string(),
                            format!("suspend: {e}"),
                        );
                    }
                    lifecycle.phase = Phase::Running;
                    lifecycle.bump_redraw();
                    drivers.output.invalidate();
                    last_frame = None;
                }
            }
        }
        // M21 quit gate: we sit in `Phase::Exiting` until the
        // session driver acknowledges the save (or we're not
        // primary, in which case the ingest above already set
        // `session.saved = true`). Standalone runs fall out
        // immediately too — `init_done` defaults to true via
        // the no-config-dir path, and `saved` is true by
        // default.
        if matches!(lifecycle.phase, Phase::Exiting)
            && (session.saved || !session.primary)
        {
            // Drop the session driver cleanly. Sending Shutdown
            // is best-effort — the worker also self-exits when
            // its Sender hangs up at process exit.
            drivers
                .session
                .execute(std::iter::once(&SessionCmd::Shutdown));
            break Ok(());
        }

        // Browser selection snap: when the active tab changed,
        // pin `selected_path` to its path. Path-based selection
        // means the `browser_entries` memo resolves to the right
        // row automatically once fs-list delivers the ancestor
        // listings. Skip when focus is on the side panel — the
        // user is arrow-navigating; don't yank the cursor.
        if !matches!(browser.focus, led_state_browser::Focus::Side) {
            let active_path_now = tabs
                .active
                .and_then(|id| tabs.open.iter().find(|t| t.id == id))
                .map(|t| t.path.clone());
            if active_path_now.is_some() && browser.selected_path != active_path_now {
                browser.selected_path = active_path_now;
            }
        }

        // ── Query ───────────────────────────────────────────────
        let load_actions = file_load_action(
            StoreLoadedInput::new(store),
            TabsOpenInput::new(tabs),
        );
        let save_actions = file_save_action(
            PendingSavesInput::new(edits),
            EditedBuffersInput::new(edits),
        );
        let list_actions = file_list_action(query::BrowserDerivedInputs {
            fs: FsTreeInput::new(fs),
            ui: BrowserUiInput::new(browser),
            tabs: TabsActiveInput::new(tabs),
            edits: EditedBuffersInput::new(edits),
        });
        let find_file_actions = find_file_action(FindFileInput::new(find_file));
        // Spinner frame clock — current millis since UNIX epoch,
        // quantised to 80ms buckets. Pinned to `0` when no LSP
        // server is busy so the render_frame memo stays warm
        // instead of invalidating every tick.
        let render_tick = if lsp_status.any_busy() {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64 / 80)
                .unwrap_or(0)
        } else {
            0
        };
        let frame = render_frame(query::RenderInputs {
            term: TerminalDimsInput::new(terminal),
            edits: EditedBuffersInput::new(edits),
            store: StoreLoadedInput::new(store),
            tabs: TabsActiveInput::new(tabs),
            alerts: AlertsInput::new(alerts),
            browser: BrowserUiInput::new(browser),
            fs: FsTreeInput::new(fs),
            overlays: query::OverlaysInput::new(find_file, isearch, file_search),
            syntax: query::SyntaxStatesInput::new(syntax),
            diagnostics: query::DiagnosticsStatesInput::new(diagnostics),
            lsp: query::LspStatusesInput::new(lsp_status),
            completions: query::CompletionsSessionInput::new(completions),
            lsp_extras: query::LspExtrasOverlayInput::new(lsp_extras),
            git: query::GitStateInput::new(git),
            render_tick,
            kbd_macro: query::KbdMacroRecordingInput::new(kbd_macro),
        });

        // ── Execute ─────────────────────────────────────────────
        // Directory listings go first so goldens' dispatched.snap
        // order matches legacy (FsListDir before FileOpen).
        drivers.fs_list.execute(list_actions.iter());

        drivers.file.execute(load_actions.iter(), store);

        // Find-file completion requests. Sync-clear the queue
        // BEFORE execute so a late wake that runs the query again
        // doesn't re-fire the same requests. Matches the save
        // pattern above.
        if !find_file_actions.is_empty()
            && let Some(ff) = find_file.as_mut()
        {
            ff.pending_find_file_list.clear();
        }
        drivers.find_file.execute(find_file_actions.iter());

        // File-search queued requests (M14). Build one
        // `FileSearchCmd` per queued edit/toggle, ship via the
        // driver (which emits the `FileSearch` trace line), and
        // sync-clear the queue. Root is the workspace (`fs.root`);
        // without a root the queue is dropped silently — M11 sets
        // `fs.root` to CWD at startup, so normal use has a root.
        if let Some(fs_state) = file_search.as_mut()
            && !fs_state.pending_search.is_empty()
        {
            if let Some(root) = fs.root.as_ref() {
                let cmds: Vec<FileSearchCmd> = fs_state
                    .pending_search
                    .drain(..)
                    .map(|req| FileSearchCmd {
                        root: root.clone(),
                        query: req.query,
                        case_sensitive: req.case_sensitive,
                        use_regex: req.use_regex,
                    })
                    .collect();
                drivers.file_search.execute(cmds.iter());
            } else {
                fs_state.pending_search.clear();
            }
        }

        // Replace-all: drain the dispatch-queued on-disk requests,
        // ship to the driver. Survives overlay deactivation because
        // the queue lives on `BufferEdits`. Each cmd becomes one
        // `FileSearchReplace` trace line.
        if !edits.pending_replace_all.is_empty() {
            let cmds: Vec<led_driver_file_search_core::FileSearchReplaceCmd> = edits
                .pending_replace_all
                .drain(..)
                .map(|p| led_driver_file_search_core::FileSearchReplaceCmd {
                    root: p.root,
                    query: p.query,
                    replacement: p.replacement,
                    case_sensitive: p.case_sensitive,
                    use_regex: p.use_regex,
                    skip_paths: p.skip_paths,
                })
                .collect();
            drivers.file_search.execute_replace(cmds.iter());
        }

        // Per-hit on-disk replaces: drain + ship. One
        // `FileSearchSingleReplace` trace line each.
        if !edits.pending_single_replace.is_empty() {
            let cmds: Vec<led_driver_file_search_core::FileSearchSingleReplaceCmd> = edits
                .pending_single_replace
                .drain(..)
                .map(|p| led_driver_file_search_core::FileSearchSingleReplaceCmd {
                    path: p.path,
                    line: p.line,
                    match_start: p.match_start,
                    match_end: p.match_end,
                    original: p.original,
                    replacement: p.replacement,
                })
                .collect();
            drivers.file_search.execute_single_replace(cmds.iter());
        }
        // Drain single-replace completions — runtime doesn't need
        // to act on them beyond the trace (the display was already
        // updated optimistically). A future iteration could alert
        // on failure (stale hit / file gone).
        let _ = drivers.file_search.process_single_replace();

        // Sync-clear pending_saves + pending_save_as for the paths
        // we're about to dispatch — the execute-pattern discipline
        // that prevents the next tick's query from re-emitting the
        // same saves.
        for action in &save_actions {
            match action {
                led_driver_buffers_core::SaveAction::Save { path, .. } => {
                    edits.pending_saves.remove(path);
                }
                led_driver_buffers_core::SaveAction::SaveAs { from, .. } => {
                    edits.pending_save_as.remove(from);
                }
            }
        }
        drivers.file_write.execute(save_actions.iter());

        // Emit the paired `WorkspaceClearUndo` trace for every
        // save. Legacy's semantic here is "drop the buffer's
        // persisted undo entries from SQLite" — NOT "wipe the
        // in-memory undo stack". The in-memory history stays
        // intact across saves so the user can still Ctrl-/ a
        // format-on-save or any other post-save state back out.
        // M21 wires the disk-side drop against the real session
        // DB; this trace line is the dispatched-intent record.
        //
        // SaveAs uses `from` (the source buffer whose content was
        // saved), not `to` — the target is a fresh file on disk
        // that has no undo history of its own yet. SaveAs also
        // emits a `FileOpen path=<from> create_if_missing=false`
        // trace line after the clear-undo, matching legacy's
        // re-open-source-on-disk behaviour (the buffer keeps its
        // in-memory rope; the trace line is the intent record).
        for action in &save_actions {
            let (path, is_save_as) = match action {
                led_driver_buffers_core::SaveAction::Save { path, .. } => (path, false),
                led_driver_buffers_core::SaveAction::SaveAs { from, .. } => (from, true),
            };
            // Drop the persisted undo blob for this path. The
            // saved bytes are now the disk baseline, so the
            // previously-stored undo (computed against the
            // pre-save content) is stale relative to disk. The
            // in-memory `eb.history` stays intact — the user
            // can still Ctrl-/ as before. The driver's adapter
            // emits the `WorkspaceClearUndo` trace line.
            drivers
                .session
                .execute(std::iter::once(&SessionCmd::ClearUndo {
                    path: path.clone(),
                }));
            // Reset the per-buffer flush tracker: legacy's
            // `save_completed` clears `chain_id` and sets
            // `persisted_undo_len = entry_count`, so the existing
            // past becomes the new disk baseline. The next user
            // edit opens a fresh chain whose flushes start from
            // the just-saved point.
            if let Some(eb) = edits.buffers.get(path) {
                undo_persistence.insert(
                    path.clone(),
                    UndoPersistTracker {
                        chain_id: new_chain_id(),
                        persisted_len: eb.history.past_groups().len(),
                        last_seq: 0,
                    },
                );
            }
            if is_save_as {
                trace.file_reopen_existing(path);
            }
        }

        // Syntax parse dispatch. For every buffer whose language
        // we've identified and whose current version is ahead of
        // (a) the last-applied tokens' version AND (b) any parse
        // currently in flight, ship a `SyntaxCmd` to the worker
        // and mark `in_flight_version`. The worker coalesces stale
        // cmds internally, but tracking in-flight on our side
        // avoids stuffing the channel on idle ticks.
        let mut syntax_cmds: Vec<SyntaxCmd> = Vec::new();
        for (path, state) in syntax.by_path.iter_mut() {
            let Some(eb) = edits.buffers.get(path) else {
                continue;
            };
            // Needs a parse if we've never parsed this buffer OR the
            // rope has moved past the last-applied tokens. The
            // initial load sits at `eb.version == state.version == 0`,
            // so without the `tree.is_none()` branch the first parse
            // would never fire — colours would only appear after the
            // user typed their first character.
            let needs_parse = state.tree.is_none() || eb.version > state.version;
            if !needs_parse {
                continue;
            }
            if state.in_flight_version == Some(eb.version) {
                continue;
            }
            // Ship the previous tree + the rope it was parsed
            // from. The worker derives the edit purely from
            // `(prev_rope, cmd.rope)` — no history-counter
            // bookkeeping, so undo/redo can't desynchronise the
            // tree from the bytes. When `tree_rope` is absent
            // (first parse, or the previous parse errored),
            // the worker falls back to a full parse.
            syntax_cmds.push(SyntaxCmd {
                path: path.clone(),
                version: eb.version,
                rope: eb.rope.clone(),
                language: state.language,
                prev_tree: state.tree.clone(),
                prev_rope: state.tree_rope.clone(),
            });
            state.in_flight_version = Some(eb.version);
        }
        if !syntax_cmds.is_empty() {
            drivers.syntax.execute(syntax_cmds.iter());
        }

        // ── LSP dispatch ──────────────────────────────────────
        //
        // One-time `Init` once the workspace root is known; then
        // on each tick: emit `BufferChanged` for any buffer whose
        // `version` has moved since the last notification, and a
        // single `RequestDiagnostics` if the state-sum
        // (Σ version + saved_version) moved. Manager-side window
        // discipline coalesces spammy request calls, so being
        // eager here is fine.
        if !*lsp_init_sent
            && let Some(root) = fs.root.as_ref()
        {
            drivers.lsp.execute(std::iter::once(&LspCmd::Init {
                root: root.clone(),
            }));
            *lsp_init_sent = true;
        }

        // ── Session dispatch (M21) ─────────────────────────────
        //
        // Init: once per session, when fs.root is known. The
        // `session.init_done` flag flips on the matching
        // SessionEvent::Restored so we don't double-fire.
        if !session.init_done
            && let Some(root) = fs.root.as_ref()
        {
            // Config dir = `--config-dir` if the CLI supplied
            // one (the goldens harness relies on this for
            // hermetic per-test SQLite + flock isolation),
            // otherwise `$XDG_CONFIG_HOME/led` →
            // `$HOME/.config/led`. Same source as the keymap /
            // theme loaders.
            let cfg = cli_config_dir
                .and_then(|p| {
                    std::fs::create_dir_all(p).ok()?;
                    Some(led_core::UserPath::new(p).canonicalize())
                })
                .or_else(config_dir_for_session);
            if let Some(cfg) = cfg {
                drivers.session.execute(std::iter::once(&SessionCmd::Init {
                    root: root.clone(),
                    config_dir: cfg,
                }));
                // Mark init_done eagerly so we don't re-fire
                // before the reply arrives. The reply will set
                // primary + last_saved when it lands.
                session.init_done = true;
            } else {
                // No config dir resolvable — treat as no-op so
                // the Quit gate can still clear.
                session.init_done = true;
                session.saved = true;
            }
        }

        // Save: once per Phase::Exiting transition for primary
        // workspaces. The flag prevents repeat dispatches while
        // we wait for the SessionEvent::Saved ack.
        if matches!(lifecycle.phase, Phase::Exiting)
            && session.primary
            && !session.saved
            && !*session_save_dispatched
        {
            let data = build_session_data(tabs, edits, store, browser, jumps);
            drivers.session.execute(std::iter::once(&SessionCmd::SaveSession {
                data,
            }));
            *session_save_dispatched = true;
        } else if matches!(lifecycle.phase, Phase::Exiting)
            && !session.primary
        {
            // Secondaries and standalone runs have nothing to
            // save; clear the gate immediately.
            session.saved = true;
        }

        // FlushUndo: per-tick incremental append of newly-finalised
        // undo groups. Mirrors legacy's `pending_undo_flush` query
        // (`led/src/model/mod.rs` ~line 399). Only primaries own
        // the SQLite file; secondaries are read-only. We cap the
        // walk at `tabs.open` because legacy only persists undo
        // for tabbed buffers (preview tabs are excluded; we don't
        // surface a preview flag yet, so every open tab is fair
        // game).
        // Clipboard actions go BEFORE the per-tick FlushUndo so
        // the trace order matches legacy: a kill_line trace
        // sequence reads `ClipboardWrite … WorkspaceFlushUndo …
        // ClipboardRead`. Legacy emits the clipboard write
        // synchronously inside dispatch (i.e. before its
        // debounced flush), so dispatching the clipboard side-
        // effect ahead of flush in the same tick reproduces the
        // same wire order. Read when a yank is pending (no read
        // already in flight); Write when a kill queued clipboard
        // text. Both flags cleared synchronously per the execute
        // pattern.
        let clip_action = clipboard_action(ClipboardStateInput::new(clip));
        match clip_action {
            Some(ClipboardAction::Read) => {
                clip.read_in_flight = true;
                drivers.clipboard.execute([&ClipboardAction::Read]);
            }
            Some(ClipboardAction::Write(_)) => {
                let text = clip.pending_write.take().expect("memo agreed write");
                drivers.clipboard.execute([&ClipboardAction::Write(text)]);
            }
            None => {}
        }

        // FlushUndo dispatches in BOTH primary and standalone
        // modes — the trace fires unconditionally so goldens see
        // the same `WorkspaceFlushUndo` lines on either side. The
        // session driver's `FlushUndo` handler skips the SQLite
        // write when we're not primary, so secondaries / standalone
        // don't corrupt anyone's DB.
        //
        // Per-buffer 200ms debounce mirrors legacy's
        // `KeepExisting` timer: the first version bump arms the
        // window, subsequent bumps reset it, and the flush fires
        // 200ms after the LAST edit. Short edit-then-quit scripts
        // (delete_backward, insert_newline, …) settle before the
        // window expires, so no FlushUndo trace fires — matching
        // the legacy goldens that captured them.
        let now = Instant::now();
        let debounce = Duration::from_millis(200);
        if session.init_done {
            for tab in tabs.open.iter() {
                let path = &tab.path;
                let Some(eb) = edits.buffers.get(path) else {
                    continue;
                };
                let current_len = eb.history.past_groups().len();
                let tracker = undo_persistence
                    .entry(path.clone())
                    .or_insert_with(|| UndoPersistTracker {
                        chain_id: new_chain_id(),
                        persisted_len: 0,
                        last_seq: 0,
                    });
                if current_len <= tracker.persisted_len {
                    continue;
                }
                // Update the debounce window when the version
                // moves; reuse the existing window on idle ticks.
                let entry = undo_flush_debounce
                    .entry(path.clone())
                    .or_insert((eb.version, now));
                if entry.0 != eb.version {
                    *entry = (eb.version, now);
                }
                if now < entry.1 + debounce {
                    continue;
                }
                let new_groups: Vec<EditGroup> = eb
                    .history
                    .past_groups()[tracker.persisted_len..current_len]
                    .to_vec();
                if new_groups.iter().all(|g| g.ops.is_empty()) {
                    // Nothing but save-point markers since last
                    // flush — advance the cursor so we don't
                    // re-walk them, but don't ship an empty payload.
                    tracker.persisted_len = current_len;
                    undo_flush_debounce.remove(path);
                    continue;
                }
                let content_hash = disk_content_hash_for(eb);
                let distance = distance_from_save_for(eb);
                let chain_id = tracker.chain_id.clone();
                drivers.session.execute(std::iter::once(
                    &SessionCmd::FlushUndo {
                        path: path.clone(),
                        chain_id,
                        content_hash,
                        undo_cursor: current_len,
                        distance_from_save: distance,
                        entries: new_groups,
                    },
                ));
                // Tentatively advance — `UndoFlushed` will confirm
                // last_seq and re-pin persisted_len to the value
                // the driver inserted (legacy treats the ack as
                // authoritative).
                tracker.persisted_len = current_len;
                undo_flush_debounce.remove(path);
            }
        }

        let mut lsp_cmds: Vec<LspCmd> = Vec::new();
        for (path, eb) in edits.buffers.iter() {
            let current_version = eb.version;
            let (last_version, last_saved) = lsp_notified
                .get(path)
                .copied()
                .unwrap_or_default();
            let version_moved = current_version > last_version;
            let save_happened = eb.saved_version > last_saved;
            if version_moved || save_happened {
                // `is_save` = the writer reported this tick
                // (saved_version advanced AND it has caught up
                // to version). Separate from `version_moved`
                // because a pure-save tick (no new edits) still
                // needs `didSave` → cargo check.
                let is_save = save_happened && eb.saved_version == eb.version;
                let hash = led_core::EphemeralContentHash::of_rope(&eb.rope).persist();
                lsp_cmds.push(LspCmd::BufferChanged {
                    path: path.clone(),
                    rope: eb.rope.clone(),
                    hash,
                    is_save,
                });
                lsp_notified.insert(path.clone(), (current_version, eb.saved_version));
            }
        }
        // RequestDiagnostics emission — unified version of
        // legacy's two rx streams (hash-sum delta + phase→Running
        // one-shot, see docs/rewrite/lsp-patterns.md §6.3).
        //
        // `buffer_state_sum` memo derives Σ(version + saved_version);
        // `lsp_requested_state_sum` atom stores the sum at our
        // last emission. `Some(current) != *lsp_requested_state_sum`
        // covers both "sum moved" and "first ever fire" (None on
        // startup → not equal to any Some). Gated on
        // `!lsp_notified.is_empty()` so we don't fire diagnostic
        // requests for a workspace with no buffers yet.
        let current_sum = query::buffer_state_sum(EditedBuffersInput::new(edits));
        let should_request_diag =
            !lsp_notified.is_empty() && Some(current_sum) != *lsp_requested_state_sum;
        if should_request_diag {
            lsp_cmds.push(LspCmd::RequestDiagnostics);
            *lsp_requested_state_sum = Some(current_sum);
        }
        // Drain queued completion requests. Dispatch populated
        // `pending_requests` on identifier-char inserts; we flush
        // each into `LspCmd::RequestCompletion` here, preserving
        // the pre-allocated `seq` so server responses round-trip
        // back to their originating session unambiguously.
        for req in completions_pending.pending_requests.drain(..) {
            lsp_cmds.push(LspCmd::RequestCompletion {
                path: req.path,
                seq: req.seq,
                line: req.line,
                col: req.col,
                trigger: req.trigger,
            });
        }
        for resolve in completions_pending.pending_resolves.drain(..) {
            lsp_cmds.push(LspCmd::ResolveCompletion {
                path: resolve.path,
                seq: resolve.seq,
                item: resolve.item,
            });
        }
        // M18 goto-definition outbox.
        for req in lsp_pending.pending_goto.drain(..) {
            lsp_cmds.push(LspCmd::RequestGotoDefinition {
                path: req.path,
                seq: req.seq,
                line: req.line,
                col: req.col,
            });
        }
        // M18 rename outbox.
        for req in lsp_pending.pending_rename.drain(..) {
            lsp_cmds.push(LspCmd::RequestRename {
                path: req.path,
                seq: req.seq,
                line: req.line,
                col: req.col,
                new_name: req.new_name,
            });
        }
        // M18 code-action request outbox.
        for req in lsp_pending.pending_code_action.drain(..) {
            lsp_cmds.push(LspCmd::RequestCodeAction {
                path: req.path,
                seq: req.seq,
                start_line: req.start_line,
                start_col: req.start_col,
                end_line: req.end_line,
                end_col: req.end_col,
            });
        }
        // M18 code-action commit outbox.
        for req in lsp_pending.pending_code_action_select.drain(..) {
            lsp_cmds.push(LspCmd::SelectCodeAction {
                path: req.path,
                seq: req.seq,
                action: req.action,
            });
        }
        // M18 inlay-hints: queue a request per active buffer
        // whose `(path, version)` hasn't been asked yet. The
        // viewport range is whole-buffer for the first cut —
        // legacy's scroll-bucket dedupe (viewport±10 rows,
        // bucketed by scroll_row/5) stays parked. Hint
        // rendering isn't wired in this stage so the server
        // round-trip happens but the data sits unused; the
        // painter pickup lands with the body-model refactor.
        // M18 format outbox.
        for req in lsp_pending.pending_format.drain(..) {
            lsp_cmds.push(LspCmd::RequestFormat {
                path: req.path,
                seq: req.seq,
            });
        }
        if lsp_extras.inlay_hints_enabled {
            for (path, eb) in edits.buffers.iter() {
                let version = eb.version;
                if lsp_pending
                    .inlay_hints_requested
                    .contains(&(path.clone(), version))
                {
                    continue;
                }
                let end_line = eb
                    .rope
                    .len_lines()
                    .saturating_sub(1)
                    .min(u32::MAX as usize) as u32;
                lsp_pending.queue_inlay_hints(path.clone(), version, 0, end_line);
            }
            for req in lsp_pending.pending_inlay_hint.drain(..) {
                lsp_cmds.push(LspCmd::RequestInlayHints {
                    path: req.path,
                    seq: req.seq,
                    version: req.version,
                    start_line: req.start_line,
                    end_line: req.end_line,
                });
            }
        } else {
            lsp_pending.pending_inlay_hint.clear();
        }
        if !lsp_cmds.is_empty() {
            drivers.lsp.execute(lsp_cmds.iter());
        }

        // ── Git dispatch ───────────────────────────────────────
        //
        // Startup one-shot (plus per-save re-fire) gated on a
        // `.git/` entry existing under the workspace root. The
        // gate matches legacy's "command is never emitted in
        // standalone / no-workspace mode" contract (spec
        // `git.md`): non-repo workspaces don't spam `GitScan`
        // trace lines, and libgit2 doesn't churn on open-fail.
        //
        // When the timers driver lands (post-M19), insert a
        // Replace(50ms) gate between the flag bump and the
        // drain here — no other call-site changes. Until then,
        // saves are user-paced so the at-most-one-per-save rate
        // stays well below legacy's 50ms debounce target.
        if let Some(root) = fs.root.as_ref() {
            // Drain the per-save flag regardless of startup
            // state so a save mid-session doesn't leave it
            // sticky. Combine with the "never scanned yet"
            // condition to decide whether to actually dispatch.
            let save_pending = std::mem::take(git_scan_pending);
            // Hold the FIRST scan until pending CLI-arg loads have
            // landed so any ancestor-reveal listing (driven by the
            // file-completion handler above) shipped this tick wins
            // the trace ordering — mirrors legacy's 50ms debounce
            // (`docs/spec/git.md` §"Debounced rescan on activity"),
            // which delays the initial scan past the workspace +
            // arg-file load burst. After the latch flips, save-
            // triggered scans fire on the same tick as the save.
            let any_pending_load = tabs
                .open
                .iter()
                .any(|t| !edits.buffers.contains_key(&t.path));
            let initial_scan_ready = *git_scan_dispatched || !any_pending_load;
            let want_scan = !*git_scan_dispatched || save_pending;
            if want_scan && initial_scan_ready && root.as_path().join(".git").exists() {
                drivers.git.execute(std::iter::once(&GitCmd::ScanFiles {
                    root: root.clone(),
                }));
                *git_scan_dispatched = true;
            } else if want_scan && initial_scan_ready {
                // Not a repo — flip the latch so we don't
                // re-check `.git/` every tick.
                *git_scan_dispatched = true;
            } else if save_pending {
                // Restore the save-pending flag if we deferred:
                // the next tick will re-enter and try again.
                *git_scan_pending = true;
            }
        } else {
            // No workspace root yet — discard the pending flag
            // silently so a future workspace load doesn't
            // double-fire.
            *git_scan_pending = false;
        }

        // ── Render ──────────────────────────────────────────────
        if frame != last_frame {
            if let Some(f) = &frame {
                drivers.output.execute(f, last_frame.as_ref(), theme, stdout)?;
                // First successful paint graduates the process
                // out of Starting. M21 reintroduces `Resuming`
                // between Starting and Running to gate rendering
                // on session-restore materialisation; for M20 the
                // first frame IS the transition.
                if lifecycle.phase == Phase::Starting {
                    lifecycle.phase = Phase::Running;
                }
            }
            last_frame = frame;
        }

        // ── Block until something happens ───────────────────────
        // Order matters: block FIRST, then collapse any additional
        // signals that piled up while we were working on THIS tick
        // or blocking. If we drained before blocking, a key event
        // arriving in the narrow window between the terminal drain
        // above and this drain would consume the wake signal
        // without getting its work done; the next key would then
        // wait the full timeout. That was the visible stutter when
        // holding a key — key-repeat events race with the drain.
        let timeout = nearest_deadline(alerts, find_file, lsp_status, undo_flush_debounce)
            .and_then(|d| d.checked_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(60));
        use std::sync::mpsc::RecvTimeoutError;
        match wake.rx.recv_timeout(timeout) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        }
        // Collapse any extra signals queued during the above —
        // they'll all be handled by the next iteration's single
        // drain of each driver's own channel.
        while wake.rx.try_recv().is_ok() {}
    }
}

/// Resolve the per-user config directory the session driver
/// stores `db.sqlite` and `primary/<hash>` under. Honours
/// `XDG_CONFIG_HOME` like the keymap/theme loaders, otherwise
/// `~/.config/led/`. Returns `None` when neither is resolvable
/// (CI sandboxes, etc.) — the runtime treats that as
/// "session is a no-op", same as standalone mode.
fn config_dir_for_session() -> Option<CanonPath> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        let path = std::path::PathBuf::from(xdg).join("led");
        std::fs::create_dir_all(&path).ok()?;
        return Some(led_core::UserPath::new(path).canonicalize());
    }
    let home = std::env::var_os("HOME")?;
    let path = std::path::PathBuf::from(home).join(".config").join("led");
    std::fs::create_dir_all(&path).ok()?;
    Some(led_core::UserPath::new(path).canonicalize())
}

/// Build the [`SessionData`] payload from the live atom set.
/// Mirrors legacy's session-on-quit assembly: one
/// `SessionBuffer` per non-preview tab (cursor + scroll), plus
/// the active-tab order, the side-panel toggle, and any kv pairs
/// the runtime collected (browser state, jump list, etc. — those
/// will arrive in a follow-up; the slot is here today).
///
/// The undo-flush + ClearUndo flow lives separately: legacy
/// flushes undo on a debounce timer (and on any save) via a
/// distinct WorkspaceOut::FlushUndo command; our `SaveSession`
/// is just the workspaces / buffers / kv portion.
fn build_session_data(
    tabs: &Tabs,
    _edits: &BufferEdits,
    _store: &led_driver_buffers_core::BufferStore,
    browser: &led_state_browser::BrowserUi,
    jumps: &led_state_jumps::JumpListState,
) -> SessionData {
    let mut session_buffers: Vec<SessionBuffer> =
        Vec::with_capacity(tabs.open.len());
    let mut active_tab_order: usize = 0;
    for tab in tabs.open.iter() {
        if tab.preview {
            continue;
        }
        if Some(tab.id) == tabs.active {
            active_tab_order = session_buffers.len();
        }
        session_buffers.push(SessionBuffer {
            path: tab.path.clone(),
            tab_order: session_buffers.len(),
            cursor: tab.cursor,
            scroll: tab.scroll,
            undo: None,
        });
    }
    SessionData {
        active_tab_order,
        show_side_panel: browser.visible,
        buffers: session_buffers,
        kv: build_session_kv(browser, jumps),
    }
}

/// Inverse of [`build_session_kv`]: re-hydrates the browser +
/// jump-list atoms from the kv blob the driver loaded out of
/// `session_kv`. Legacy's equivalent is `model::session_of`.
/// Unknown keys are tolerated; type-mismatched values fall back
/// to defaults so a corrupted row doesn't block the restore.
fn apply_session_kv(
    kv: &std::collections::HashMap<String, String>,
    browser: &mut led_state_browser::BrowserUi,
    jumps: &mut led_state_jumps::JumpListState,
) {
    if let Some(sel) = kv.get("browser.selected_path") {
        browser.selected_path = Some(
            led_core::UserPath::new(std::path::PathBuf::from(sel))
                .canonicalize(),
        );
    }
    if let Some(off) = kv.get("browser.scroll_offset")
        && let Ok(n) = off.parse::<usize>()
    {
        browser.scroll_offset = n;
    }
    if let Some(dirs) = kv.get("browser.expanded_dirs") {
        browser.expanded_dirs = dirs
            .split('\n')
            .filter(|s| !s.is_empty())
            .map(|s| {
                led_core::UserPath::new(std::path::PathBuf::from(s))
                    .canonicalize()
            })
            .collect();
    }
    if let Some(json) = kv.get("jump_list.entries")
        && let Ok(entries) =
            serde_json::from_str::<std::collections::VecDeque<
                led_state_jumps::JumpPosition,
            >>(json)
    {
        jumps.entries = entries;
        if let Some(idx) = kv.get("jump_list.index")
            && let Ok(n) = idx.parse::<usize>()
        {
            jumps.index = n.min(jumps.entries.len());
        } else {
            jumps.index = jumps.entries.len();
        }
    }
}

/// Mirrors legacy's `build_session_kv` (`led/src/derived.rs`).
/// Browser selection / scroll / expanded set + jump-list entries
/// + index, encoded as plain string values so the schema row stays
///   stable across rewrite-internal type churn.
fn build_session_kv(
    browser: &led_state_browser::BrowserUi,
    jumps: &led_state_jumps::JumpListState,
) -> std::collections::HashMap<String, String> {
    let mut kv = std::collections::HashMap::new();
    if let Some(sel) = &browser.selected_path {
        kv.insert(
            "browser.selected_path".into(),
            sel.as_path().to_string_lossy().into_owned(),
        );
    }
    kv.insert(
        "browser.scroll_offset".into(),
        browser.scroll_offset.to_string(),
    );
    let dirs: Vec<String> = browser
        .expanded_dirs
        .iter()
        .map(|d| d.as_path().to_string_lossy().into_owned())
        .collect();
    if !dirs.is_empty() {
        kv.insert("browser.expanded_dirs".into(), dirs.join("\n"));
    }
    if let Ok(json) = serde_json::to_string(&jumps.entries) {
        kv.insert("jump_list.entries".into(), json);
        kv.insert("jump_list.index".into(), jumps.index.to_string());
    }
    kv
}

/// Apply a stashed [`UndoRestoreData`] to a now-materialised
/// buffer: replay each `EditGroup`'s ops forward onto the rope,
/// install the restored chain into `eb.history.past`, and seed
/// the per-buffer flush tracker so subsequent `FlushUndo`
/// commands resume from the restored tail.
///
/// Two callers:
/// - the load-completion ingest hook (first-time materialise
///   path; runs once per buffer per session)
/// - the `SessionEvent::Restored` arm (CLI-arg buffers that
///   loaded BEFORE Init replied — `inserted` was true on a tick
///   where `pending_undo` was still empty, so the restore data
///   has to be applied retroactively here)
///
/// Returns silently when the disk-hash gate fails (file
/// changed externally between sessions) — the chain stays in
/// `pending_undo`'s now-removed slot, effectively dropped.
fn apply_pending_undo_restore(
    path: &CanonPath,
    edits: &mut BufferEdits,
    session: &mut led_state_session::SessionState,
    undo_persistence: &mut std::collections::HashMap<CanonPath, UndoPersistTracker>,
) {
    let Some(restore) = session.pending_undo.remove(path) else {
        return;
    };
    let Some(eb) = edits.buffers.get_mut(path) else {
        return;
    };
    if eb.disk_content_hash != restore.content_hash {
        return;
    }
    let mut new_rope = (*eb.rope).clone();
    for group in &restore.entries {
        for op in &group.ops {
            use led_state_buffer_edits::EditOp;
            match op {
                EditOp::Delete { at, text } => {
                    let len = text.chars().count();
                    let end = (*at + len).min(new_rope.len_chars());
                    if *at < new_rope.len_chars() && end > *at {
                        new_rope.remove(*at..end);
                    }
                }
                EditOp::Insert { at, text } => {
                    let pos = (*at).min(new_rope.len_chars());
                    new_rope.insert(pos, text);
                }
            }
        }
    }
    eb.rope = std::sync::Arc::new(new_rope);
    if !restore.entries.is_empty() {
        eb.version = eb.version.saturating_add(1);
    }
    let mut history = led_state_buffer_edits::History::with_seq_gen(
        edits.seq_gen.clone(),
    );
    history.restore_past(restore.entries.clone());
    eb.history = history;
    undo_persistence.insert(
        path.clone(),
        UndoPersistTracker {
            chain_id: restore.chain_id.clone(),
            persisted_len: restore.entries.len(),
            last_seq: restore.last_seq,
        },
    );
}

/// Generate a unique `chain_id` for an undo persistence chain.
/// Mirrors legacy's `led_workspace::new_chain_id` — 64-bit hash
/// of (now, pid). Collision-safe enough for a per-buffer
/// session marker; not cryptographic.
fn new_chain_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::SystemTime;
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut hasher = DefaultHasher::new();
    t.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    UNDO_CHAIN_NONCE
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

static UNDO_CHAIN_NONCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Hash that anchors the chain to "what's on disk." Returns
/// `eb.disk_content_hash`, which is set at load completion (the
/// rope at version 0 IS the disk content) and refreshed at save
/// completion (the just-written rope is the new disk content).
/// Mirrors legacy `BufferState::content_hash` — used so the
/// next-launch restore can re-hash the disk file and refuse to
/// replay if the bytes shifted between sessions.
fn disk_content_hash_for(eb: &EditedBuffer) -> led_core::PersistedContentHash {
    eb.disk_content_hash
}

/// Distance (in finalised groups) between the current head and
/// the most recent save-point marker. Used by legacy's `buffer_
/// undo_state.distance_from_save` for on-restore conflict
/// detection. We compute it on demand from `past`; legacy tracks
/// it incrementally on the doc, but the values agree at flush
/// time so the on-disk row is identical.
fn distance_from_save_for(eb: &EditedBuffer) -> i32 {
    let past = eb.history.past_groups();
    let last_save_idx = past
        .iter()
        .rposition(|g| g.save_point_hash.is_some());
    let after = match last_save_idx {
        Some(idx) => &past[idx + 1..],
        None => past,
    };
    after.iter().filter(|g| !g.ops.is_empty()).count() as i32
}

/// Min-fold over every currently-registered wake deadline.
///
/// Course-correct #8: isolates the "when should the main loop next
/// wake up?" decision from `alerts.info_expires_at` so future
/// timer sources (diagnostics debouncing M12, command-palette
/// animation M13, LSP completion timeouts M18, file-watch debounce
/// M26) plug in without touching the main-loop shape.
///
/// Takes individual `&` refs (not `&Atoms`) so call sites inside
/// `run()` can use it alongside the disjoint-field `&mut` borrows
/// that dispatch needs. Not a drv memo — the inputs change every
/// tick and the fold is trivially cheap; caching would churn.
pub fn nearest_deadline(
    alerts: &AlertState,
    find_file: &Option<FindFileState>,
    lsp_status: &LspStatuses,
    undo_flush_debounce: &std::collections::HashMap<CanonPath, (u64, Instant)>,
) -> Option<Instant> {
    let mut soonest: Option<Instant> = None;
    let consider = |soonest: &mut Option<Instant>, candidate: Option<Instant>| {
        if let Some(t) = candidate {
            *soonest = match *soonest {
                Some(cur) if cur < t => Some(cur),
                _ => Some(t),
            };
        }
    };
    consider(&mut soonest, alerts.info_expires_at);
    consider(
        &mut soonest,
        find_file.as_ref().and_then(|ff| ff.input.hint_expires_at),
    );
    // LSP spinner animation — 80ms cadence while any server is
    // busy. Matches legacy's `format_lsp_status` spinner (10
    // braille frames, each 80ms). Without this wake source the
    // status-bar spinner would freeze between user events.
    if lsp_status.any_busy() {
        consider(
            &mut soonest,
            Some(Instant::now() + std::time::Duration::from_millis(80)),
        );
    }
    // Undo-flush debounce — wake at the earliest pending window
    // expiry so the per-tick flush check fires the trace on time
    // even when no other event is in flight.
    let debounce = std::time::Duration::from_millis(200);
    if let Some(earliest) = undo_flush_debounce
        .values()
        .map(|(_, seen_at)| *seen_at + debounce)
        .min()
    {
        consider(&mut soonest, Some(earliest));
    }
    soonest
}

/// Seed the edit-buffer map from a newly-Ready FS read. The
/// discipline (course-correct #6): an existing entry in `edits`
/// represents the user's edited view of that buffer and is
/// authoritative — a late load completion for the same path is
/// discarded. Returns `true` when a new entry was inserted,
/// `false` when the existing entry absorbed the discard.
/// Extract the typed prefix the user's cursor is parked at for an
/// incoming `LspEvent::Completion`. Used by ingest to refilter the
/// server response against the current buffer state before
/// installing the session — without this, items appear
/// unfiltered for one frame until the next keystroke.
/// Walk left from `cursor_col` on `prefix_line` while characters
/// are identifier-like (alphanumeric or `_`). The returned col is
/// the first identifier char — `cursor_col` itself when the char
/// to the left isn't identifier-like, `0` when the line begins
/// with an unbroken run. Used as the fallback for completion
/// responses where the server didn't carry a `textEdit.range`
/// (legacy `convert_completion_response`).
fn identifier_start_col(
    edits: &BufferEdits,
    path: &CanonPath,
    prefix_line: usize,
    cursor_col: usize,
) -> u32 {
    let Some(eb) = edits.buffers.get(path) else {
        return cursor_col as u32;
    };
    if prefix_line >= eb.rope.len_lines() {
        return cursor_col as u32;
    }
    let line_start = eb.rope.line_to_char(prefix_line);
    let line_end = if prefix_line + 1 < eb.rope.len_lines() {
        eb.rope.line_to_char(prefix_line + 1)
    } else {
        eb.rope.len_chars()
    };
    let line_len = line_end - line_start;
    let mut start = cursor_col.min(line_len);
    while start > 0 {
        let ch = eb.rope.char(line_start + start - 1);
        if ch.is_alphanumeric() || ch == '_' {
            start -= 1;
        } else {
            break;
        }
    }
    start as u32
}

fn completion_prefix(
    edits: &BufferEdits,
    path: &CanonPath,
    tab: &led_state_tabs::Tab,
    prefix_line: usize,
    prefix_start_col: usize,
) -> String {
    let Some(eb) = edits.buffers.get(path) else {
        return String::new();
    };
    if prefix_line >= eb.rope.len_lines() {
        return String::new();
    }
    let line_start = eb.rope.line_to_char(prefix_line);
    let from = line_start + prefix_start_col;
    let to = line_start + tab.cursor.col;
    if to < from || to > eb.rope.len_chars() {
        return String::new();
    }
    eb.rope.slice(from..to).to_string()
}

/// Bundle of references `LspGotoApply::apply` needs. Carved out
/// of the runtime tick / test sites so the apply method can take
/// a small `&mut self` instead of an 8-positional-arg list.
struct LspGotoApply<'a> {
    tabs: &'a mut Tabs,
    edits: &'a BufferEdits,
    jumps: &'a mut JumpListState,
    alerts: &'a mut AlertState,
    lsp_pending: &'a mut led_state_lsp::LspPending,
    terminal: &'a led_driver_terminal_core::Terminal,
    browser: &'a led_state_browser::BrowserUi,
    path_chains: &'a mut std::collections::HashMap<CanonPath, PathChain>,
}

/// Apply a goto-definition response: record a jump, switch to
/// the target tab (when open), move the cursor. Dropped
/// silently if the seq doesn't match the latest outstanding
/// request (user navigated elsewhere). `None` location surfaces
/// a warn alert so the user knows why the keystroke went
/// nowhere.
///
/// Opening a fresh buffer when the target is outside the
/// currently-open tabs is deferred to M21 (session / persistence
/// will stash a pending cursor the same way find-file does);
/// for M18 the jump silent-no-ops when the path isn't open.
impl<'a> LspGotoApply<'a> {
    fn apply(&mut self, seq: u64, location: Option<led_driver_lsp_core::Location>) {
        let tabs = &mut *self.tabs;
        let edits = self.edits;
        let jumps = &mut *self.jumps;
        let alerts = &mut *self.alerts;
        let lsp_pending = &mut *self.lsp_pending;
        let terminal = self.terminal;
        let browser = self.browser;
        let path_chains = &mut *self.path_chains;

        if lsp_pending.latest_goto_seq != Some(seq) {
            return;
        }
        lsp_pending.latest_goto_seq = None;
        let Some(loc) = location else {
            alerts.set_warn(
                "lsp.goto".to_string(),
                "No definition found".to_string(),
            );
            return;
        };
        // Capture the pre-jump position before applying the
        // target, so Alt-b returns to where the user called the
        // command from.
        let Some(current) = current_jump_position(tabs) else {
            return;
        };
        jumps.record(current);

        // Two paths now (M21):
        //   * Buffer is already loaded → land cursor + recenter
        //     scroll inline, exactly like before.
        //   * Buffer not yet loaded → open / focus a tab at the
        //     target path and stash the cursor as `pending_cursor`.
        //     The load-completion ingest applies it once the rope
        //     materialises.
        if let Some(idx) = tabs.open.iter().position(|t| t.path == loc.path)
            && let Some(eb) = edits.buffers.get(&loc.path)
        {
            let line_count = eb.rope.len_lines();
            let line = (loc.line as usize).min(line_count.saturating_sub(1));
            let line_start = eb.rope.line_to_char(line);
            let line_end = if line + 1 < line_count {
                eb.rope.line_to_char(line + 1)
            } else {
                eb.rope.len_chars()
            };
            let line_len = line_end.saturating_sub(line_start);
            let col = (loc.col as usize).min(line_len);
            let body_rows = terminal
                .dims
                .map(|d| {
                    led_driver_terminal_core::Layout::compute(d, browser.visible)
                        .editor_area
                        .rows as usize
                })
                .unwrap_or(0);
            let content_cols = dispatch::editor_content_cols(terminal, browser);
            let tab = &mut tabs.open[idx];
            tab.cursor.line = line;
            tab.cursor.col = col;
            tab.cursor.preferred_col = col;
            tab.scroll = dispatch::center_on_cursor(
                tab.scroll,
                tab.cursor,
                body_rows,
                &eb.rope,
                content_cols,
            );
            tabs.active = Some(tab.id);
            alerts.clear_warn("lsp.goto");
            return;
        }

        // Open a fresh tab at the target path with a pending
        // cursor; the load-completion hook applies it. Stash the
        // path-chain so the language detector picks up the
        // user-typed extension on load.
        let chain = led_core::UserPath::new(loc.path.as_path()).resolve_chain();
        path_chains.insert(loc.path.clone(), chain);
        dispatch::open_or_focus_tab(tabs, &loc.path, true);
        if let Some(tab) = tabs
            .open
            .iter_mut()
            .find(|t| t.path == loc.path)
        {
            tab.pending_cursor = Some(led_state_tabs::Cursor {
                line: loc.line as usize,
                col: loc.col as usize,
                preferred_col: loc.col as usize,
            });
            // Don't pre-set a scroll — let the load-completion
            // hook clear pending_scroll = None and the active tab
            // tick recenter via the scroll-adjust pass on the next
            // cursor move (or via a future "if pending_cursor and
            // pending_scroll is None, recenter on apply" path).
        }
        alerts.clear_warn("lsp.goto");
    }
}

fn current_jump_position(tabs: &Tabs) -> Option<led_state_jumps::JumpPosition> {
    let id = tabs.active?;
    let tab = tabs.open.iter().find(|t| t.id == id)?;
    Some(led_state_jumps::JumpPosition {
        path: tab.path.clone(),
        line: tab.cursor.line,
        col: tab.cursor.col,
    })
}

/// Bundle of references `LspEditApply::apply` needs. Carved out
/// of the runtime tick / test sites so the apply method can take
/// a small `&mut self` instead of a 7-positional-arg list.
struct LspEditApply<'a> {
    edits: &'a mut BufferEdits,
    tabs: &'a led_state_tabs::Tabs,
    alerts: &'a mut AlertState,
    lsp_pending: &'a mut led_state_lsp::LspPending,
}

/// Apply an `LspEvent::Edits` delivery: walk `file_edits`, apply
/// each `TextEditOp` to its target buffer (when currently open),
/// and record history entries so Undo can revert. Edits for
/// paths we don't have open are dropped silently — M18 parity
/// with legacy, which writes disk-only edits from the manager
/// side rather than through the buffer layer.
///
/// Stale seq (rename only, for now) drops the whole delivery.
/// Edits arrive ordered by the server; we reapply per-file from
/// latest range to earliest so later applies don't shift
/// earlier ones. Alerts surface "Renamed N occurrence(s) in M
/// file(s)" on success.
impl<'a> LspEditApply<'a> {
    fn apply(
        &mut self,
        seq: u64,
        origin: led_driver_lsp_core::EditsOrigin,
        file_edits: &std::sync::Arc<Vec<led_driver_lsp_core::FileEdit>>,
    ) {
        let edits = &mut *self.edits;
        let tabs = self.tabs;
        let alerts = &mut *self.alerts;
        let lsp_pending = &mut *self.lsp_pending;
    // Stale-seq gate per origin.
    match origin {
        led_driver_lsp_core::EditsOrigin::Rename => {
            if lsp_pending.latest_rename_seq != Some(seq) {
                return;
            }
            lsp_pending.latest_rename_seq = None;
        }
        led_driver_lsp_core::EditsOrigin::CodeAction => {
            if lsp_pending.latest_code_action_select_seq != Some(seq) {
                return;
            }
            lsp_pending.latest_code_action_select_seq = None;
        }
        led_driver_lsp_core::EditsOrigin::Format => {
            // Per-path stale gate: the most-recently-queued
            // format for each path is the only reply whose
            // edits the runtime accepts. Older replies (e.g.
            // from a pre-reformat keystroke's follow-up)
            // drop silently.
            let mut keep = false;
            for fe in file_edits.iter() {
                if lsp_pending.latest_format_seq.get(&fe.path) == Some(&seq) {
                    lsp_pending.latest_format_seq.remove(&fe.path);
                    keep = true;
                }
            }
            if !keep && file_edits.is_empty() {
                // Empty-edit formats still need to release the
                // save gate. Walk every `pending_save_after_format`
                // path and if ANY has its latest_format_seq
                // matching, accept this delivery as that path's
                // completion.
                let matching: Vec<CanonPath> = lsp_pending
                    .pending_save_after_format
                    .iter()
                    .filter(|p| lsp_pending.latest_format_seq.get(*p) == Some(&seq))
                    .cloned()
                    .collect();
                for p in &matching {
                    lsp_pending.latest_format_seq.remove(p);
                }
                if matching.is_empty() {
                    return;
                }
                // Post-format save trigger below still handles
                // matching.
            } else if !keep {
                return;
            }
        }
    }

    let mut total_ops = 0usize;
    let mut files_touched = 0usize;
    for fe in file_edits.iter() {
        // Capture the tab's cursor for this file (if any) before
        // the edit runs, so the group's undo/redo bookends point
        // at a meaningful location rather than (0, 0). When no
        // tab is open for the path (shouldn't happen in
        // practice — we only get edits for paths we asked about)
        // we fall back to Default.
        let cursor = tabs
            .open
            .iter()
            .find(|t| t.path == fe.path)
            .map(|t| t.cursor)
            .unwrap_or_default();
        let Some(eb) = edits.buffers.get_mut(&fe.path) else {
            continue;
        };
        if fe.edits.is_empty() {
            continue;
        }
        let applied = apply_file_edits(eb, &fe.edits, cursor);
        if applied > 0 {
            total_ops += applied;
            files_touched += 1;
        }
    }

    if total_ops > 0
        && !matches!(origin, led_driver_lsp_core::EditsOrigin::Format)
    {
        let msg = match origin {
            led_driver_lsp_core::EditsOrigin::Rename => {
                if files_touched == 1 {
                    format!(
                        "Renamed {total_ops} occurrence{} in 1 file",
                        if total_ops == 1 { "" } else { "s" },
                    )
                } else {
                    format!(
                        "Renamed {total_ops} occurrences in {files_touched} files"
                    )
                }
            }
            led_driver_lsp_core::EditsOrigin::CodeAction => {
                format!("Applied code action ({total_ops} edit{})",
                    if total_ops == 1 { "" } else { "s" })
            }
            led_driver_lsp_core::EditsOrigin::Format => unreachable!(),
        };
        alerts.set_info(msg, std::time::Instant::now(), INFO_TTL);
    }

    // Post-format save trigger: paths awaiting save after
    // format now slot into `pending_saves`. Covers the
    // format-arrived-empty case (no file_edits, nothing
    // touched) as well as the format-with-edits case (edits
    // applied above, now save).
    if matches!(origin, led_driver_lsp_core::EditsOrigin::Format) {
        // Collect paths associated with this format delivery:
        // either referenced in `file_edits`, or in
        // `pending_save_after_format` (fallback for empty
        // deliveries where `file_edits` is empty).
        let mut to_save: Vec<CanonPath> = file_edits
            .iter()
            .map(|fe| fe.path.clone())
            .collect();
        if to_save.is_empty() {
            to_save = lsp_pending
                .pending_save_after_format
                .iter()
                .cloned()
                .collect();
        }
        for path in to_save {
            if lsp_pending.pending_save_after_format.remove(&path).is_none() {
                continue;
            }
            // Always save, even if the buffer looks clean: the
            // user asked for Save, the format round-trip is
            // complete, and writing a byte-identical file is
            // cheap. Gating on `eb.dirty()` here would drop the
            // save whenever format returned no edits on a clean
            // buffer, contradicting "save should always save".
            if edits.buffers.contains_key(&path) {
                edits.pending_saves.insert(path);
            }
        }
    }
    }
}

/// Apply a batch of per-file `TextEditOp`s to a single buffer
/// and record them as a **single** undo group so one Ctrl-/
/// reverses the whole batch atomically.
///
/// Per-op groups (the previous approach) break whenever the
/// server returns overlapping-by-effect edits — e.g. sort-imports
/// is `(delete "foo, " at X, insert "foo, " at Y)`. Undoing them
/// one at a time leaves a duplicate-text intermediate state, and
/// the second undo then uses stale positions. Coalescing into
/// one group keeps the intermediate state unobservable and keeps
/// every op's recorded `at` valid relative to the rope at the
/// moment of inversion.
///
/// Edits apply bottom-first (descending start position) so each
/// apply's char indices stay valid for the next one. `cursor`
/// is the active-tab cursor captured pre-apply; it doubles as
/// `cursor_before` and `cursor_after` so undo/redo don't
/// teleport the user to (0, 0). Returns the number of ops
/// actually applied (skips any whose range is out of bounds).
fn apply_file_edits(
    eb: &mut EditedBuffer,
    ops: &[led_driver_lsp_core::TextEditOp],
    cursor: led_state_tabs::Cursor,
) -> usize {
    // Sort descending by (start_line, start_col) so later edits
    // don't invalidate earlier ones' indices.
    let mut sorted: Vec<&led_driver_lsp_core::TextEditOp> = ops.iter().collect();
    sorted.sort_by(|a, b| {
        (b.start_line, b.start_col)
            .cmp(&(a.start_line, a.start_col))
    });
    let mut replaces: Vec<(
        usize,
        std::sync::Arc<str>,
        std::sync::Arc<str>,
    )> = Vec::with_capacity(sorted.len());
    for op in sorted {
        if let Some((at, removed, inserted)) = apply_one_text_edit(eb, op) {
            replaces.push((at, removed, inserted));
        }
    }
    let applied = replaces.len();
    if applied > 0 {
        eb.history
            .record_replace_batch(replaces, cursor, cursor);
    }
    applied
}

/// Apply a single `TextEditOp` to the rope + bump version, and
/// return the `(at, removed, inserted)` triple the caller needs
/// to record in history. Returns `None` when the op's range is
/// out of bounds; the caller skips those.
fn apply_one_text_edit(
    eb: &mut EditedBuffer,
    op: &led_driver_lsp_core::TextEditOp,
) -> Option<(usize, std::sync::Arc<str>, std::sync::Arc<str>)> {
    let rope = &eb.rope;
    let line_count = rope.len_lines();
    if (op.start_line as usize) >= line_count {
        return None;
    }
    let start_line = op.start_line as usize;
    let end_line = (op.end_line as usize).min(line_count.saturating_sub(1));
    let start_line_char = rope.line_to_char(start_line);
    let end_line_char = rope.line_to_char(end_line);
    let start_line_len = if start_line + 1 < line_count {
        rope.line_to_char(start_line + 1) - start_line_char
    } else {
        rope.len_chars() - start_line_char
    };
    let end_line_len = if end_line + 1 < line_count {
        rope.line_to_char(end_line + 1) - end_line_char
    } else {
        rope.len_chars() - end_line_char
    };
    let start_char = start_line_char + (op.start_col as usize).min(start_line_len);
    let end_char = end_line_char + (op.end_col as usize).min(end_line_len);
    if end_char < start_char {
        return None;
    }

    let mut new_rope = (*eb.rope).clone();
    let removed: String = new_rope.slice(start_char..end_char).to_string();
    new_rope.remove(start_char..end_char);
    new_rope.insert(start_char, &op.new_text);

    eb.rope = std::sync::Arc::new(new_rope);
    eb.version = eb.version.saturating_add(1);
    Some((
        start_char,
        std::sync::Arc::<str>::from(removed),
        std::sync::Arc::<str>::from(op.new_text.as_ref()),
    ))
}

fn seed_edit_from_load(
    edits: &mut BufferEdits,
    path: led_core::CanonPath,
    rope: Arc<ropey::Rope>,
) -> bool {
    use imbl::hashmap::Entry;
    let seq_gen = edits.seq_gen.clone();
    match edits.buffers.entry(path) {
        Entry::Vacant(v) => {
            v.insert(EditedBuffer::fresh_with_seq_gen(rope, seq_gen));
            true
        }
        Entry::Occupied(_) => false,
    }
}

/// When a fresh find-file listing arrives AND the overlay is in
/// arrow-follow mode (user engaged arrow-nav, then descended via
/// Enter) AND nothing is currently selected, auto-select entry 0.
///
/// Mirrors what `move_selection` would do: rewrites `input` to
/// `dir_prefix(base_input) + entry.name`, keeps `show_side` up, and
/// creates a preview tab for file entries (capturing `tabs.active`
/// into `previous_tab` on the first preview). This lets the user
/// drill through directories by repeatedly pressing Enter without
/// needing to Down again after every listing arrives.
fn auto_advance_arrow_follow(
    ff: &mut led_state_find_file::FindFileState,
    tabs: &mut led_state_tabs::Tabs,
) {
    if !ff.arrow_follow || ff.completions.is_empty() || ff.selected.is_some() {
        return;
    }
    ff.selected = Some(0);
    ff.show_side = true;
    let base = led_state_find_file::dir_prefix(&ff.base_input).to_string();
    let entry = &ff.completions[0];
    let mut new_input = base;
    new_input.push_str(&entry.name);
    ff.input.set(new_input);
    if !entry.is_dir {
        if ff.previous_tab.is_none() {
            ff.previous_tab = tabs.active;
        }
        let path = entry.full.clone();
        dispatch::open_or_focus_tab(tabs, &path, /* promote= */ false);
    }
}

/// Convenience constructor: spawns both drivers with a shared trace
/// using the desktop `*-native` implementations. Every driver gets a
/// clone of the wake [`Notifier`]; each completion signals the main
/// loop so it wakes immediately.
pub fn spawn_drivers(
    trace: SharedTrace,
    wake: &Wake,
    lsp_server_override: Option<String>,
    clipboard_isolated: bool,
) -> io::Result<Drivers> {
    let (file, file_native) =
        led_driver_buffers_native::spawn(trace.clone().as_file_trace(), wake.notifier.clone());
    let (file_write, file_write_native) = led_driver_buffers_native::spawn_write(
        trace.clone().as_file_trace(),
        wake.notifier.clone(),
    );
    let clip_trace = trace.clone().as_clipboard_trace();
    let (clipboard, clipboard_native) = if clipboard_isolated {
        led_driver_clipboard_native::spawn_isolated(clip_trace, wake.notifier.clone())
    } else {
        led_driver_clipboard_native::spawn(clip_trace, wake.notifier.clone())
    };
    let (fs_list, fs_list_native) = led_driver_fs_list_native::spawn(
        trace.clone().as_fs_list_trace(),
        wake.notifier.clone(),
    );
    let (find_file, find_file_native) = led_driver_find_file_native::spawn(
        trace.clone().as_find_file_trace(),
        wake.notifier.clone(),
    );
    let (file_search, file_search_native) = led_driver_file_search_native::spawn(
        trace.clone().as_file_search_trace(),
        wake.notifier.clone(),
    );
    let (syntax, syntax_native) = led_driver_syntax_native::spawn(
        trace.clone().as_syntax_trace(),
        wake.notifier.clone(),
    );
    let (lsp, lsp_native) = led_driver_lsp_native::spawn(
        trace.clone().as_lsp_trace(),
        wake.notifier.clone(),
        lsp_server_override,
    );
    let (git, git_native) = led_driver_git_native::spawn(
        trace.clone().as_git_trace(),
        wake.notifier.clone(),
    );
    let (session, session_native) = led_driver_session_native::spawn(
        trace.clone().as_session_trace(),
        wake.notifier.clone(),
    );
    let (input, input_native) = led_driver_terminal_native::spawn(
        trace.clone().as_terminal_trace(),
        wake.notifier.clone(),
    )?;
    let output = TerminalOutputDriver::new(trace.as_terminal_trace());
    Ok(Drivers {
        file,
        file_write,
        input,
        output,
        clipboard,
        fs_list,
        find_file,
        file_search,
        syntax,
        lsp,
        git,
        session,
        _file_native: file_native,
        _file_write_native: file_write_native,
        _input_native: input_native,
        _clipboard_native: clipboard_native,
        _fs_list_native: fs_list_native,
        _find_file_native: find_file_native,
        _file_search_native: file_search_native,
        _syntax_native: syntax_native,
        _lsp_native: lsp_native,
        _git_native: git_native,
        _session_native: session_native,
    })
}

// ── Trace adapter plumbing ─────────────────────────────────────────────
//
// Each driver's `*-core` crate defines its own narrow `Trace` trait so
// `*-core` has no dependency on the runtime. The runtime owns the
// unified `Trace` trait + `SharedTrace` and provides adapters that
// bridge between them.

pub(crate) mod trace_adapter {
    use std::sync::Arc;

    use led_core::CanonPath;
    use led_driver_terminal_core::{Dims, KeyEvent};
    use ropey::Rope;

    use crate::trace::Trace;

    pub(crate) struct FileTraceAdapter(pub Arc<dyn Trace>);
    pub(crate) struct TermTraceAdapter(pub Arc<dyn Trace>);
    pub(crate) struct ClipboardTraceAdapter(pub Arc<dyn Trace>);
    pub(crate) struct FsListTraceAdapter(pub Arc<dyn Trace>);
    pub(crate) struct FindFileTraceAdapter(pub Arc<dyn Trace>);
    pub(crate) struct FileSearchTraceAdapter(pub Arc<dyn Trace>);
    pub(crate) struct SyntaxTraceAdapter(pub Arc<dyn Trace>);
    pub(crate) struct LspTraceAdapter(pub Arc<dyn Trace>);
    pub(crate) struct GitTraceAdapter(pub Arc<dyn Trace>);
    pub(crate) struct SessionTraceAdapter(pub Arc<dyn Trace>);

    impl led_driver_buffers_core::Trace for FileTraceAdapter {
        fn file_load_start(&self, path: &CanonPath) {
            self.0.file_load_start(path);
        }
        fn file_load_done(&self, path: &CanonPath, result: &Result<Arc<Rope>, String>) {
            self.0.file_load_done(path, result);
        }
        fn file_save_start(&self, path: &CanonPath, version: u64) {
            self.0.file_save_start(path, version);
        }
        fn file_save_done(&self, path: &CanonPath, version: u64, result: &Result<(), String>) {
            self.0.file_save_done(path, version, result);
        }
        fn file_save_as_start(&self, from: &CanonPath, to: &CanonPath) {
            self.0.file_save_as_start(from, to);
        }
        fn file_save_as_done(&self, from: &CanonPath, to: &CanonPath, result: &Result<(), String>) {
            self.0.file_save_as_done(from, to, result);
        }
    }

    impl led_driver_terminal_core::Trace for TermTraceAdapter {
        fn key_in(&self, ev: &KeyEvent) {
            self.0.key_in(ev);
        }
        fn resize(&self, dims: Dims) {
            self.0.resize(dims);
        }
        fn render_tick(&self) {
            self.0.render_tick();
        }
    }

    impl led_driver_clipboard_core::Trace for ClipboardTraceAdapter {
        fn clipboard_read_start(&self) {
            self.0.clipboard_read_start();
        }
        fn clipboard_read_done(&self, ok: bool, empty: bool) {
            self.0.clipboard_read_done(ok, empty);
        }
        fn clipboard_write_start(&self, text: &str) {
            self.0.clipboard_write_start(text);
        }
        fn clipboard_write_done(&self, ok: bool) {
            self.0.clipboard_write_done(ok);
        }
    }

    impl led_driver_fs_list_core::Trace for FsListTraceAdapter {
        fn list_start(&self, path: &CanonPath) {
            self.0.fs_list_start(path);
        }
        fn list_done(
            &self,
            path: &CanonPath,
            result: &Result<Vec<led_driver_fs_list_core::DirEntry>, String>,
        ) {
            self.0.fs_list_done(path, result.is_ok());
        }
    }

    impl led_driver_find_file_core::Trace for FindFileTraceAdapter {
        fn find_file_start(&self, cmd: &led_driver_find_file_core::FindFileCmd) {
            self.0.find_file_start(cmd);
        }
        fn find_file_done(&self, path: &CanonPath, prefix: &str, ok: bool) {
            self.0.find_file_done(path, prefix, ok);
        }
    }

    impl led_driver_syntax_core::Trace for SyntaxTraceAdapter {
        fn syntax_parse_start(
            &self,
            path: &CanonPath,
            version: u64,
            language: led_state_syntax::Language,
        ) {
            self.0.syntax_parse_start(path, version, language);
        }
        fn syntax_parse_done(&self, path: &CanonPath, version: u64, ok: bool) {
            self.0.syntax_parse_done(path, version, ok);
        }
    }

    impl led_driver_lsp_core::Trace for LspTraceAdapter {
        fn lsp_server_started(&self, server: &str) {
            self.0.lsp_server_started(server);
        }
        fn lsp_request_diagnostics(&self) {
            self.0.lsp_request_diagnostics();
        }
        fn lsp_diagnostics_done(
            &self,
            path: &CanonPath,
            n: usize,
            hash: led_core::PersistedContentHash,
        ) {
            self.0.lsp_diagnostics_done(path, n, hash);
        }
        fn lsp_mode_fallback(&self) {
            self.0.lsp_mode_fallback();
        }
        fn lsp_send_request(
            &self,
            server: &str,
            method: &str,
            id: i64,
            path_uri: Option<&str>,
        ) {
            self.0.lsp_send_request(server, method, id, path_uri);
        }
        fn lsp_send_notification(
            &self,
            server: &str,
            method: &str,
            path_uri: Option<&str>,
            version: Option<i32>,
        ) {
            self.0
                .lsp_send_notification(server, method, path_uri, version);
        }
        fn lsp_recv_response(&self, server: &str, id: i64) {
            self.0.lsp_recv_response(server, id);
        }
        fn lsp_recv_notification(&self, server: &str, method: &str) {
            self.0.lsp_recv_notification(server, method);
        }
    }

    impl led_driver_git_core::Trace for GitTraceAdapter {
        fn git_scan_start(&self, root: &CanonPath) {
            self.0.git_scan_start(root);
        }
        fn git_scan_done(&self, _ok: bool, _n_files: usize) {
            // Not surfaced in dispatched.snap — the intent log only
            // tracks the dispatch side. Keep the hook so future
            // debug traces can light it up.
        }
    }

    impl led_driver_session_core::Trace for SessionTraceAdapter {
        fn session_init_start(&self, root: &CanonPath) {
            self.0.session_init_start(root);
        }
        fn session_save_start(&self) {
            self.0.session_save_start();
        }
        fn session_save_done(&self, _ok: bool) {
            // Successful save lands as the SessionEvent::Saved
            // ingest in the runtime; no separate trace line.
        }
        fn session_drop_undo(&self, path: &CanonPath) {
            self.0.workspace_clear_undo(path);
        }
        fn session_flush_undo(&self, path: &CanonPath, chain_id: &str) {
            self.0.workspace_flush_undo(path, chain_id);
        }
    }

    impl led_driver_file_search_core::Trace for FileSearchTraceAdapter {
        fn file_search_start(&self, cmd: &led_driver_file_search_core::FileSearchCmd) {
            self.0.file_search_start(
                &cmd.query,
                &cmd.root,
                cmd.case_sensitive,
                cmd.use_regex,
            );
        }
        fn file_search_done(&self, _query: &str, _ok: bool) {}
        fn file_search_replace_start(
            &self,
            cmd: &led_driver_file_search_core::FileSearchReplaceCmd,
        ) {
            self.0.file_search_replace_start(
                &cmd.query,
                &cmd.replacement,
                &cmd.root,
                cmd.case_sensitive,
                cmd.use_regex,
            );
        }
        fn file_search_replace_done(
            &self,
            _query: &str,
            _files_changed: usize,
            _total_replacements: usize,
        ) {
        }
        fn file_search_single_replace_start(
            &self,
            cmd: &led_driver_file_search_core::FileSearchSingleReplaceCmd,
        ) {
            self.0
                .file_search_single_replace_start(&cmd.path, cmd.line);
        }
        fn file_search_single_replace_done(&self, _: &CanonPath, _: bool) {}
    }
}

impl SharedTrace {
    pub(crate) fn as_file_trace(&self) -> Arc<dyn led_driver_buffers_core::Trace> {
        Arc::new(trace_adapter::FileTraceAdapter(self.inner()))
    }
    pub(crate) fn as_terminal_trace(&self) -> Arc<dyn led_driver_terminal_core::Trace> {
        Arc::new(trace_adapter::TermTraceAdapter(self.inner()))
    }
    pub(crate) fn as_clipboard_trace(&self) -> Arc<dyn led_driver_clipboard_core::Trace> {
        Arc::new(trace_adapter::ClipboardTraceAdapter(self.inner()))
    }
    pub(crate) fn as_fs_list_trace(&self) -> Arc<dyn led_driver_fs_list_core::Trace> {
        Arc::new(trace_adapter::FsListTraceAdapter(self.inner()))
    }
    pub(crate) fn as_find_file_trace(&self) -> Arc<dyn led_driver_find_file_core::Trace> {
        Arc::new(trace_adapter::FindFileTraceAdapter(self.inner()))
    }
    pub(crate) fn as_file_search_trace(
        &self,
    ) -> Arc<dyn led_driver_file_search_core::Trace> {
        Arc::new(trace_adapter::FileSearchTraceAdapter(self.inner()))
    }
    pub(crate) fn as_syntax_trace(&self) -> Arc<dyn led_driver_syntax_core::Trace> {
        Arc::new(trace_adapter::SyntaxTraceAdapter(self.inner()))
    }
    pub(crate) fn as_lsp_trace(&self) -> Arc<dyn led_driver_lsp_core::Trace> {
        Arc::new(trace_adapter::LspTraceAdapter(self.inner()))
    }
    pub(crate) fn as_git_trace(&self) -> Arc<dyn led_driver_git_core::Trace> {
        Arc::new(trace_adapter::GitTraceAdapter(self.inner()))
    }
    pub(crate) fn as_session_trace(&self) -> Arc<dyn led_driver_session_core::Trace> {
        Arc::new(trace_adapter::SessionTraceAdapter(self.inner()))
    }
}

#[cfg(test)]
mod tests {
    //! Ingest-level invariants (course-correct #6).

    use super::*;
    use led_core::UserPath;
    use ropey::Rope;

    fn canon(s: &str) -> led_core::CanonPath {
        UserPath::new(s).canonicalize()
    }

    #[test]
    fn seed_edit_from_load_inserts_when_absent() {
        let mut edits = BufferEdits::default();
        let path = canon("a.rs");
        let rope = Arc::new(Rope::from_str("disk\n"));
        let inserted = seed_edit_from_load(&mut edits, path.clone(), rope);
        assert!(inserted);
        assert_eq!(edits.buffers[&path].rope.to_string(), "disk\n");
    }

    #[test]
    fn seed_edit_from_load_discards_late_completion_on_edited_buffer() {
        // Simulates the race: the user opened a file, edited it,
        // and a *second* read completion for the same path arrives.
        // The edit view must win — we must NOT clobber it with the
        // stale disk rope.
        let mut edits = BufferEdits::default();
        let path = canon("a.rs");
        let edited = Arc::new(Rope::from_str("edited\n"));
        edits
            .buffers
            .insert(path.clone(), EditedBuffer::fresh(edited.clone()));
        // Mutate so the entry is visibly "the user's view".
        edits
            .buffers
            .get_mut(&path)
            .unwrap()
            .rope = Arc::new(Rope::from_str("user typed more"));

        let stale_disk = Arc::new(Rope::from_str("old disk\n"));
        let inserted = seed_edit_from_load(&mut edits, path.clone(), stale_disk);
        assert!(!inserted);
        // User's rope preserved.
        assert_eq!(edits.buffers[&path].rope.to_string(), "user typed more");
    }

    // ── arrow-follow auto-advance ─────────────────────────────────

    fn entry(name: &str, is_dir: bool) -> led_state_find_file::FindFileEntry {
        use led_driver_find_file_core::FindFileEntry;
        let display = if is_dir { format!("{name}/") } else { name.to_string() };
        FindFileEntry {
            name: display,
            full: canon(&format!("/x/{name}")),
            is_dir,
        }
    }

    #[test]
    fn auto_advance_selects_first_when_arrow_follow_engaged() {
        use led_state_find_file::FindFileState;
        let mut ff = FindFileState::open("/x/".into());
        ff.base_input = "/x/".into();
        ff.arrow_follow = true;
        ff.completions = vec![entry("a", true), entry("b", true)];
        let mut tabs = led_state_tabs::Tabs::default();
        auto_advance_arrow_follow(&mut ff, &mut tabs);
        assert_eq!(ff.selected, Some(0));
        assert_eq!(ff.input.text, "/x/a/");
        assert!(ff.show_side);
    }

    #[test]
    fn auto_advance_does_nothing_when_arrow_follow_off() {
        use led_state_find_file::FindFileState;
        let mut ff = FindFileState::open("/x/".into());
        ff.base_input = "/x/".into();
        ff.arrow_follow = false;
        ff.completions = vec![entry("a", true)];
        let mut tabs = led_state_tabs::Tabs::default();
        auto_advance_arrow_follow(&mut ff, &mut tabs);
        assert!(ff.selected.is_none());
        assert_eq!(ff.input.text, "/x/");
    }

    #[test]
    fn auto_advance_creates_preview_tab_for_file_entry() {
        use led_state_find_file::FindFileState;
        let mut ff = FindFileState::open("/x/".into());
        ff.base_input = "/x/".into();
        ff.arrow_follow = true;
        ff.completions = vec![entry("main.rs", false)];
        let mut tabs = led_state_tabs::Tabs::default();
        auto_advance_arrow_follow(&mut ff, &mut tabs);
        assert_eq!(tabs.open.len(), 1);
        assert!(tabs.open[0].preview);
        assert_eq!(ff.input.text, "/x/main.rs");
    }

    #[test]
    fn auto_advance_respects_existing_selection() {
        // If user is mid-arrow (selected already Some) the auto-
        // advance shouldn't clobber their pick.
        use led_state_find_file::FindFileState;
        let mut ff = FindFileState::open("/x/".into());
        ff.base_input = "/x/".into();
        ff.arrow_follow = true;
        ff.selected = Some(1);
        ff.completions = vec![entry("a", true), entry("b", true)];
        let mut tabs = led_state_tabs::Tabs::default();
        auto_advance_arrow_follow(&mut ff, &mut tabs);
        assert_eq!(ff.selected, Some(1));
    }

    // ── Syntax wiring ─────────────────────────────────────────────

    /// Stage-3/4 wiring: the main loop spawns a real native worker,
    /// seeds a Rust buffer, ticks the dispatch side, and waits for a
    /// `SyntaxOut` to land + populate `Atoms.syntax`. Verifies the
    /// three pieces composed properly:
    /// (1) language detection on seed,
    /// (2) cmd dispatch when buffer.version > state.version,
    /// (3) ingest updates state.tokens with usable spans.
    #[test]
    fn syntax_pipeline_populates_tokens_for_loaded_rust_buffer() {
        use std::time::{Duration, Instant};
        let path = canon("pipeline.rs");
        let rope = Arc::new(Rope::from_str("fn main() {}\n"));

        // Kick the seed through the runtime helper so `edits.seq_gen`
        // threads into the history, matching main-loop wiring.
        let mut edits = BufferEdits::default();
        seed_edit_from_load(&mut edits, path.clone(), rope.clone());

        // Language detection + SyntaxState insert, mirroring the
        // main loop's post-seed block.
        let mut syntax = SyntaxStates::default();
        let lang = Language::from_path(&path).expect("rust extension recognised");
        syntax
            .by_path
            .insert(path.clone(), SyntaxState::new(lang));

        // Spawn the real worker + issue a parse cmd.
        let (drv, _native) = led_driver_syntax_native::spawn(
            Arc::new(led_driver_syntax_core::NoopTrace),
            Notifier::noop(),
        );
        let eb = edits.buffers.get(&path).unwrap();
        let cmd = led_driver_syntax_core::SyntaxCmd {
            path: path.clone(),
            version: eb.version,
            rope: eb.rope.clone(),
            language: lang,
            prev_tree: None,
            prev_rope: None,
        };
        drv.execute(std::iter::once(&cmd));
        {
            let state = syntax.by_path.get_mut(&path).unwrap();
            state.in_flight_version = Some(eb.version);
        }

        // Wait (up to 5s) for a completion.
        let start = Instant::now();
        let mut applied = false;
        while start.elapsed() < Duration::from_secs(5) && !applied {
            for done in drv.process() {
                let state = syntax.by_path.get_mut(&done.path).unwrap();
                state.in_flight_version = None;
                state.tree = Some(done.tree);
                state.tokens = done.tokens;
                state.version = done.version;
                applied = true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(applied, "expected SyntaxOut within 5s");
        let state = &syntax.by_path[&path];
        assert!(
            !state.tokens.is_empty(),
            "expected tokens for `fn main() {{}}`"
        );
        let kinds: std::collections::HashSet<_> =
            state.tokens.iter().map(|t| t.kind).collect();
        assert!(
            kinds.contains(&led_state_syntax::TokenKind::Keyword),
            "expected a Keyword token; got {kinds:?}",
        );
    }

    // ── M18 goto-definition ingest ────────────────────────

    fn seed_tab(path: &str) -> led_state_tabs::Tabs {
        let mut tabs = led_state_tabs::Tabs::default();
        let id = led_state_tabs::TabId(1);
        tabs.open.push_back(led_state_tabs::Tab {
            id,
            path: canon(path),
            ..Default::default()
        });
        tabs.active = Some(id);
        tabs
    }

    #[test]
    fn apply_goto_definition_moves_cursor_and_records_jump() {
        let mut tabs = seed_tab("main.rs");
        tabs.open[0].cursor = led_state_tabs::Cursor {
            line: 5,
            col: 10,
            preferred_col: 10,
        };
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("main.rs"),
            EditedBuffer::fresh(Arc::new(Rope::from_str(
                "line0\nline1\nline2\nline3\nline4\nline5 longer\n",
            ))),
        );
        let mut jumps = led_state_jumps::JumpListState::default();
        let mut alerts = AlertState::default();
        // Caller allocates the seq via queue_*; simulate by
        // setting latest_goto_seq to 42.
        let mut lsp_pending = led_state_lsp::LspPending {
            latest_goto_seq: Some(42),
            ..Default::default()
        };
        let mut _path_chains: std::collections::HashMap<CanonPath, PathChain> =
            std::collections::HashMap::new();
        LspGotoApply {
            tabs: &mut tabs,
            edits: &edits,
            jumps: &mut jumps,
            alerts: &mut alerts,
            lsp_pending: &mut lsp_pending,
            terminal: &led_driver_terminal_core::Terminal::default(),
            browser: &led_state_browser::BrowserUi::default(),
            path_chains: &mut _path_chains,
        }
        .apply(
            42,
            Some(led_driver_lsp_core::Location {
                path: canon("main.rs"),
                line: 2,
                col: 3,
            }),
        );
        assert_eq!(tabs.open[0].cursor.line, 2);
        assert_eq!(tabs.open[0].cursor.col, 3);
        // Pre-jump recorded onto the jump list.
        assert_eq!(jumps.entries.len(), 1);
        assert_eq!(jumps.entries[0].line, 5);
        assert_eq!(jumps.entries[0].col, 10);
        // Seq consumed.
        assert!(lsp_pending.latest_goto_seq.is_none());
    }

    #[test]
    fn apply_goto_definition_drops_stale_seq() {
        let mut tabs = seed_tab("main.rs");
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("main.rs"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("abc\n"))),
        );
        let mut jumps = led_state_jumps::JumpListState::default();
        let mut alerts = AlertState::default();
        let mut lsp_pending = led_state_lsp::LspPending {
            latest_goto_seq: Some(99),
            ..Default::default()
        };
        let mut _path_chains: std::collections::HashMap<CanonPath, PathChain> =
            std::collections::HashMap::new();
        LspGotoApply {
            tabs: &mut tabs,
            edits: &edits,
            jumps: &mut jumps,
            alerts: &mut alerts,
            lsp_pending: &mut lsp_pending,
            terminal: &led_driver_terminal_core::Terminal::default(),
            browser: &led_state_browser::BrowserUi::default(),
            path_chains: &mut _path_chains,
        }
        .apply(
            /* stale */ 7,
            Some(led_driver_lsp_core::Location {
                path: canon("main.rs"),
                line: 0,
                col: 2,
            }),
        );
        assert_eq!(tabs.open[0].cursor.line, 0);
        assert_eq!(tabs.open[0].cursor.col, 0);
        assert!(jumps.entries.is_empty());
        // The in-flight seq is preserved so the correct
        // response can still land.
        assert_eq!(lsp_pending.latest_goto_seq, Some(99));
    }

    #[test]
    fn apply_goto_definition_recenters_scroll_when_target_off_screen() {
        // Target line 60 with a 12-row viewport rooted at line 0:
        // cursor is off-screen. Scroll should move so the target
        // lands ~one-third from the top (60 - 12/3 = 56).
        use led_driver_terminal_core::{Dims, Terminal};
        let path = canon("main.rs");
        let mut rope = String::new();
        for i in 0..100 {
            rope.push_str(&format!("line {i}\n"));
        }
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            path.clone(),
            EditedBuffer::fresh(Arc::new(Rope::from_str(&rope))),
        );
        let mut tabs = seed_tab("main.rs");
        tabs.open[0].cursor = led_state_tabs::Cursor {
            line: 0,
            col: 0,
            preferred_col: 0,
        };
        tabs.open[0].scroll = led_state_tabs::Scroll::default();
        let mut jumps = led_state_jumps::JumpListState::default();
        let mut alerts = AlertState::default();
        let mut lsp_pending = led_state_lsp::LspPending {
            latest_goto_seq: Some(1),
            ..Default::default()
        };
        let term = Terminal {
            dims: Some(Dims { cols: 80, rows: 14 }), // body ≈ 12 rows
            ..Default::default()
        };
        let mut _path_chains: std::collections::HashMap<CanonPath, PathChain> =
            std::collections::HashMap::new();
        LspGotoApply {
            tabs: &mut tabs,
            edits: &edits,
            jumps: &mut jumps,
            alerts: &mut alerts,
            lsp_pending: &mut lsp_pending,
            terminal: &term,
            browser: &led_state_browser::BrowserUi {
                visible: false,
                ..Default::default()
            },
            path_chains: &mut _path_chains,
        }
        .apply(
            1,
            Some(led_driver_lsp_core::Location {
                path: path.clone(),
                line: 60,
                col: 0,
            }),
        );
        assert_eq!(tabs.open[0].cursor.line, 60);
        assert_eq!(
            tabs.open[0].scroll.top, 56,
            "scroll should position line 60 at ~body_rows/3 from top",
        );
    }

    #[test]
    fn apply_goto_definition_leaves_scroll_when_target_visible() {
        // Target line 5 with scroll.top=0 and a 20-row viewport:
        // already on screen, no scroll adjustment.
        use led_driver_terminal_core::{Dims, Terminal};
        let path = canon("main.rs");
        let mut rope = String::new();
        for i in 0..50 {
            rope.push_str(&format!("line {i}\n"));
        }
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            path.clone(),
            EditedBuffer::fresh(Arc::new(Rope::from_str(&rope))),
        );
        let mut tabs = seed_tab("main.rs");
        tabs.open[0].cursor = led_state_tabs::Cursor {
            line: 0,
            col: 0,
            preferred_col: 0,
        };
        tabs.open[0].scroll = led_state_tabs::Scroll::default();
        let mut jumps = led_state_jumps::JumpListState::default();
        let mut alerts = AlertState::default();
        let mut lsp_pending = led_state_lsp::LspPending {
            latest_goto_seq: Some(1),
            ..Default::default()
        };
        let term = Terminal {
            dims: Some(Dims { cols: 80, rows: 22 }), // body ≈ 20 rows
            ..Default::default()
        };
        let mut _path_chains: std::collections::HashMap<CanonPath, PathChain> =
            std::collections::HashMap::new();
        LspGotoApply {
            tabs: &mut tabs,
            edits: &edits,
            jumps: &mut jumps,
            alerts: &mut alerts,
            lsp_pending: &mut lsp_pending,
            terminal: &term,
            browser: &led_state_browser::BrowserUi {
                visible: false,
                ..Default::default()
            },
            path_chains: &mut _path_chains,
        }
        .apply(
            1,
            Some(led_driver_lsp_core::Location {
                path: path.clone(),
                line: 5,
                col: 0,
            }),
        );
        assert_eq!(tabs.open[0].cursor.line, 5);
        assert_eq!(
            tabs.open[0].scroll.top, 0,
            "scroll must not jerk when target is already visible",
        );
    }

    #[test]
    fn apply_goto_definition_no_match_surfaces_warn_alert() {
        let mut tabs = seed_tab("main.rs");
        let edits = BufferEdits::default();
        let mut jumps = led_state_jumps::JumpListState::default();
        let mut alerts = AlertState::default();
        let mut lsp_pending = led_state_lsp::LspPending {
            latest_goto_seq: Some(1),
            ..Default::default()
        };
        let mut _path_chains: std::collections::HashMap<CanonPath, PathChain> =
            std::collections::HashMap::new();
        LspGotoApply {
            tabs: &mut tabs,
            edits: &edits,
            jumps: &mut jumps,
            alerts: &mut alerts,
            lsp_pending: &mut lsp_pending,
            terminal: &led_driver_terminal_core::Terminal::default(),
            browser: &led_state_browser::BrowserUi::default(),
            path_chains: &mut _path_chains,
        }
        .apply(1, None);
        assert!(alerts.warns.iter().any(|(k, _)| k == "lsp.goto"));
    }

    #[test]
    fn apply_lsp_edits_rename_applies_and_bumps_version() {
        use led_driver_lsp_core::{EditsOrigin, FileEdit, TextEditOp};
        let path = canon("a.rs");
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            path.clone(),
            EditedBuffer::fresh(Arc::new(Rope::from_str("foo + foo"))),
        );
        let mut alerts = AlertState::default();
        let mut lsp_extras = led_state_lsp::LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending {
            latest_rename_seq: Some(7),
            ..Default::default()
        };
        let file_edits = std::sync::Arc::new(vec![FileEdit {
            path: path.clone(),
            edits: vec![
                TextEditOp {
                    start_line: 0,
                    start_col: 0,
                    end_line: 0,
                    end_col: 3,
                    new_text: std::sync::Arc::<str>::from("bar"),
                },
                TextEditOp {
                    start_line: 0,
                    start_col: 6,
                    end_line: 0,
                    end_col: 9,
                    new_text: std::sync::Arc::<str>::from("bar"),
                },
            ],
        }]);
        let _ = &mut lsp_extras;
        let tabs = led_state_tabs::Tabs::default();
        LspEditApply {
            edits: &mut edits,
            tabs: &tabs,
            alerts: &mut alerts,
            lsp_pending: &mut lsp_pending,
        }
        .apply(7, EditsOrigin::Rename, &file_edits);
        let eb = edits.buffers.get(&path).unwrap();
        assert_eq!(eb.rope.to_string(), "bar + bar");
        assert!(eb.version > 0);
        assert!(lsp_pending.latest_rename_seq.is_none());
        assert!(
            alerts.info.as_ref().is_some_and(|m| m.contains("Renamed"))
        );
    }

    #[test]
    fn format_on_save_history_survives_for_undo() {
        // Regression: saving a file that triggers an LSP format
        // (which records history entries for each applied edit)
        // must leave those entries in the buffer's history so the
        // user can Ctrl-/ back to pre-format content. Legacy's
        // `WorkspaceClearUndo` is a SQLite-side operation, not an
        // in-memory wipe; the rewrite used to conflate the two.
        use led_driver_lsp_core::{EditsOrigin, FileEdit, TextEditOp};
        let path = canon("a.rs");
        let mut edits = BufferEdits::default();
        let mut eb = EditedBuffer::fresh(Arc::new(Rope::from_str("x")));
        eb.version = 1;
        edits.buffers.insert(path.clone(), eb);
        let mut alerts = AlertState::default();
        let mut lsp_extras = led_state_lsp::LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        lsp_pending.pending_save_after_format.insert(path.clone());
        lsp_pending.latest_format_seq.insert(path.clone(), 1);

        let file_edits = std::sync::Arc::new(vec![FileEdit {
            path: path.clone(),
            edits: vec![TextEditOp {
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 1,
                new_text: std::sync::Arc::<str>::from("X"),
            }],
        }]);
        let _ = &mut lsp_extras;
        let tabs = led_state_tabs::Tabs::default();
        LspEditApply {
            edits: &mut edits,
            tabs: &tabs,
            alerts: &mut alerts,
            lsp_pending: &mut lsp_pending,
        }
        .apply(1, EditsOrigin::Format, &file_edits);
        // Format applied.
        assert_eq!(edits.buffers[&path].rope.to_string(), "X");
        // History MUST retain the record_replace entry so undo
        // can revert it. Before the fix this was cleared by the
        // save-action loop in run().
        let eb = &edits.buffers[&path];
        assert!(
            eb.history.past_len() > 0,
            "format edit should leave a history entry behind for Ctrl-/ to undo",
        );
    }

    #[test]
    fn format_with_overlapping_edits_undoes_atomically() {
        // Regression for the sort-imports bug: rust-analyzer's
        // sort returns two edits — one deletes a run of names
        // from the start of a list, the other inserts the same
        // run at a later position. Per-op undo groups reverse
        // one edit at a time, leaving a DUPLICATE-text
        // intermediate state and using stale positions on the
        // next pop. The batch-group approach fixes it: one
        // Ctrl-/ reverts the whole format atomically.
        use led_driver_lsp_core::{EditsOrigin, FileEdit, TextEditOp};
        use led_state_buffer_edits::EditOp;
        let path = canon("a.rs");
        let original = "AAA|BBB|CCC\n";
        let mut edits = BufferEdits::default();
        let eb = EditedBuffer::fresh(Arc::new(Rope::from_str(original)));
        edits.buffers.insert(path.clone(), eb);
        let mut alerts = AlertState::default();
        let mut lsp_extras = led_state_lsp::LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        lsp_pending.latest_format_seq.insert(path.clone(), 1);

        // Two edits that together perform "move AAA| to the end":
        //   * delete chars 0..4 ("AAA|")
        //   * insert "AAA|" at char 11 (end of line, before '\n')
        let file_edits = std::sync::Arc::new(vec![FileEdit {
            path: path.clone(),
            edits: vec![
                TextEditOp {
                    start_line: 0,
                    start_col: 0,
                    end_line: 0,
                    end_col: 4,
                    new_text: std::sync::Arc::<str>::from(""),
                },
                TextEditOp {
                    start_line: 0,
                    start_col: 11,
                    end_line: 0,
                    end_col: 11,
                    new_text: std::sync::Arc::<str>::from("AAA|"),
                },
            ],
        }]);
        let _ = &mut lsp_extras;
        let tabs = led_state_tabs::Tabs::default();
        LspEditApply {
            edits: &mut edits,
            tabs: &tabs,
            alerts: &mut alerts,
            lsp_pending: &mut lsp_pending,
        }
        .apply(1, EditsOrigin::Format, &file_edits);
        let formatted = edits.buffers[&path].rope.to_string();
        assert_eq!(formatted, "BBB|CCCAAA|\n", "sort applied correctly");

        // ONE undo group for the whole batch.
        assert_eq!(
            edits.buffers[&path].history.past_len(),
            1,
            "format ops coalesce into a single undo group",
        );

        // Manually invert the group the way `undo_active` does —
        // reverse-iterate and invert each op. This must restore
        // the original rope exactly, with no duplicate text.
        let mut eb = edits.buffers.remove(&path).unwrap();
        let group = eb.history.take_undo().expect("one group");
        let mut rope = (*eb.rope).clone();
        for op in group.ops.iter().rev() {
            match op {
                EditOp::Insert { at, text } => {
                    let len = text.chars().count();
                    rope.remove(*at..*at + len);
                }
                EditOp::Delete { at, text } => {
                    rope.insert(*at, text);
                }
            }
        }
        assert_eq!(
            rope.to_string(),
            original,
            "undo reverses the entire format atomically",
        );
    }

    #[test]
    fn apply_lsp_edits_format_triggers_save_when_pending() {
        use led_driver_lsp_core::{EditsOrigin, FileEdit, TextEditOp};
        let path = canon("a.rs");
        let mut edits = BufferEdits::default();
        let mut eb = EditedBuffer::fresh(Arc::new(Rope::from_str("x")));
        eb.version = 1; // dirty (saved_version still 0)
        edits.buffers.insert(path.clone(), eb);
        let mut alerts = AlertState::default();
        let mut lsp_extras = led_state_lsp::LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        lsp_pending.pending_save_after_format.insert(path.clone());
        lsp_pending
            .latest_format_seq
            .insert(path.clone(), 42);
        // Non-empty format edit (cosmetic: capitalise "x" → "X").
        let file_edits = std::sync::Arc::new(vec![FileEdit {
            path: path.clone(),
            edits: vec![TextEditOp {
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 1,
                new_text: std::sync::Arc::<str>::from("X"),
            }],
        }]);
        let _ = &mut lsp_extras;
        let tabs = led_state_tabs::Tabs::default();
        LspEditApply {
            edits: &mut edits,
            tabs: &tabs,
            alerts: &mut alerts,
            lsp_pending: &mut lsp_pending,
        }
        .apply(42, EditsOrigin::Format, &file_edits);
        assert_eq!(edits.buffers[&path].rope.to_string(), "X");
        // Post-format save is queued.
        assert!(edits.pending_saves.contains(&path));
        assert!(!lsp_pending.pending_save_after_format.contains(&path));
    }

    #[test]
    fn apply_lsp_edits_format_empty_still_triggers_save() {
        use led_driver_lsp_core::{EditsOrigin};
        let path = canon("a.rs");
        let mut edits = BufferEdits::default();
        let mut eb = EditedBuffer::fresh(Arc::new(Rope::from_str("x")));
        eb.version = 1;
        edits.buffers.insert(path.clone(), eb);
        let mut alerts = AlertState::default();
        let mut lsp_extras = led_state_lsp::LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        lsp_pending.pending_save_after_format.insert(path.clone());
        lsp_pending
            .latest_format_seq
            .insert(path.clone(), 5);
        let file_edits = std::sync::Arc::new(Vec::new());
        let _ = &mut lsp_extras;
        let tabs = led_state_tabs::Tabs::default();
        LspEditApply {
            edits: &mut edits,
            tabs: &tabs,
            alerts: &mut alerts,
            lsp_pending: &mut lsp_pending,
        }
        .apply(5, EditsOrigin::Format, &file_edits);
        assert!(edits.pending_saves.contains(&path));
    }

    #[test]
    fn apply_lsp_edits_rename_drops_stale_seq() {
        use led_driver_lsp_core::{EditsOrigin, FileEdit, TextEditOp};
        let path = canon("a.rs");
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            path.clone(),
            EditedBuffer::fresh(Arc::new(Rope::from_str("foo"))),
        );
        let mut alerts = AlertState::default();
        let mut lsp_extras = led_state_lsp::LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending {
            latest_rename_seq: Some(99),
            ..Default::default()
        };
        let file_edits = std::sync::Arc::new(vec![FileEdit {
            path: path.clone(),
            edits: vec![TextEditOp {
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 3,
                new_text: std::sync::Arc::<str>::from("bar"),
            }],
        }]);
        let _ = &mut lsp_extras;
        let tabs = led_state_tabs::Tabs::default();
        LspEditApply {
            edits: &mut edits,
            tabs: &tabs,
            alerts: &mut alerts,
            lsp_pending: &mut lsp_pending,
        }
        .apply(/* stale */ 5, EditsOrigin::Rename, &file_edits);
        // Buffer unchanged, seq preserved.
        assert_eq!(edits.buffers[&path].rope.to_string(), "foo");
        assert_eq!(lsp_pending.latest_rename_seq, Some(99));
    }

    #[test]
    fn apply_goto_definition_opens_unopened_target_with_pending_cursor() {
        // M21: target path not in the open tab set used to
        // silent-no-op. Now we open a tab at the target path,
        // record the jump, and stash a pending cursor that the
        // load-completion ingest will apply once the buffer
        // lands. The tab promotes (preview = false) so the
        // user gets a real, persistent tab — matches legacy.
        let mut tabs = seed_tab("main.rs");
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("main.rs"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("abc\n"))),
        );
        let mut jumps = led_state_jumps::JumpListState::default();
        let mut alerts = AlertState::default();
        let mut lsp_pending = led_state_lsp::LspPending {
            latest_goto_seq: Some(1),
            ..Default::default()
        };
        let mut path_chains: std::collections::HashMap<CanonPath, PathChain> =
            std::collections::HashMap::new();
        let target = canon("other.rs");
        LspGotoApply {
            tabs: &mut tabs,
            edits: &edits,
            jumps: &mut jumps,
            alerts: &mut alerts,
            lsp_pending: &mut lsp_pending,
            terminal: &led_driver_terminal_core::Terminal::default(),
            browser: &led_state_browser::BrowserUi::default(),
            path_chains: &mut path_chains,
        }
        .apply(
            1,
            Some(led_driver_lsp_core::Location {
                path: target.clone(),
                line: 7,
                col: 3,
            }),
        );
        // A new tab appears and is active; jump recorded; seq
        // consumed; pending_cursor stashed for the
        // load-completion hook to apply.
        let new_tab = tabs
            .open
            .iter()
            .find(|t| t.path == target)
            .expect("opened tab for goto target");
        assert_eq!(tabs.active, Some(new_tab.id));
        assert_eq!(
            new_tab.pending_cursor,
            Some(led_state_tabs::Cursor {
                line: 7,
                col: 3,
                preferred_col: 3,
            }),
        );
        assert_eq!(jumps.entries.len(), 1);
        assert!(lsp_pending.latest_goto_seq.is_none());
    }
}
