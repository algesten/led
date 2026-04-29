//! The runtime: event enum, dispatch, query layer, trace, main loop.
//!
//! Each driver is strictly isolated — it knows only its own source + its
//! own ABI types. This crate is where they're **combined**:
//!
//! - [`query`] defines the cross-source lenses + memos that produce
//!   `LoadAction`s (for `FileReadDriver::execute`) and `Frame`s (for
//!   `paint`).
//! - [`dispatch`] mutates driver sources in response to input events.
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

pub(crate) mod apply;
pub(crate) mod phases;
pub(crate) use apply::session::config_dir_for_session;

use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use led_driver_buffers_core::{BufferStore, FileReadDriver, FileWriteDriver};
use led_driver_buffers_native::{FileReadNative, FileWriteNative};
use led_driver_clipboard_core::ClipboardDriver;
use led_driver_clipboard_native::ClipboardNative;
use led_core::{
    BufferStateSum, BufferVersion, CanonPath, ChainId, Notifier, PathChain, SavedVersion,
    UndoDbSeq, WatchSeq,
};
use led_driver_terminal_core::{Dims, Frame, KeyEvent, Terminal, TerminalInputDriver};
use led_driver_terminal_native::{TerminalInputNative, TerminalOutputDriver};
use led_driver_file_search_core::FileSearchDriver;
use led_driver_file_search_native::FileSearchNative;
use led_driver_find_file_core::FindFileDriver;
use led_driver_find_file_native::FindFileNative;
use led_driver_fs_list_core::FsListDriver;
use led_driver_fs_list_native::FsListNative;
use led_driver_syntax_core::SyntaxDriver;
use led_driver_syntax_native::SyntaxNative;
use led_driver_lsp_core::LspDriver;
use led_driver_lsp_native::LspNative;
use led_driver_git_core::GitDriver;
use led_driver_git_native::GitNative;
use led_driver_session_core::SessionDriver;
use led_driver_session_native::SessionNative;
use led_state_alerts::AlertState;
use led_state_browser::{BrowserUi, FsTree};
use led_state_buffer_edits::BufferEdits;
use led_state_clipboard::ClipboardState;
use led_state_file_search::FileSearchState;
use led_state_find_file::FindFileState;
use led_state_isearch::IsearchState;
use led_state_jumps::JumpListState;
use led_state_kbd_macro::KbdMacroState;
use led_state_kill_ring::KillRing;
use led_state_diagnostics::{DiagnosticsStates, LspStatuses};
use led_state_git::GitState;
use led_state_lifecycle::LifecycleState;
use led_state_session::SessionState;
use led_state_syntax::SyntaxStates;
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
pub(crate) const INFO_TTL: Duration = Duration::from_secs(2);

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
    /// M26 — declared *first* so its `Sender` drops *first*.
    /// Reasoning per `EXAMPLE-ARCH.md` G12 + the M26 design doc:
    /// the file-watch driver receives notify-touches that fire
    /// from `FlushUndo` / `ClearUndo` writes still in flight on
    /// the session driver. If the watcher were alive when the
    /// session driver issued its final touch, we'd self-trigger
    /// a CheckSync against a shutting-down session. Dropping the
    /// Sender first lets the worker self-exit on hangup before
    /// any other shutdown side-effects.
    pub file_watch: led_driver_file_watch_core::FileWatchDriver,
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
    _file_watch_native: led_driver_file_watch_native::FileWatchNative,
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

