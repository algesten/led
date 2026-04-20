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

pub mod dispatch;
pub mod query;
pub mod trace;

use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use led_driver_buffers_core::{BufferStore, FileReadDriver};
use led_driver_buffers_native::FileReadNative;
use led_driver_terminal_core::{Dims, Frame, KeyEvent, TermEvent, Terminal, TerminalInputDriver};
use led_driver_terminal_native::{paint, TerminalInputNative};
use led_state_buffer_edits::{BufferEdits, EditedBuffer};
use led_state_tabs::{TabId, Tabs};

pub use dispatch::{dispatch, dispatch_key, DispatchOutcome};
pub use query::{
    body_model, file_load_action, render_frame, tab_bar_model, EditedBuffersInput,
    StoreLoadedInput, TabsActiveInput, TabsOpenInput, TerminalDimsInput,
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
    pub input: TerminalInputDriver,

    // Held only for lifetime management; detached on drop.
    _file_native: FileReadNative,
    _input_native: TerminalInputNative,
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

/// Run the main loop until dispatch signals quit.
pub fn run(
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    store: &mut BufferStore,
    terminal: &mut Terminal,
    drivers: &Drivers,
    stdout: &mut impl Write,
    trace: &SharedTrace,
) -> io::Result<()> {
    let mut last_frame: Option<Frame> = None;

    loop {
        // ── Ingest ──────────────────────────────────────────────
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
            match dispatch(ev, tabs, edits, store, terminal) {
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
        let actions = file_load_action(
            StoreLoadedInput::new(store),
            TabsOpenInput::new(tabs),
        );
        let frame = render_frame(
            TerminalDimsInput::new(terminal),
            EditedBuffersInput::new(edits),
            StoreLoadedInput::new(store),
            TabsActiveInput::new(tabs),
        );

        // ── Execute ─────────────────────────────────────────────
        drivers.file.execute(actions.iter(), store);

        // ── Render ──────────────────────────────────────────────
        if frame != last_frame {
            if let Some(f) = &frame {
                trace.render_tick();
                paint(f, stdout)?;
            }
            last_frame = frame;
        }

        // Short sleep; a proper cross-channel select is a later
        // refinement (would require crossbeam or a Condvar fan-in).
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Convenience constructor: spawns both drivers with a shared trace
/// using the desktop `*-native` implementations.
pub fn spawn_drivers(trace: SharedTrace) -> io::Result<Drivers> {
    let (file, file_native) = led_driver_buffers_native::spawn(trace.clone().as_file_trace());
    let (input, input_native) = led_driver_terminal_native::spawn(trace.as_terminal_trace())?;
    Ok(Drivers {
        file,
        input,
        _file_native: file_native,
        _input_native: input_native,
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

    impl led_driver_buffers_core::Trace for FileTraceAdapter {
        fn file_load_start(&self, path: &CanonPath) {
            self.0.file_load_start(path);
        }
        fn file_load_done(&self, path: &CanonPath, result: &Result<Arc<Rope>, String>) {
            self.0.file_load_done(path, result);
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
}

impl SharedTrace {
    pub(crate) fn as_file_trace(&self) -> Arc<dyn led_driver_buffers_core::Trace> {
        Arc::new(trace_adapter::FileTraceAdapter(self.inner()))
    }
    pub(crate) fn as_terminal_trace(&self) -> Arc<dyn led_driver_terminal_core::Trace> {
        Arc::new(trace_adapter::TermTraceAdapter(self.inner()))
    }
}
