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
use led_state_alerts::AlertState;
use led_state_browser::{BrowserUi, FsTree};
use led_state_buffer_edits::{BufferEdits, EditedBuffer};
use led_state_clipboard::ClipboardState;
use led_state_file_search::FileSearchState;
use led_state_find_file::FindFileState;
use led_state_isearch::IsearchState;
use led_state_jumps::JumpListState;
use led_state_kill_ring::KillRing;
use led_state_diagnostics::{
    BufferDiagnostics, DiagnosticsStates, LspServerStatus, LspStatuses,
};
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
pub use dispatch::{dispatch_key, DispatchOutcome, Dispatcher};
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
    /// Symlink resolution chain for every path the user has
    /// opened, keyed by canonical path. Populated at tab-open
    /// time (main.rs CLI, find-file commit, browser open) so the
    /// load-completion handler can detect the language from the
    /// user-typed name even when canonicalization has stripped
    /// the informative extension. Mirrors legacy led's
    /// `PathChain` → `LanguageId::from_chain` routing.
    pub path_chains: std::collections::HashMap<CanonPath, PathChain>,
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
    } = &mut *world.atoms;
    let drivers = world.drivers;
    let wake = world.wake;
    let keymap = world.keymap;
    let theme = world.theme;
    let stdout = &mut *world.stdout;
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
                lsp_notified.insert(completion.path, (version, saved));
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
                    if seq != completions.seq_gen {
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
                    // Stage 6 replaces this with a proper fuzzy
                    // refilter; for now we display the items in
                    // server order unfiltered.
                    let filtered: Vec<usize> = (0..items.len()).collect();
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
                    }
                    alerts.clear_warn(&basename);
                    alerts.set_info(format!("Saved {basename}"), Instant::now(), INFO_TTL);
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
        // (staged in `edits.pending_replace_in_memory`) to produce
        // a single "Replaced N occurrences in M files" alert.
        for done in drivers.file_search.process_replace() {
            let memory = std::mem::take(&mut edits.pending_replace_in_memory);
            let memory_files = memory.len();
            let memory_total: usize = memory.iter().map(|m| m.count).sum();
            let total_files = done.files_changed + memory_files;
            let total = done.total_replacements + memory_total;
            let msg = if total == 0 {
                format!("No occurrences of `{}`.", done.query)
            } else {
                format!(
                    "Replaced {} occurrence{} in {} file{}.",
                    total,
                    if total == 1 { "" } else { "s" },
                    total_files,
                    if total_files == 1 { "" } else { "s" },
                )
            };
            alerts.set_info(msg, Instant::now(), INFO_TTL);
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
        let mut quit = false;
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
                path_chains,
                keymap,
                chord: &mut chord,
            };
            match dispatcher.dispatch(ev) {
                DispatchOutcome::Continue => {}
                DispatchOutcome::Quit => {
                    quit = true;
                    break;
                }
            }
        }
        if quit {
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
        let list_actions = file_list_action(
            FsTreeInput::new(fs),
            BrowserUiInput::new(browser),
            TabsActiveInput::new(tabs),
        );
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
            render_tick,
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

        // Saved state becomes the new baseline: truncate each saved
        // buffer's undo history and emit the paired
        // `WorkspaceClearUndo` trace. Matches legacy.
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
            if let Some(eb) = edits.buffers.get_mut(path) {
                eb.history.clear();
            }
            trace.workspace_clear_undo(path);
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
        for req in completions.pending_requests.drain(..) {
            lsp_cmds.push(LspCmd::RequestCompletion {
                path: req.path,
                seq: req.seq,
                line: req.line,
                col: req.col,
                trigger: req.trigger,
            });
        }
        for resolve in completions.pending_resolves.drain(..) {
            lsp_cmds.push(LspCmd::ResolveCompletion {
                path: resolve.path,
                seq: resolve.seq,
                item: resolve.item,
            });
        }
        if !lsp_cmds.is_empty() {
            drivers.lsp.execute(lsp_cmds.iter());
        }

        // Clipboard actions: a Read when a yank is pending (no read
        // already in flight), a Write when a kill queued clipboard
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

        // ── Render ──────────────────────────────────────────────
        if frame != last_frame {
            if let Some(f) = &frame {
                drivers.output.execute(f, last_frame.as_ref(), theme, stdout)?;
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
        let timeout = nearest_deadline(alerts, find_file, lsp_status)
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
    soonest
}

/// Seed the edit-buffer map from a newly-Ready FS read. The
/// discipline (course-correct #6): an existing entry in `edits`
/// represents the user's edited view of that buffer and is
/// authoritative — a late load completion for the same path is
/// discarded. Returns `true` when a new entry was inserted,
/// `false` when the existing entry absorbed the discard.
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
) -> io::Result<Drivers> {
    let (file, file_native) =
        led_driver_buffers_native::spawn(trace.clone().as_file_trace(), wake.notifier.clone());
    let (file_write, file_write_native) = led_driver_buffers_native::spawn_write(
        trace.clone().as_file_trace(),
        wake.notifier.clone(),
    );
    let (clipboard, clipboard_native) = led_driver_clipboard_native::spawn(
        trace.clone().as_clipboard_trace(),
        wake.notifier.clone(),
    );
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
        _file_native: file_native,
        _file_write_native: file_write_native,
        _input_native: input_native,
        _clipboard_native: clipboard_native,
        _fs_list_native: fs_list_native,
        _find_file_native: find_file_native,
        _file_search_native: file_search_native,
        _syntax_native: syntax_native,
        _lsp_native: lsp_native,
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
        fn clipboard_write_start(&self, bytes: usize) {
            self.0.clipboard_write_start(bytes);
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
}
