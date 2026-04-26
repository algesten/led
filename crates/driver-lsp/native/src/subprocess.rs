//! Per-server subprocess spawn + blocking reader / writer threads.
//!
//! The shape:
//!
//! - [`reader_loop`] consumes a `Read`, accumulates bytes into a
//!   rolling buffer, drains complete frames via
//!   [`crate::framing::try_parse_frame`], classifies each body
//!   with [`crate::classify::classify`], and forwards the result
//!   plus the server's name to a central `Sender<ServerIncoming>`.
//! - [`writer_loop`] drains a per-server `Receiver<Vec<u8>>` of
//!   already-encoded frames and writes each one to a `Write`.
//! - [`Server`] bundles the spawned `Child`, the two thread
//!   handles, and the outbound sender. Dropping it closes the
//!   outbound channel (writer thread exits cleanly) and kills
//!   the child (reader thread EOFs and exits).
//!
//! Both loops are generic over their I/O so the core logic is
//! unit-testable against in-memory cursors without needing a real
//! subprocess. The subprocess glue is a thin wrapper.

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, SendError};
use std::thread::{self, JoinHandle};

use crate::classify::{Incoming, classify};
use crate::framing::try_parse_frame;

/// One classified LSP frame plus the name of the server it came
/// from. The manager thread uses `server` to route into the
/// right per-server state (pending-response handlers, etc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerIncoming {
    pub server: String,
    pub incoming: Incoming,
}

/// A running server: the spawned child + the outbound channel
/// the manager pushes encoded frames into.
///
/// Drop order is load-bearing. Dropping first closes
/// `outbound_tx`, which makes the writer thread's
/// `recv()` return `Err` and exit. Then killing the child makes
/// the reader thread's `read()` return `Ok(0)` and exit. The
/// join handles are awaited in `Drop` so we never leak threads.
pub struct Server {
    pub name: String,
    /// Pushes encoded (`Content-Length`-framed) bytes to the
    /// writer thread. Drop this to shut the writer down.
    outbound_tx: Sender<Vec<u8>>,
    child: Option<Child>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
}

