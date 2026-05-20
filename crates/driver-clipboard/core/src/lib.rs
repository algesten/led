//! Sync core of the clipboard driver.
//!
//! Per EXAMPLE-ARCH § "Stateless drivers still need an in-flight
//! source", this driver owns a [`ClipboardState`] source tracking
//! in-flight Read/Write operations and the last error. The
//! synchronous `execute` writes intent into the source before
//! firing the async command; `process` clears the in-flight flag
//! when the worker's `Done` arrives.
//!
//! The user-decision crate `state-clipboard` carries the
//! `pending_yank` / `pending_write` user intents. A memo combines
//! both sources to decide which action to fire each tick — the
//! driver's `read`/`write` fields gate against double-firing while
//! an op is in flight.
//!
//! Knows nothing about other drivers or state crates. Cross-driver
//! composition (yank's "read clipboard, fall back to kill ring")
//! lives in the runtime.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

// ── ABI types ─────────────────────────────────────────────────────────

/// Action produced by the runtime's clipboard query, consumed by
/// [`ClipboardDriver::execute`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardAction {
    Read,
    Write(Arc<str>),
}

/// Command from the sync driver to the async worker.
#[derive(Clone, Debug)]
pub enum ClipboardCmd {
    Read,
    Write(Arc<str>),
}

/// Completion posted by the async worker.
#[derive(Debug)]
pub struct ClipboardDone {
    pub result: Result<ClipboardResult, String>,
}

#[derive(Debug, Clone)]
pub enum ClipboardResult {
    /// Read completed. `None` ⇒ the system clipboard was empty or
    /// held non-text content.
    Text(Option<Arc<str>>),
    /// Write completed successfully.
    Written,
}

// ── Driver-owned source ───────────────────────────────────────────────

/// In-flight state of a single Read or Write operation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, drv::Input)]
pub enum ReadState {
    #[default]
    Idle,
    InFlight,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, drv::Input)]
pub enum WriteState {
    #[default]
    Idle,
    InFlight,
}

/// Driver-owned source. Written by `execute` (sync intent) and
/// `process` (Done acknowledgement). Memos read it to gate
/// "should we fire another Read/Write?" against in-flight ops;
/// `last_error` records the most recent failure so the runtime
/// can surface a degraded mode without re-firing the same op.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClipboardState {
    pub read: ReadState,
    pub write: WriteState,
    pub last_error: Option<Arc<str>>,
}

// ── Trace ─────────────────────────────────────────────────────────────

pub trait Trace: Send + Sync {
    fn clipboard_read_start(&self);
    fn clipboard_read_done(&self, ok: bool, empty: bool);
    /// Outbound clipboard write. `text` is the full payload — the
    /// implementation is expected to format a short `preview="…"`
    /// from its first ~14 chars (legacy parity) without retaining
    /// the buffer beyond the trace line.
    fn clipboard_write_start(&self, text: &str);
    fn clipboard_write_done(&self, ok: bool);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn clipboard_read_start(&self) {}
    fn clipboard_read_done(&self, _ok: bool, _empty: bool) {}
    fn clipboard_write_start(&self, _text: &str) {}
    fn clipboard_write_done(&self, _ok: bool) {}
}

// ── Sync driver API ───────────────────────────────────────────────────

pub struct ClipboardDriver {
    tx_cmd: Sender<ClipboardCmd>,
    rx_done: Receiver<ClipboardDone>,
    trace: Arc<dyn Trace>,
}

impl ClipboardDriver {
    pub fn new(
        tx_cmd: Sender<ClipboardCmd>,
        rx_done: Receiver<ClipboardDone>,
        trace: Arc<dyn Trace>,
    ) -> Self {
        Self {
            tx_cmd,
            rx_done,
            trace,
        }
    }

    /// Drain completions into the driver source and return the
    /// raw `Done` payloads so the caller (ingest) can apply
    /// downstream effects (paste-into-buffer, kill-ring fallback).
    pub fn process(&self, state: &mut ClipboardState) -> Vec<ClipboardDone> {
        let mut out: Vec<ClipboardDone> = Vec::new();
        while let Ok(done) = self.rx_done.try_recv() {
            match &done.result {
                Ok(ClipboardResult::Text(t)) => {
                    state.read = ReadState::Idle;
                    self.trace
                        .clipboard_read_done(true, t.is_none());
                }
                Ok(ClipboardResult::Written) => {
                    state.write = WriteState::Idle;
                    self.trace.clipboard_write_done(true);
                }
                Err(e) => {
                    // Either op may have errored — clear both
                    // in-flight flags and record the message.
                    // Trace fidelity matches legacy (no explicit
                    // per-op failure trace).
                    state.read = ReadState::Idle;
                    state.write = WriteState::Idle;
                    state.last_error = Some(Arc::from(e.as_str()));
                }
            }
            out.push(done);
        }
        out
    }