/// Every mutable state source the main loop touches, bundled.
///
/// Per course-correction #4: groups the nine per-domain state
/// structs so the main loop signature stops growing with each new
/// milestone. Rust allows disjoint-field `&mut` borrows at compile
/// time, so dispatch + memo call sites still extract the sources
/// they actually need without runtime cost.
#[derive(Default)]
pub struct Sources {
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
    pub lsp_notified: imbl::HashMap<CanonPath, LspNotified>,
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
    /// observations of current source state.
    pub lsp_requested_state_sum: Option<BufferStateSum>,
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
    /// M26-followup — server-registered
    /// `workspace/didChangeWatchedFiles` glob sets keyed by
    /// `(server, registration_id)`. External-fact source per G1
    /// (the server told us); folded from
    /// `LspEvent::WatchedFilesRegistered/Unregistered` in the
    /// ingest phase. The `lsp_watched_file_notifications`
    /// dispatch helper walks `file_watch.recent_events` against
    /// these globs to fan out
    /// `LspCmd::DidChangeWatchedFiles` per-server.
    pub lsp_watched_globs: led_state_lsp::LspWatchedGlobs,
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
    /// re-evaluated next tick. Always derivable from sources; we
    /// store it as a flag because we don't want to scan every
    /// tab every tick.
    pub resume_check_pending: bool,
    /// Per-buffer undo persistence bookkeeping. Tracks how far
    /// each path's `history.past` has been flushed to the
    /// session DB so subsequent flushes ship only the newly-
    /// finalised groups (mirrors legacy's incremental append).
    pub undo_persistence: imbl::HashMap<CanonPath, UndoPersistTracker>,
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
    pub undo_flush_debounce: imbl::HashMap<CanonPath, UndoFlushDebounce>,
    /// Symlink resolution chain for every path the user has
    /// opened, keyed by canonical path. Populated at tab-open
    /// time (main.rs CLI, find-file commit, browser open) so the
    /// load-completion handler can detect the language from the
    /// user-typed name even when canonicalization has stripped
    /// the informative extension. Mirrors legacy led's
    /// `PathChain` → `LanguageId::from_chain` routing.
    pub path_chains: std::collections::HashMap<CanonPath, PathChain>,
    /// M26 — driver-owned source for the file-watch driver.
    /// Holds the actual side of the desired/actual diff that
    /// produces `FileWatchCmd::Watch` / `Unwatch`, plus the
    /// queue of `FileWatchEvent`s the worker emitted since the
    /// last drain. Memos in `query.rs` read this to derive the
    /// per-tick reread / sync-check / browser-refresh
    /// dispatches; the runtime calls
    /// `FileWatchState::clear_events` at the end of each
    /// Execute phase.
    pub file_watch: led_driver_file_watch_core::FileWatchState,
    /// M26 — monotonic id allocator for `WatchSeq`. Bumped
    /// whenever `desired_watch_set` introduces a path that
    /// hasn't been seen this session. Persisted-in-memory only;
    /// there's no on-disk catalog to reconcile against.
    pub watch_id_seq: WatchSeq,
    /// "Now" as a source field per the EXAMPLE-ARCH "Time is a
    /// source field" prescription. Set once at the top of every
    /// ingest tick from `Instant::now()`; everything downstream
    /// reads via [`Clock::now`] or [`query::ClockInput`] so a
    /// single syscall per tick covers every consumer and
    /// time-dependent memos can cache-hit on idle.
    pub clock: Clock,
}

/// Clock source. One field, mutated once per tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    pub now: Instant,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            now: Instant::now(),
        }
    }
}

/// Per-buffer record of the last `(version, saved_version)`
/// pair the runtime told the LSP driver about. The execute
/// phase emits another `BufferChanged` whenever either
/// coordinate has advanced past this record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LspNotified {
    pub version: BufferVersion,
    pub saved_version: SavedVersion,
}

/// Per-buffer debounce state for the undo-flush dispatch. The
/// flush fires once 200ms have elapsed without `last_version`
/// changing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UndoFlushDebounce {
    pub last_version: BufferVersion,
    pub first_seen: Instant,
}

/// Per-buffer state tracking what we've shipped to the
/// `undo_entries` table. Mirrors legacy's `BufferState::
/// {chain_id, persisted_undo_len, last_seen_seq}` triple.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UndoPersistTracker {
    /// UUID-ish chain id we generate on first flush per
    /// buffer-session. Stable across the rest of the session;
    /// regenerated on session restore (with a new chain) when
    /// the disk content's hash differs from the persisted
    /// `content_hash`.
    pub chain_id: ChainId,
    /// `past.len()` we've already flushed. Next flush ships
    /// `history.past_groups()[persisted_len..]`.
    pub persisted_len: usize,
    /// Latest `seq` returned by the session driver after a
    /// successful flush. Used by future cross-instance sync.
    pub last_seq: UndoDbSeq,
}

/// Run-time seam: the single thing the main loop sees. Owns nothing
/// — just bundles borrowed views of the sources, drivers, config,
/// wake signal, trace sink, and stdout writer. Shrinks `run()` to
/// a one-arg function.
pub struct World<'a, W: Write> {
    pub sources: &'a mut Sources,
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
    /// `--no-workspace` CLI flag. When `true` the runtime skips
    /// session init AND the M26 file-watch dispatch — there's no
    /// workspace root to track, no session DB to sync. The flag
    /// is checked alongside `session.init_done` because
    /// standalone mode short-circuits init by setting
    /// `session.init_done = true` without ever dispatching
    /// `SessionCmd::Init`, which the file-watch gate needs to
    /// distinguish from a normal-mode init that completed.
    pub no_workspace: bool,
}

