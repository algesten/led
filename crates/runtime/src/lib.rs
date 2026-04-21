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
use led_driver_terminal_native::{paint, TerminalInputNative};
use led_driver_fs_list_core::FsListDriver;
use led_driver_fs_list_native::FsListNative;
use led_state_alerts::AlertState;
use led_state_browser::{BrowserUi, FsTree, rebuild_entries};
use led_state_buffer_edits::{BufferEdits, EditedBuffer};
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
    body_model, file_list_action, file_load_action, file_save_action, render_frame,
    side_panel_model, status_bar_model, tab_bar_model, AlertsInput, BrowserUiInput,
    EditedBuffersInput, FsTreeInput, PendingSavesInput, StoreLoadedInput, TabsActiveInput,
    TabsOpenInput, TerminalDimsInput,
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
    pub clipboard: ClipboardDriver,
    pub fs_list: FsListDriver,

    // Held only for lifetime management; detached on drop.
    _file_native: FileReadNative,
    _file_write_native: FileWriteNative,
    _input_native: TerminalInputNative,
    _clipboard_native: ClipboardNative,
    _fs_list_native: FsListNative,
}

/// Allocator for fresh `TabId`s. Counter only; ids are never reused.
#[derive(Debug, Default)]
pub struct TabIdGen(u64);

impl TabIdGen {
    pub fn next(&mut self) -> TabId {
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
    pub alerts: AlertState,
    pub jumps: JumpListState,
    pub browser: BrowserUi,
    pub fs: FsTree,
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
        alerts,
        jumps,
        browser,
        fs,
        store,
        terminal,
    } = &mut *world.atoms;
    let drivers = world.drivers;
    let wake = world.wake;
    let keymap = world.keymap;
    let stdout = &mut *world.stdout;
    let trace = world.trace;
    let mut last_frame: Option<Frame> = None;
    let mut chord = ChordState::default();

    loop {
        // ── Ingest ──────────────────────────────────────────────
        // Clear expired info alerts at the top of each tick — one
        // `Instant::now()` compare per tick, zero allocs when no
        // alert is live.
        alerts.expire_info(Instant::now());

        // Seed BufferEdits from newly-Ready loads. `process` returns
        // an empty Vec on idle ticks (no heap alloc); `or_insert_with`
        // avoids clobbering a buffer the user has already edited if
        // a later reload round-trips through here.
        let completions = drivers.file.process(store);
        for completion in completions {
            edits
                .buffers
                .entry(completion.path)
                .or_insert_with(|| EditedBuffer::fresh(completion.rope));
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

        // Apply clipboard completions: either paste the text at the
        // tab the yank was issued from, or on empty/error fall back
        // to the kill ring. Writes only clear the in-flight bit.
        for done in drivers.clipboard.process() {
            match done.result {
                Ok(ClipboardResult::Text(Some(text))) => {
                    if let Some(target) = kill_ring.pending_yank.take() {
                        dispatch::apply_yank(tabs, edits, target, &text);
                    }
                    kill_ring.read_in_flight = false;
                }
                Ok(ClipboardResult::Text(None)) | Err(_) => {
                    // Empty clipboard or read failure — fall back to
                    // the kill ring's latest entry.
                    if let Some(target) = kill_ring.pending_yank.take()
                        && let Some(fallback) = kill_ring.latest.clone()
                    {
                        dispatch::apply_yank(tabs, edits, target, &fallback);
                    }
                    kill_ring.read_in_flight = false;
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
                alerts,
                jumps,
                browser,
                fs,
                store,
                terminal,
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
        let frame = render_frame(
            TerminalDimsInput::new(terminal),
            EditedBuffersInput::new(edits),
            StoreLoadedInput::new(store),
            TabsActiveInput::new(tabs),
            AlertsInput::new(alerts),
            BrowserUiInput::new(browser),
        );

        // ── Execute ─────────────────────────────────────────────
        drivers.file.execute(load_actions.iter(), store);

        // Sync-clear pending_saves for the paths we're about to
        // dispatch — the execute-pattern discipline that prevents
        // the next tick's query from re-emitting the same saves.
        for action in &save_actions {
            let led_driver_buffers_core::SaveAction::Save { path, .. } = action;
            edits.pending_saves.remove(path);
        }
        drivers.file_write.execute(save_actions.iter());

        drivers.fs_list.execute(list_actions.iter());

        // Clipboard actions: a Read when a yank is pending (no read
        // already in flight), a Write when a kill queued clipboard
        // text. Both flags cleared synchronously per the execute
        // pattern.
        let mut clip_actions: Vec<ClipboardAction> = Vec::new();
        if kill_ring.pending_yank.is_some() && !kill_ring.read_in_flight {
            kill_ring.read_in_flight = true;
            clip_actions.push(ClipboardAction::Read);
        }
        if let Some(text) = kill_ring.pending_clipboard_write.take() {
            clip_actions.push(ClipboardAction::Write(text));
        }
        drivers.clipboard.execute(clip_actions.iter());

        // ── Render ──────────────────────────────────────────────
        if frame != last_frame {
            if let Some(f) = &frame {
                trace.render_tick();
                paint(f, stdout)?;
            }
            last_frame = frame;
        }

        // ── Block until something happens ───────────────────────
        // Drain any stale wake signals that arrived while we were
        // working — they're already accounted for by the drains
        // above. Then block on `recv_timeout` with a deadline equal
        // to the nearest pending timer (only the info-alert expiry
        // today). Drivers notify on every completion, terminal
        // input notifies on every key; we wake instantly on either
        // and sleep the rest of the time.
        while wake.rx.try_recv().is_ok() {}
        let timeout = alerts
            .info_expires_at
            .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(60));
        use std::sync::mpsc::RecvTimeoutError;
        match wake.rx.recv_timeout(timeout) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        }
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
    let (input, input_native) =
        led_driver_terminal_native::spawn(trace.as_terminal_trace(), wake.notifier.clone())?;
    Ok(Drivers {
        file,
        file_write,
        input,
        clipboard,
        fs_list,
        _file_native: file_native,
        _file_write_native: file_write_native,
        _input_native: input_native,
        _clipboard_native: clipboard_native,
        _fs_list_native: fs_list_native,
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
}