    /// Forward actions to the worker, writing intent into the
    /// source first so the same tick's downstream query sees the
    /// op as in-flight.
    pub fn execute<'a, I>(&self, actions: I, state: &mut ClipboardState)
    where
        I: IntoIterator<Item = &'a ClipboardAction>,
    {
        for action in actions {
            match action {
                ClipboardAction::Read => {
                    if matches!(state.read, ReadState::InFlight) {
                        continue;
                    }
                    state.read = ReadState::InFlight;
                    self.trace.clipboard_read_start();
                    let _ = self.tx_cmd.send(ClipboardCmd::Read);
                }
                ClipboardAction::Write(text) => {
                    if matches!(state.write, WriteState::InFlight) {
                        continue;
                    }
                    state.write = WriteState::InFlight;
                    self.trace.clipboard_write_start(text);
                    let _ = self.tx_cmd.send(ClipboardCmd::Write(text.clone()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn execute_writes_in_flight_and_forwards_read() {
        let (tx_cmd, rx_cmd) = mpsc::channel::<ClipboardCmd>();
        let (_tx_done, rx_done) = mpsc::channel::<ClipboardDone>();
        let driver = ClipboardDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));
        let mut state = ClipboardState::default();

        driver.execute([&ClipboardAction::Read], &mut state);

        assert_eq!(state.read, ReadState::InFlight);
        assert!(matches!(rx_cmd.try_recv().unwrap(), ClipboardCmd::Read));
    }

    #[test]
    fn execute_writes_in_flight_and_forwards_write() {
        let (tx_cmd, rx_cmd) = mpsc::channel::<ClipboardCmd>();
        let (_tx_done, rx_done) = mpsc::channel::<ClipboardDone>();
        let driver = ClipboardDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));
        let mut state = ClipboardState::default();

        let text: Arc<str> = Arc::from("hello");
        driver.execute([&ClipboardAction::Write(text.clone())], &mut state);

        assert_eq!(state.write, WriteState::InFlight);
        match rx_cmd.try_recv().unwrap() {
            ClipboardCmd::Write(got) => assert_eq!(&*got, "hello"),
            other => panic!("expected Write, got {other:?}"),
        }
    }

    #[test]
    fn execute_skips_when_already_in_flight() {
        let (tx_cmd, rx_cmd) = mpsc::channel::<ClipboardCmd>();
        let (_tx_done, rx_done) = mpsc::channel::<ClipboardDone>();
        let driver = ClipboardDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));
        let mut state = ClipboardState::default();

        driver.execute([&ClipboardAction::Read], &mut state);
        driver.execute([&ClipboardAction::Read], &mut state);

        assert!(matches!(rx_cmd.try_recv().unwrap(), ClipboardCmd::Read));
        assert!(rx_cmd.try_recv().is_err());
    }

    #[test]
    fn process_clears_in_flight_on_text_done() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<ClipboardCmd>();
        let (tx_done, rx_done) = mpsc::channel::<ClipboardDone>();
        let driver = ClipboardDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));
        let mut state = ClipboardState {
            read: ReadState::InFlight,
            ..ClipboardState::default()
        };

        tx_done
            .send(ClipboardDone {
                result: Ok(ClipboardResult::Text(Some(Arc::from("payload")))),
            })
            .unwrap();

        let dones = driver.process(&mut state);
        assert_eq!(state.read, ReadState::Idle);
        assert_eq!(dones.len(), 1);
    }

    #[test]
    fn process_clears_in_flight_on_written() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<ClipboardCmd>();
        let (tx_done, rx_done) = mpsc::channel::<ClipboardDone>();
        let driver = ClipboardDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));
        let mut state = ClipboardState {
            write: WriteState::InFlight,
            ..ClipboardState::default()
        };

        tx_done
            .send(ClipboardDone {
                result: Ok(ClipboardResult::Written),
            })
            .unwrap();

        let dones = driver.process(&mut state);
        assert_eq!(state.write, WriteState::Idle);
        assert_eq!(dones.len(), 1);
    }

    #[test]
    fn process_records_error_on_err_done() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<ClipboardCmd>();
        let (tx_done, rx_done) = mpsc::channel::<ClipboardDone>();
        let driver = ClipboardDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));
        let mut state = ClipboardState {
            read: ReadState::InFlight,
            ..ClipboardState::default()
        };

        tx_done
            .send(ClipboardDone {
                result: Err("clipboard unavailable".into()),
            })
            .unwrap();

        let dones = driver.process(&mut state);
        assert_eq!(state.read, ReadState::Idle);
        assert_eq!(
            state.last_error.as_deref(),
            Some("clipboard unavailable")
        );
        assert!(matches!(&dones[0].result, Err(msg) if msg == "clipboard unavailable"));
    }
}
