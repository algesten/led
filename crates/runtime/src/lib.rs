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
use led_core::Notifier;
use led_driver_terminal_core::{Dims, Frame, KeyEvent, TermEvent, Terminal, TerminalInputDriver};
use led_driver_terminal_native::{TerminalInputNative, TerminalOutputDriver};
use led_driver_find_file_core::FindFileDriver;
use led_driver_find_file_native::FindFileNative;
use led_driver_fs_list_core::FsListDriver;
use led_driver_fs_list_native::FsListNative;
use led_state_alerts::AlertState;
use led_state_browser::{BrowserUi, FsTree, rebuild_entries};
use led_state_buffer_edits::{BufferEdits, EditedBuffer};
use led_state_clipboard::ClipboardState;
use led_state_find_file::FindFileState;
use led_state_jumps::JumpListState;
use led_state_kill_ring::KillRing;
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

    // Held only for lifetime management; detached on drop.
    _file_native: FileReadNative,
    _file_write_native: FileWriteNative,
    _input_native: TerminalInputNative,
    _clipboard_native: ClipboardNative,
    _fs_list_native: FsListNative,
    _find_file_native: FindFileNative,
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
}

/// Run-time seam: the single thing the main loop sees. Owns nothing
/// — just bundles borrowed views of the atoms, drivers, config,
/// wake signal, trace sink, and stdout writer. Shrinks `run()` to
/// a one-arg function.
pub struct World<'a, W: Write> {
    pub atoms: &'a mut Atoms,
    pub drivers: &'a Drivers,
    pub keymap: &'a Keymap,
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
    } = &mut *world.atoms;
    let drivers = world.drivers;
    let wake = world.wake;
    let keymap = world.keymap;
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
        // alert is live.
        alerts.expire_info(Instant::now());

        // Seed BufferEdits from newly-Ready loads. `seed_edit_from_load`
        // enforces the discipline that an existing edit entry wins
        // over a late-arriving load completion (course-correct #6).
        // `process` returns an empty Vec on idle ticks — no heap
        // alloc on the happy path.
        let completions = drivers.file.process(store);
        for completion in completions {
            seed_edit_from_load(edits, completion.path, completion.rope);
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
        let had_listing = !fs_completions.is_empty();
        for done in fs_completions {
            if let Ok(entries) = done.result {
                fs.dir_contents
                    .insert(done.path, imbl::Vector::from_iter(entries));
            }
        }
        if had_listing {
            rebuild_entries(browser, fs);
        }

        // Apply find-file completions. Late arrivals whose `dir` +
        // `prefix` no longer match the overlay's current input are
        // dropped — legacy's "expected_dir" discipline. Matching
        // completions replace `state.completions` wholesale.
        for done in drivers.find_file.process() {
            let Some(ff) = find_file.as_mut() else {
                continue;
            };
            let (dir_part, prefix) = led_state_find_file::split_input(&ff.input);
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
        }

        // Apply clipboard completions: either paste the text at the
        // tab the yank was issued from, or on empty/error fall back
        // to the kill ring. Writes only clear the in-flight bit.
        for done in drivers.clipboard.process() {
            match done.result {
                Ok(ClipboardResult::Text(Some(text))) => {
                    if let Some(target) = clip.pending_yank.take() {
                        dispatch::apply_yank(tabs, edits, target, &text);
                    }
                    clip.read_in_flight = false;
                }
                Ok(ClipboardResult::Text(None)) | Err(_) => {
                    // Empty clipboard or read failure — fall back to
                    // the kill ring's latest entry.
                    if let Some(target) = clip.pending_yank.take()
                        && let Some(fallback) = kill_ring.latest.clone()
                    {
                        dispatch::apply_yank(tabs, edits, target, &fallback);
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
        );
        let find_file_actions = find_file_action(FindFileInput::new(find_file));
        let frame = render_frame(
            TerminalDimsInput::new(terminal),
            EditedBuffersInput::new(edits),
            StoreLoadedInput::new(store),
            TabsActiveInput::new(tabs),
            AlertsInput::new(alerts),
            BrowserUiInput::new(browser),
            FindFileInput::new(find_file),
        );

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

        // Sync-clear pending_saves for the paths we're about to
        // dispatch — the execute-pattern discipline that prevents
        // the next tick's query from re-emitting the same saves.
        for action in &save_actions {
            let led_driver_buffers_core::SaveAction::Save { path, .. } = action;
            edits.pending_saves.remove(path);
        }
        drivers.file_write.execute(save_actions.iter());

        // Saved state becomes the new baseline: truncate each saved
        // buffer's undo history and emit the paired
        // `WorkspaceClearUndo` trace. Matches legacy.
        for action in &save_actions {
            let led_driver_buffers_core::SaveAction::Save { path, .. } = action;
            if let Some(eb) = edits.buffers.get_mut(path) {
                eb.history.clear();
            }
            trace.workspace_clear_undo(path);
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
                drivers.output.execute(f, last_frame.as_ref(), stdout)?;
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
        let timeout = nearest_deadline(alerts)
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
pub fn nearest_deadline(alerts: &AlertState) -> Option<Instant> {
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
    // Future timer sources: add `consider(...)` lines here.
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
    match edits.buffers.entry(path) {
        Entry::Vacant(v) => {
            v.insert(EditedBuffer::fresh(rope));
            true
        }
        Entry::Occupied(_) => false,
    }
}

/// Convenience constructor: spawns both drivers with a shared trace
/// using the desktop `*-native` implementations. Every driver gets a
/// clone of the wake [`Notifier`]; each completion signals the main
/// loop so it wakes immediately.
pub fn spawn_drivers(trace: SharedTrace, wake: &Wake) -> io::Result<Drivers> {
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
        _file_native: file_native,
        _file_write_native: file_write_native,
        _input_native: input_native,
        _clipboard_native: clipboard_native,
        _fs_list_native: fs_list_native,
        _find_file_native: find_file_native,
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
}