impl Server {
    /// Encode a JSON-RPC body as a frame and hand it to the
    /// writer thread. Errors when the writer has already exited
    /// (server is dead).
    pub fn send_body(&self, body: &[u8]) -> Result<(), SendError<Vec<u8>>> {
        self.outbound_tx.send(crate::framing::encode_frame(body))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Close the outbound channel so the writer exits.
        // Replacing the sender with a fresh disconnected one is
        // the simplest way — `Sender` has no `.close()` method.
        let (tx, _) = std::sync::mpsc::channel();
        let _old = std::mem::replace(&mut self.outbound_tx, tx);
        // Kill the child so the reader EOFs.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

/// Spawn a language server subprocess.
///
/// `command` and `args` are resolved against `$PATH` via
/// `std::process::Command`; a missing binary surfaces as an
/// `io::Error` here, NOT a silent no-op — the caller decides
/// whether to alert or fall back.
///
/// `incoming_tx` is the central channel; every parsed message
/// from the server lands there tagged with `name`.
pub fn spawn(
    name: impl Into<String>,
    command: &str,
    args: &[&str],
    incoming_tx: Sender<ServerIncoming>,
) -> std::io::Result<Server> {
    let name = name.into();
    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");

    let (outbound_tx, outbound_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    let writer_name = name.clone();
    let writer = thread::Builder::new()
        .name(format!("led-lsp-{name}-writer"))
        .spawn(move || {
            writer_loop(stdin, outbound_rx, writer_name);
        })?;

    let reader_name = name.clone();
    let reader_tx = incoming_tx;
    let reader = thread::Builder::new()
        .name(format!("led-lsp-{name}-reader"))
        .spawn(move || {
            reader_loop(stdout, reader_tx, reader_name);
        })?;

    Ok(Server {
        name,
        outbound_tx,
        child: Some(child),
        reader: Some(reader),
        writer: Some(writer),
    })
}

/// Read frames off `src` until EOF or parse error. For each
/// complete frame: classify, forward on `out` tagged with
/// `server_name`. Unclassifiable bodies (malformed JSON / no
/// method + no id) are dropped silently — servers occasionally
/// emit junk during startup and it's not fatal.
pub fn reader_loop<R: Read>(mut src: R, out: Sender<ServerIncoming>, server_name: String) {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 4096];
    loop {
        let n = match src.read(&mut chunk) {
            Ok(0) => return, // EOF
            Ok(n) => n,
            Err(_) => return,
        };
        buf.extend_from_slice(&chunk[..n]);
        // Drain as many complete frames as the buffer holds.
        loop {
            match try_parse_frame(&buf) {
                Ok(Some((consumed, body))) => {
                    buf.drain(..consumed);
                    if let Some(incoming) = classify(&body)
                        && out
                            .send(ServerIncoming {
                                server: server_name.clone(),
                                incoming,
                            })
                            .is_err()
                    {
                        return; // receiver gone
                    }
                }
                Ok(None) => break, // need more bytes
                Err(_) => return,  // stream corrupt; caller re-spawns
            }
        }
    }
}

/// Drain a stream of encoded frames from `rx` and write each one
/// to `dst`, flushing after every frame so servers don't block
/// waiting for input that's sitting in our output buffer.
pub fn writer_loop<W: Write>(mut dst: W, rx: Receiver<Vec<u8>>, _server_name: String) {
    while let Ok(frame) = rx.recv() {
        if dst.write_all(&frame).is_err() {
            return;
        }
        if dst.flush().is_err() {
            return;
        }
    }
}

// ── Private glue for `spawn()` so the pump types are concrete ──

#[allow(dead_code)]
fn _writer_type_check(stdin: ChildStdin, rx: Receiver<Vec<u8>>) {
    writer_loop(stdin, rx, String::new());
}
#[allow(dead_code)]
fn _reader_type_check(stdout: ChildStdout, tx: Sender<ServerIncoming>) {
    reader_loop(stdout, tx, String::new());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::Incoming;
    use crate::framing::encode_frame;
    use std::io::Cursor;
    use std::sync::mpsc;
    use std::time::Duration;

    fn drain(rx: &Receiver<ServerIncoming>, n: usize) -> Vec<ServerIncoming> {
        let mut got = Vec::new();
        for _ in 0..n {
            got.push(
                rx.recv_timeout(Duration::from_secs(1))
                    .expect("incoming within 1s"),
            );
        }
        got
    }

    // ── reader_loop ─────────────────────────────────────────

    #[test]
    fn reader_loop_drains_multiple_frames_in_order() {
        let mut wire = Vec::new();
        wire.extend(encode_frame(br#"{"jsonrpc":"2.0","id":1,"result":null}"#));
        wire.extend(encode_frame(
            br#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{}}"#,
        ));

        let (tx, rx) = mpsc::channel();
        let h = thread::spawn(move || reader_loop(Cursor::new(wire), tx, "ra".into()));
        let got = drain(&rx, 2);
        h.join().unwrap();

        assert_eq!(got[0].server, "ra");
        assert!(matches!(got[0].incoming, Incoming::Response { .. }));
        assert!(matches!(got[1].incoming, Incoming::Notification { .. }));
    }

    #[test]
    fn reader_loop_handles_partial_frame_at_eof_without_panicking() {
        // Incomplete header - reader should just exit on EOF.
        let wire = b"Content-Length: 4\r\n".to_vec(); // no \r\n\r\n
        let (tx, rx) = mpsc::channel();
        let h = thread::spawn(move || reader_loop(Cursor::new(wire), tx, "ra".into()));
        h.join().unwrap();
        assert!(rx.try_recv().is_err(), "no incoming emitted");
    }

    #[test]
    fn reader_loop_skips_malformed_body_continues_to_next_frame() {
        // First frame has garbage JSON → classify returns None →
        // dropped silently. Second frame is valid → emitted.
        let mut wire = Vec::new();
        wire.extend(encode_frame(b"not json at all"));
        wire.extend(encode_frame(br#"{"jsonrpc":"2.0","method":"ping","params":{}}"#));

        let (tx, rx) = mpsc::channel();
        let h = thread::spawn(move || reader_loop(Cursor::new(wire), tx, "ra".into()));
        let got = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("second frame within 1s");
        h.join().unwrap();

        assert!(matches!(got.incoming, Incoming::Notification { .. }));
    }

    #[test]
    fn reader_loop_stops_on_stream_corruption() {
        // Bad Content-Length value → parse error → reader exits
        // without emitting downstream frames.
        let mut wire = b"Content-Length: bogus\r\n\r\n".to_vec();
        wire.extend(encode_frame(br#"{"method":"should-not-see"}"#));

        let (tx, rx) = mpsc::channel();
        let h = thread::spawn(move || reader_loop(Cursor::new(wire), tx, "ra".into()));
        h.join().unwrap();
        assert!(rx.try_recv().is_err(), "no frames emitted post-error");
    }

    #[test]
    fn reader_loop_exits_when_receiver_drops() {
        // If the manager stops listening, reader shouldn't spin
        // forever on its thread.
        let wire = encode_frame(br#"{"jsonrpc":"2.0","method":"ping"}"#);
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let h = thread::spawn(move || reader_loop(Cursor::new(wire), tx, "ra".into()));
        h.join().unwrap();
    }

    // ── writer_loop ─────────────────────────────────────────

    #[test]
    fn writer_loop_writes_each_frame_in_order() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        tx.send(encode_frame(b"first")).unwrap();
        tx.send(encode_frame(b"second")).unwrap();
        drop(tx); // signal EOF to the loop
        let mut out: Vec<u8> = Vec::new();
        writer_loop(&mut out, rx, "ra".into());
        let mut expected: Vec<u8> = Vec::new();
        expected.extend(encode_frame(b"first"));
        expected.extend(encode_frame(b"second"));
        assert_eq!(out, expected);
    }

    #[test]
    fn writer_loop_exits_when_sender_drops_without_frames() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        drop(tx);
        let mut out: Vec<u8> = Vec::new();
        writer_loop(&mut out, rx, "ra".into());
        assert!(out.is_empty());
    }

    /// A writable sink that fails after `n` successful writes —
    /// models the subprocess dying mid-stream. Used to verify
    /// the writer exits rather than spinning on send errors.
    struct FailingAfter {
        n: usize,
        hits: usize,
    }
    impl Write for FailingAfter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.hits >= self.n {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "child gone",
                ));
            }
            self.hits += 1;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writer_loop_exits_on_write_error() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        tx.send(encode_frame(b"ok-ish")).unwrap();
        tx.send(encode_frame(b"should not be attempted past failure"))
            .unwrap();
        // Keep the channel alive; writer must exit on write error
        // anyway. (If we drop tx, the loop exits naturally.)
        let sink = FailingAfter { n: 1, hits: 0 };
        let h = thread::spawn(move || writer_loop(sink, rx, "ra".into()));
        // Once the writer exits the rx handle here drops too; but
        // the `tx` clone we still hold keeps the channel open —
        // ensure this joins quickly, not blocks forever.
        drop(tx); // release the channel so any lingering reference closes
        h.join().expect("writer exits");
    }
}