/// Run the main loop until dispatch signals quit.
pub fn run<W: Write>(world: &mut World<'_, W>) -> io::Result<()> {
    // Resolve the per-user config dir + watcher's `notify/`
    // subdir once. Both are inputs to the file-watch and
    // session-init dispatch paths, and both involve
    // `create_dir_all` + `realpath` syscalls. Doing them per-tick
    // (the original code called `cli_config_dir.or_else(
    // config_dir_for_session)` from three sites inside `loop {}`)
    // dominated the main thread on the profiler — `__getattrlist`
    // accounted for ~70 % of main-thread samples on an idle
    // editor. Resolving once here amortises the syscalls over
    // process lifetime; nothing here changes at runtime.
    let resolved_config_dir: Option<CanonPath> = world
        .cli_config_dir
        .and_then(|p| {
            std::fs::create_dir_all(p).ok()?;
            Some(led_core::UserPath::new(p).canonicalize())
        })
        .or_else(config_dir_for_session);
    let resolved_notify_dir: Option<CanonPath> = resolved_config_dir.as_ref().map(|cfg| {
        let raw = cfg.as_path().join("notify");
        // Same belt-and-braces as the original
        // `compute_watch_actions` body: ensure `<config>/notify/`
        // exists before the watcher subscribes. Idempotent — the
        // session driver's init will hit the same path on its
        // own thread.
        let _ = std::fs::create_dir_all(&raw);
        led_core::UserPath::new(raw).canonicalize()
    });
    let env = phases::TickEnv {
        drivers: world.drivers,
        keymap: world.keymap,
        theme: world.theme,
        wake: world.wake,
        trace: world.trace,
        no_workspace: world.no_workspace,
        resolved_config_dir: &resolved_config_dir,
        resolved_notify_dir: &resolved_notify_dir,
    };
    let stdout = &mut *world.stdout;
    let mut last_frame: Option<Frame> = None;
    let mut chord = ChordState::default();

    loop {
        // ── Ingest ──────────────────────────────────────────────
        phases::ingest::ingest_clock(world.sources);
        phases::ingest::ingest_file_watch(world.sources, &env);
        phases::ingest::ingest_file_completions(world.sources, &env);
        phases::ingest::ingest_lsp_events(world.sources, &env);
        phases::ingest::ingest_file_writes(world.sources, &env);
        phases::ingest::ingest_fs_list(world.sources, &env);
        phases::ingest::ingest_find_file(world.sources, &env);
        phases::ingest::ingest_file_search(world.sources, &env);
        phases::ingest::ingest_syntax(world.sources, &env);
        phases::ingest::ingest_session(world.sources, &env);
        phases::ingest::ingest_git(world.sources, &env);
        phases::ingest::ingest_clipboard(world.sources, &env);
        // Drain terminal events + dispatch each. Quit short-circuits
        // back to the next iteration's gate; Suspend handles its
        // alt-screen round-trip inline.
        let _ = phases::dispatch_phase::dispatch_input(
            world.sources,
            &env,
            stdout,
            &mut chord,
            &mut last_frame,
        );
        phases::dispatch_phase::cleanup_orphans(world.sources);
        if phases::dispatch_phase::check_quit_gate(world.sources, &env) {
            break Ok(());
        }
        phases::ingest::ingest_browser_snap(world.sources);

        // ── Query ───────────────────────────────────────────────
        let q = phases::query_phase::run(world.sources);

        // ── Execute ─────────────────────────────────────────────
        phases::execute_phase::run(world.sources, &env, &q);

        // ── LSP dispatch ──────────────────────────────────────
        phases::lsp_dispatch::run(world.sources, &env);

        // ── Session dispatch ──────────────────────────────────
        phases::session_dispatch::run(world.sources, &env);

        // ── File-watch dispatch + clipboard + FlushUndo + LSP cmds ─
        phases::file_watch_dispatch::run(world.sources, &env);

        // ── Git dispatch + file-watch event drain ─────────────
        phases::git_dispatch::run(world.sources, &env);

        // ── Render ──────────────────────────────────────────────
        phases::render_phase::run(world.sources, &env, stdout, q.frame, &mut last_frame)?;

        // ── Wait ──────────────────────────────────────────────
        match phases::wait_phase::run(world.sources, &env) {
            Some(()) => {}
            None => break Ok(()),
        }
    }
}

// session / persistence helpers moved to `apply::session`.

// ── M26 file-watch dispatch helpers ──────────────────────────

/// Stable "kind tag" for our three baseline registrations. The
/// runtime uses these as bit-pattern WatchSeq ids so memos /
/// dispatch helpers can identify them without consulting the
/// registry. Per-buffer ids are minted from `watch_id_seq`
/// starting at 0.
pub(crate) const WATCHER_ID_ROOT: WatchSeq = WatchSeq(u64::MAX);
pub(crate) const WATCHER_ID_NOTIFY_DIR: WatchSeq = WatchSeq(u64::MAX - 1);

