//! Sync core of the clipboard driver.
//!
//! Stateless — it owns only the mpsc pair used to talk to the async
//! worker. The runtime's `KillRing` carries the `read_in_flight` and
//! `pending_yank` bits.
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

    /// Drain completions. `Vec::new()` on idle — no alloc.
    pub fn process(&self) -> Vec<ClipboardDone> {
        let mut out: Vec<ClipboardDone> = Vec::new();
        while let Ok(done) = self.rx_done.try_recv() {
            match &done.result {
                Ok(ClipboardResult::Text(t)) => {
                    self.trace
                        .clipboard_read_done(true, t.is_none());
                }
                Ok(ClipboardResult::Written) => {
                    self.trace.clipboard_write_done(true);
                }
                Err(_) => {
                    // Caller doesn't distinguish which op failed from
                    // the trace — legacy's trace doesn't either.
                }
            }
            out.push(done);
        }
        out
    }

    /// Forward actions to the worker.
    pub fn execute<'a, I>(&self, actions: I)
    where
        I: IntoIterator<Item = &'a ClipboardAction>,
    {
        for action in actions {
            match action {
                ClipboardAction::Read => {
                    self.trace.clipboard_read_start();
                    let _ = self.tx_cmd.send(ClipboardCmd::Read);
                }
                ClipboardAction::Write(text) => {
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
    fn execute_forwards_read_and_write() {
        let (tx_cmd, rx_cmd) = mpsc::channel::<ClipboardCmd>();
        let (_tx_done, rx_done) = mpsc::channel::<ClipboardDone>();
        let driver = ClipboardDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));

        let text: Arc<str> = Arc::from("hello");
        driver.execute([
            &ClipboardAction::Read,
            &ClipboardAction::Write(text.clone()),
        ]);

        assert!(matches!(rx_cmd.try_recv().unwrap(), ClipboardCmd::Read));
        match rx_cmd.try_recv().unwrap() {
            ClipboardCmd::Write(got) => assert_eq!(&*got, "hello"),
            other => panic!("expected Write, got {other:?}"),
        }
    }

    #[test]
    fn process_surfaces_text_and_empty() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<ClipboardCmd>();
        let (tx_done, rx_done) = mpsc::channel::<ClipboardDone>();
        let driver = ClipboardDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));

        tx_done
            .send(ClipboardDone {
                result: Ok(ClipboardResult::Text(Some(Arc::from("payload")))),
            })
            .unwrap();
        tx_done
            .send(ClipboardDone {
                result: Ok(ClipboardResult::Text(None)),
            })
            .unwrap();

        let dones = driver.process();
        assert_eq!(dones.len(), 2);
        match &dones[0].result {
            Ok(ClipboardResult::Text(Some(t))) => assert_eq!(&**t, "payload"),
            other => panic!("expected Text(Some), got {other:?}"),
        }
        match &dones[1].result {
            Ok(ClipboardResult::Text(None)) => {}
            other => panic!("expected Text(None), got {other:?}"),
        }
    }

    #[test]
    fn process_surfaces_error() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<ClipboardCmd>();
        let (tx_done, rx_done) = mpsc::channel::<ClipboardDone>();
        let driver = ClipboardDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));

        tx_done
            .send(ClipboardDone {
                result: Err("clipboard unavailable".into()),
            })
            .unwrap();
        let dones = driver.process();
        assert!(matches!(&dones[0].result, Err(msg) if msg == "clipboard unavailable"));
    }
}