// fs / lsp / edit helpers moved to the `apply` submodule.

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
    // M23: pre-compile every supported language's indent +
    // imports tree-sitter query on the main thread BEFORE the
    // syntax worker spawns. Without this warm-up, dispatch's
    // first call to `Query::new` (when the user presses Tab /
    // Ctrl-x i) races the worker thread's parser-bound query
    // compilation and stalls in tree-sitter's FFI on macOS.
    // Pre-warming costs ~10ms once and keeps the dispatch tick
    // deadlock-free.
    led_state_syntax::indent::precompile_all_queries();
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
    let (file_watch, file_watch_native) = led_driver_file_watch_native::spawn(
        trace.clone().as_file_watch_trace(),
        wake.notifier.clone(),
    );
    let (input, input_native) = led_driver_terminal_native::spawn(
        trace.clone().as_terminal_trace(),
        wake.notifier.clone(),
    )?;
    let output = TerminalOutputDriver::new(trace.as_terminal_trace());
    Ok(Drivers {
        file_watch,
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
        _file_watch_native: file_watch_native,
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

    use led_core::{BufferVersion, CanonPath, ChainId, SavedVersion, ServerId};
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
    pub(crate) struct FileWatchTraceAdapter;

    impl led_driver_buffers_core::Trace for FileTraceAdapter {
        fn file_load_start(&self, path: &CanonPath) {
            self.0.file_load_start(path);
        }
        fn file_load_done(&self, path: &CanonPath, result: &Result<Arc<Rope>, String>) {
            self.0.file_load_done(path, result);
        }
        fn file_save_start(&self, path: &CanonPath, version: BufferVersion) {
            self.0.file_save_start(path, SavedVersion(version.0));
        }
        fn file_save_done(
            &self,
            path: &CanonPath,
            version: BufferVersion,
            result: &Result<(), String>,
        ) {
            self.0
                .file_save_done(path, SavedVersion(version.0), result);
        }
        fn file_save_as_start(&self, from: &CanonPath, to: &CanonPath) {
            self.0.file_save_as_start(from, to);
        }
        fn file_save_as_done(&self, from: &CanonPath, to: &CanonPath, result: &Result<(), String>) {
            self.0.file_save_as_done(from, to, result);
        }
        fn file_reread_start(&self, path: &CanonPath) {
            self.0.file_reread_start(path);
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
            version: BufferVersion,
            language: led_state_syntax::Language,
        ) {
            self.0.syntax_parse_start(path, version, language);
        }
        fn syntax_parse_done(&self, path: &CanonPath, version: BufferVersion, ok: bool) {
            self.0.syntax_parse_done(path, version, ok);
        }
    }

    impl led_driver_lsp_core::Trace for LspTraceAdapter {
        fn lsp_server_started(&self, server: &ServerId) {
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
            server: &ServerId,
            method: &str,
            id: i64,
            path_uri: Option<&str>,
        ) {
            self.0.lsp_send_request(server, method, id, path_uri);
        }
        fn lsp_send_notification(
            &self,
            server: &ServerId,
            method: &str,
            path_uri: Option<&str>,
            version: Option<i32>,
        ) {
            self.0
                .lsp_send_notification(server, method, path_uri, version);
        }
        fn lsp_recv_response(&self, server: &ServerId, id: i64) {
            self.0.lsp_recv_response(server, id);
        }
        fn lsp_recv_notification(&self, server: &ServerId, method: &str) {
            self.0.lsp_recv_notification(server, method);
        }
        fn lsp_recv_request(&self, server: &ServerId, method: &str, id: i64) {
            self.0.lsp_recv_request(server, method, id);
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
        fn session_flush_undo(&self, path: &CanonPath, chain_id: &ChainId) {
            self.0.workspace_flush_undo(path, chain_id);
        }
        fn session_check_sync(&self, path: &CanonPath) {
            self.0.workspace_check_sync(path);
        }
    }

    impl led_driver_file_watch_core::Trace for FileWatchTraceAdapter {
        fn file_watch_event(
            &self,
            _id: led_core::WatchSeq,
            _path: &CanonPath,
            _kinds: led_driver_file_watch_core::ChangeKinds,
        ) {
            // Watch events are input-side; not traced in
            // dispatched.snap.
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
    pub(crate) fn as_file_watch_trace(&self) -> Arc<dyn led_driver_file_watch_core::Trace> {
        Arc::new(trace_adapter::FileWatchTraceAdapter)
    }
    pub(crate) fn as_session_trace(&self) -> Arc<dyn led_driver_session_core::Trace> {
        Arc::new(trace_adapter::SessionTraceAdapter(self.inner()))
    }
}

#[cfg(test)]
mod tests {
    //! Ingest-level invariants (course-correct #6).

    use super::*;
    use crate::apply::edit::{auto_advance_arrow_follow, seed_edit_from_load};
    use crate::apply::fs::{
        apply_workspace_tree_delta, invalidate_subtree, is_git_internal, is_git_sentinel,
    };
    use crate::apply::lsp::{LspEditApply, LspGotoApply};
    use led_core::UserPath;
    use led_state_buffer_edits::EditedBuffer;
    use led_state_syntax::{Language, SyntaxState};
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
    /// `SyntaxOut` to land + populate `Sources.syntax`. Verifies the
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
            latest_goto_seq: Some(led_core::LspRequestSeq(42)),
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
            led_core::LspRequestSeq(42),
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
            latest_goto_seq: Some(led_core::LspRequestSeq(99)),
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
            /* stale */ led_core::LspRequestSeq(7),
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
        assert_eq!(lsp_pending.latest_goto_seq, Some(led_core::LspRequestSeq(99)));
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
            latest_goto_seq: Some(led_core::LspRequestSeq(1)),
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
            led_core::LspRequestSeq(1),
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
            latest_goto_seq: Some(led_core::LspRequestSeq(1)),
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
            led_core::LspRequestSeq(1),
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
            latest_goto_seq: Some(led_core::LspRequestSeq(1)),
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
        .apply(led_core::LspRequestSeq(1), None);
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
            latest_rename_seq: Some(led_core::LspRequestSeq(7)),
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
        .apply(led_core::LspRequestSeq(7), EditsOrigin::Rename, &file_edits);
        let eb = edits.buffers.get(&path).unwrap();
        assert_eq!(eb.rope.to_string(), "bar + bar");
        assert!(eb.version.0 > 0);
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
        eb.version = BufferVersion(1);
        edits.buffers.insert(path.clone(), eb);
        let mut alerts = AlertState::default();
        let mut lsp_extras = led_state_lsp::LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        lsp_pending.pending_save_after_format.insert(path.clone());
        lsp_pending
            .latest_format_seq
            .insert(path.clone(), led_core::LspRequestSeq(1));

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
        .apply(led_core::LspRequestSeq(1), EditsOrigin::Format, &file_edits);
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
        lsp_pending
            .latest_format_seq
            .insert(path.clone(), led_core::LspRequestSeq(1));

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
        .apply(led_core::LspRequestSeq(1), EditsOrigin::Format, &file_edits);
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
        eb.version = BufferVersion(1); // dirty (saved_version still 0)
        edits.buffers.insert(path.clone(), eb);
        let mut alerts = AlertState::default();
        let mut lsp_extras = led_state_lsp::LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        lsp_pending.pending_save_after_format.insert(path.clone());
        lsp_pending
            .latest_format_seq
            .insert(path.clone(), led_core::LspRequestSeq(42));
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
        .apply(led_core::LspRequestSeq(42), EditsOrigin::Format, &file_edits);
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
        eb.version = BufferVersion(1);
        edits.buffers.insert(path.clone(), eb);
        let mut alerts = AlertState::default();
        let mut lsp_extras = led_state_lsp::LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        lsp_pending.pending_save_after_format.insert(path.clone());
        lsp_pending
            .latest_format_seq
            .insert(path.clone(), led_core::LspRequestSeq(5));
        let file_edits = std::sync::Arc::new(Vec::new());
        let _ = &mut lsp_extras;
        let tabs = led_state_tabs::Tabs::default();
        LspEditApply {
            edits: &mut edits,
            tabs: &tabs,
            alerts: &mut alerts,
            lsp_pending: &mut lsp_pending,
        }
        .apply(led_core::LspRequestSeq(5), EditsOrigin::Format, &file_edits);
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
            latest_rename_seq: Some(led_core::LspRequestSeq(99)),
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
        .apply(
            /* stale */ led_core::LspRequestSeq(5),
            EditsOrigin::Rename,
            &file_edits,
        );
        // Buffer unchanged, seq preserved.
        assert_eq!(edits.buffers[&path].rope.to_string(), "foo");
        assert_eq!(lsp_pending.latest_rename_seq, Some(led_core::LspRequestSeq(99)));
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
            latest_goto_seq: Some(led_core::LspRequestSeq(1)),
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
            led_core::LspRequestSeq(1),
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

    // ── workspace_tree_delta — surgical refresh ───────────────────

    fn make_event(path: &CanonPath, kinds_bits: u8) -> led_driver_file_watch_core::FileWatchEvent {
        use led_driver_file_watch_core::{ChangeKinds, FileWatchEvent};
        FileWatchEvent::Changed {
            id: WATCHER_ID_ROOT,
            path: path.clone(),
            kinds: ChangeKinds::from_bits(kinds_bits),
        }
    }

    fn fw_with(events: Vec<led_driver_file_watch_core::FileWatchEvent>)
        -> led_driver_file_watch_core::FileWatchState
    {
        use led_driver_file_watch_core::FileWatchState;
        let mut s = FileWatchState::default();
        let q = s.recent_events.entry(WATCHER_ID_ROOT).or_default();
        for e in events {
            q.push_back(e);
        }
        s
    }

    /// Real-disk fixture so `stat_kind` works.
    fn workspace_fs(root: &std::path::Path) -> FsTree {
        let canon_root = led_core::UserPath::new(root.to_path_buf()).canonicalize();
        let mut fs = FsTree {
            root: Some(canon_root.clone()),
            ..FsTree::default()
        };
        // Empty initial listing — caller seeds whatever entries
        // the test cares about.
        fs.dir_contents.insert(canon_root, imbl::Vector::new());
        fs
    }

    #[test]
    fn delta_suppresses_git_objects_no_listing_change() {
        use led_driver_file_watch_core::ChangeKinds;
        let dir = tempfile::tempdir().unwrap();
        let mut fs = workspace_fs(dir.path());
        let edits = BufferEdits::default();

        let p = canon(dir.path().join(".git/objects/ab/cdef").to_str().unwrap());
        let fw = fw_with(vec![make_event(&p, ChangeKinds::CREATED | ChangeKinds::REMOVED)]);

        let git_scan = apply_workspace_tree_delta(&fw, &edits, &mut fs);
        assert!(!git_scan, ".git/objects must not request a rescan");
        // Root listing untouched.
        let root_canon = led_core::UserPath::new(dir.path().to_path_buf()).canonicalize();
        assert!(fs.dir_contents[&root_canon].is_empty());
    }

    #[test]
    fn delta_git_sentinel_triggers_scan_only() {
        use led_driver_file_watch_core::ChangeKinds;
        let dir = tempfile::tempdir().unwrap();
        let mut fs = workspace_fs(dir.path());
        let edits = BufferEdits::default();
        let root_canon = led_core::UserPath::new(dir.path().to_path_buf()).canonicalize();

        let cases = [
            ".git/index",
            ".git/HEAD",
            ".git/refs/heads/main",
        ];
        for rel in cases {
            let p = canon(dir.path().join(rel).to_str().unwrap());
            let fw = fw_with(vec![make_event(&p, ChangeKinds::CREATED)]);
            let git_scan = apply_workspace_tree_delta(&fw, &edits, &mut fs);
            assert!(git_scan, "sentinel {rel} must request a rescan");
            assert!(
                fs.dir_contents[&root_canon].is_empty(),
                "sentinel {rel} must not touch root listing"
            );
        }
    }

    #[test]
    fn delta_create_inserts_entry_into_cached_parent() {
        use led_driver_file_watch_core::ChangeKinds;
        let dir = tempfile::tempdir().unwrap();
        let new_file = dir.path().join("hello.txt");
        std::fs::write(&new_file, b"x").unwrap();
        let mut fs = workspace_fs(dir.path());
        let edits = BufferEdits::default();
        let root_canon = led_core::UserPath::new(dir.path().to_path_buf()).canonicalize();
        let new_canon = canon(new_file.to_str().unwrap());

        let fw = fw_with(vec![make_event(&new_canon, ChangeKinds::CREATED)]);
        let _ = apply_workspace_tree_delta(&fw, &edits, &mut fs);

        let entries = &fs.dir_contents[&root_canon];
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "hello.txt");
        assert_eq!(entries[0].path, new_canon);
    }

    #[test]
    fn delta_create_skipped_when_parent_not_cached() {
        // Mirrors the "thousands of cargo target/ events while
        // target/ is collapsed" scenario: we must NOT stat or
        // insert anything for events whose parent listing we
        // never expanded.
        use led_driver_file_watch_core::ChangeKinds;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let buried = target.join("debug").join("foo");
        std::fs::create_dir_all(buried.parent().unwrap()).unwrap();
        std::fs::write(&buried, b"x").unwrap();

        let mut fs = workspace_fs(dir.path());
        // Note: target/ NOT in dir_contents — collapsed.
        let edits = BufferEdits::default();
        let root_canon = led_core::UserPath::new(dir.path().to_path_buf()).canonicalize();

        let fw = fw_with(vec![make_event(
            &canon(buried.to_str().unwrap()),
            ChangeKinds::CREATED,
        )]);
        let _ = apply_workspace_tree_delta(&fw, &edits, &mut fs);

        // Root unchanged. No magical stats happened in target/.
        assert!(fs.dir_contents[&root_canon].is_empty());
        let target_canon = led_core::UserPath::new(target.clone()).canonicalize();
        assert!(!fs.dir_contents.contains_key(&target_canon));
    }

    #[test]
    fn delta_remove_drops_entry_and_invalidates_subtree() {
        use led_driver_file_watch_core::ChangeKinds;
        use led_driver_fs_list_core::{DirEntry, DirEntryKind};
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("src");
        std::fs::create_dir(&sub).unwrap();

        let mut fs = workspace_fs(dir.path());
        let root_canon = led_core::UserPath::new(dir.path().to_path_buf()).canonicalize();
        let sub_canon = led_core::UserPath::new(sub.clone()).canonicalize();

        // Seed: root contains "src", and src/ has a cached
        // listing of one file.
        fs.dir_contents.insert(
            root_canon.clone(),
            imbl::Vector::from_iter([DirEntry {
                name: "src".into(),
                path: sub_canon.clone(),
                kind: DirEntryKind::Directory,
            }]),
        );
        fs.dir_contents.insert(
            sub_canon.clone(),
            imbl::Vector::from_iter([DirEntry {
                name: "main.rs".into(),
                path: canon(sub.join("main.rs").to_str().unwrap()),
                kind: DirEntryKind::File,
            }]),
        );

        let edits = BufferEdits::default();
        let fw = fw_with(vec![make_event(&sub_canon, ChangeKinds::REMOVED)]);
        let _ = apply_workspace_tree_delta(&fw, &edits, &mut fs);

        // src removed from root listing.
        assert!(fs.dir_contents[&root_canon].is_empty());
        // Cached subtree invalidated.
        assert!(!fs.dir_contents.contains_key(&sub_canon));
    }

    #[test]
    fn delta_create_dedup_when_event_repeats() {
        use led_driver_file_watch_core::ChangeKinds;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, b"x").unwrap();
        let mut fs = workspace_fs(dir.path());
        let edits = BufferEdits::default();
        let root_canon = led_core::UserPath::new(dir.path().to_path_buf()).canonicalize();
        let p = canon(f.to_str().unwrap());

        let fw = fw_with(vec![
            make_event(&p, ChangeKinds::CREATED),
            make_event(&p, ChangeKinds::CREATED),
        ]);
        let _ = apply_workspace_tree_delta(&fw, &edits, &mut fs);
        assert_eq!(fs.dir_contents[&root_canon].len(), 1);
    }

    #[test]
    fn delta_create_skips_hidden_dotfile() {
        use led_driver_file_watch_core::ChangeKinds;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join(".secret");
        std::fs::write(&f, b"x").unwrap();
        let mut fs = workspace_fs(dir.path());
        let edits = BufferEdits::default();
        let root_canon = led_core::UserPath::new(dir.path().to_path_buf()).canonicalize();
        let p = canon(f.to_str().unwrap());

        let fw = fw_with(vec![make_event(&p, ChangeKinds::CREATED)]);
        let _ = apply_workspace_tree_delta(&fw, &edits, &mut fs);
        assert!(fs.dir_contents[&root_canon].is_empty());
    }

    #[test]
    fn delta_create_failed_stat_is_dropped() {
        // Path doesn't actually exist on disk (file deleted
        // before we got to the stat). We must not insert a
        // ghost entry.
        use led_driver_file_watch_core::ChangeKinds;
        let dir = tempfile::tempdir().unwrap();
        let mut fs = workspace_fs(dir.path());
        let edits = BufferEdits::default();
        let root_canon = led_core::UserPath::new(dir.path().to_path_buf()).canonicalize();
        let ghost = canon(dir.path().join("ghost").to_str().unwrap());

        let fw = fw_with(vec![make_event(&ghost, ChangeKinds::CREATED)]);
        let _ = apply_workspace_tree_delta(&fw, &edits, &mut fs);
        assert!(fs.dir_contents[&root_canon].is_empty());
    }

    #[test]
    fn delta_modified_only_event_is_dropped() {
        use led_driver_file_watch_core::ChangeKinds;
        let dir = tempfile::tempdir().unwrap();
        let mut fs = workspace_fs(dir.path());
        let edits = BufferEdits::default();
        let root_canon = led_core::UserPath::new(dir.path().to_path_buf()).canonicalize();
        let p = canon(dir.path().join("a.txt").to_str().unwrap());

        let fw = fw_with(vec![make_event(&p, ChangeKinds::MODIFIED)]);
        let git = apply_workspace_tree_delta(&fw, &edits, &mut fs);
        assert!(!git);
        assert!(fs.dir_contents[&root_canon].is_empty());
    }

    #[test]
    fn invalidate_subtree_clears_failed_dirs_too() {
        // The companion to `dir_contents` cleanup: a subtree
        // removal must also drop any "we tried, it failed"
        // markers under that prefix, otherwise the failure
        // verdict outlives the inode it was about and a later
        // re-mkdir can't get past the gate.
        let mut fs = workspace_fs(std::path::Path::new("/proj"));
        fs.failed_dirs.insert(canon("/proj/sub"));
        fs.failed_dirs.insert(canon("/proj/sub/inner"));
        fs.failed_dirs.insert(canon("/proj/keepme"));

        invalidate_subtree(&mut fs, &canon("/proj/sub"));

        assert!(!fs.failed_dirs.contains(&canon("/proj/sub")));
        assert!(!fs.failed_dirs.contains(&canon("/proj/sub/inner")));
        assert!(fs.failed_dirs.contains(&canon("/proj/keepme")));
    }

    #[test]
    fn delta_create_clears_failed_ancestors() {
        // Recovery path: when the watcher reports a CREATE for a
        // path nested inside a previously-failed dir (a re-mkdir
        // or git checkout that brings the tree back), every
        // ancestor up to the workspace root must be dropped from
        // `failed_dirs` so the next tick relists them. Without
        // this hook a deleted-then-restored expansion would never
        // re-populate the sidebar even though the watcher saw
        // the recreation events.
        use led_driver_file_watch_core::ChangeKinds;
        let dir = tempfile::tempdir().unwrap();
        // Build a real subtree on disk so the CREATE event has
        // something to reference. The watcher delta only looks at
        // event metadata, but we use canon paths under the temp
        // root so the ancestor walk has somewhere to terminate.
        let nested = dir.path().join("revived").join("inner");
        std::fs::create_dir_all(&nested).unwrap();
        let leaf = nested.join("file.txt");
        std::fs::write(&leaf, b"x").unwrap();

        let mut fs = workspace_fs(dir.path());
        let revived_canon =
            led_core::UserPath::new(dir.path().join("revived")).canonicalize();
        let inner_canon = led_core::UserPath::new(nested.clone()).canonicalize();
        // Pre-seed the failures to mimic "stale persisted
        // expansions whose dirs were missing earlier this session".
        fs.failed_dirs.insert(revived_canon.clone());
        fs.failed_dirs.insert(inner_canon.clone());

        let edits = BufferEdits::default();
        let leaf_canon = canon(leaf.to_str().unwrap());
        let fw = fw_with(vec![make_event(&leaf_canon, ChangeKinds::CREATED)]);
        let _ = apply_workspace_tree_delta(&fw, &edits, &mut fs);

        assert!(
            !fs.failed_dirs.contains(&revived_canon),
            "ancestor `revived/` must be cleared"
        );
        assert!(
            !fs.failed_dirs.contains(&inner_canon),
            "ancestor `revived/inner/` must be cleared"
        );
    }

    #[test]
    fn is_git_internal_walks_components() {
        assert!(is_git_internal(&canon("/proj/.git/objects/ab")));
        assert!(is_git_internal(&canon("/proj/sub/.git/HEAD")));
        assert!(!is_git_internal(&canon("/proj/src/main.rs")));
        // A file literally named ".gitignore" is NOT inside `.git/`.
        assert!(!is_git_internal(&canon("/proj/.gitignore")));
    }

    #[test]
    fn is_git_sentinel_matches_index_head_refs_only() {
        assert!(is_git_sentinel(&canon("/proj/.git/index")));
        assert!(is_git_sentinel(&canon("/proj/.git/HEAD")));
        assert!(is_git_sentinel(&canon("/proj/.git/refs/heads/main")));
        assert!(is_git_sentinel(&canon("/proj/.git/refs/tags/v1")));
        assert!(!is_git_sentinel(&canon("/proj/.git/index.lock")));
        assert!(!is_git_sentinel(&canon("/proj/.git/objects/ab/cd")));
        assert!(!is_git_sentinel(&canon("/proj/.git/refs"))); // refs/ itself
        assert!(!is_git_sentinel(&canon("/proj/src/main.rs")));
    }
}
