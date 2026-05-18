//! Per-subprocess thread group.
//!
//! Each `Spawn` cmd creates four cooperating std::threads:
//!
//! - **writer**: drains `rx_stdin` and pushes NDJSON bytes onto
//!   the child's stdin. Closing the receiver (sender dropped) is
//!   the graceful-shutdown signal — the writer falls out of its
//!   `recv` loop and the stdin pipe drops, which the CLI reads
//!   as EOF and exits cleanly once the current turn finishes.
//! - **reader**: BufReader::lines on stdout → parse_line → emits
//!   [`ClaudeEvent::Parsed`] (or drops the line silently if
//!   parsing returned None — already logged via stderr).
//! - **stderr**: BufReader::lines on stderr → emits
//!   [`ClaudeEvent::Stderr`] so the user sees CLI warnings /
//!   errors in the trace.
//! - **waiter**: blocks on `child.wait()` and emits
//!   [`ClaudeEvent::Exited`] when the process is gone.
//!
//! All four are independent and may terminate in any order; the
//! waiter is the canonical "session is gone" signal the runtime
//! folds into [`ChatLifecycle::Exited`].

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use led_core::{Notifier, SessionUuid};
use led_driver_claude_core::{ClaudeEvent, ExitInfo, parse_line};

/// Handles the manager keeps for each live subprocess. Sending
/// on `tx_stdin` posts a single NDJSON line to the child;
/// dropping the slot drops `tx_stdin` (graceful shutdown) and
/// the waiter eventually fires Exited.
pub struct SubprocessSlot {
    pub tx_stdin: Sender<String>,
    /// Held so the manager can `kill()` for hard Cancel. Wrapped
    /// in Mutex because the waiter thread also calls `wait()`.
    pub child: Arc<Mutex<Child>>,
}

/// Wire up the four threads around an already-spawned `Child`.
pub fn wire_subprocess(
    uuid: SessionUuid,
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
    tx_event: Sender<ClaudeEvent>,
    notify: Notifier,
) -> SubprocessSlot {
    let child = Arc::new(Mutex::new(child));
    let (tx_stdin, rx_stdin) = mpsc::channel::<String>();

    spawn_writer(&uuid, rx_stdin, stdin);
    spawn_reader(&uuid, stdout, tx_event.clone(), notify.clone());
    spawn_stderr_reader(&uuid, stderr, tx_event.clone(), notify.clone());
    spawn_waiter(&uuid, child.clone(), tx_event, notify);

    SubprocessSlot { tx_stdin, child }
}

fn spawn_writer(uuid: &SessionUuid, rx_stdin: Receiver<String>, mut stdin: ChildStdin) {
    let name = format!("led-claude-{}-write", uuid.as_str());
    thread::Builder::new()
        .name(name)
        .spawn(move || {
            while let Ok(line) = rx_stdin.recv() {
                if stdin.write_all(line.as_bytes()).is_err() {
                    return;
                }
                if stdin.flush().is_err() {
                    return;
                }
            }
            // Receiver hung up — fall out so stdin drops,
            // signalling EOF to the child.
        })
        .expect("spawning Claude writer thread");
}

fn spawn_reader(
    uuid: &SessionUuid,
    stdout: ChildStdout,
    tx_event: Sender<ClaudeEvent>,
    notify: Notifier,
) {
    let uuid = uuid.clone();
    let name = format!("led-claude-{}-read", uuid.as_str());
    thread::Builder::new()
        .name(name)
        .spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else {
                    return;
                };
                if let Some(parsed) = parse_line(&line) {
                    if tx_event
                        .send(ClaudeEvent::Parsed {
                            uuid: uuid.clone(),
                            parsed,
                        })
                        .is_err()
                    {
                        return;
                    }
                    notify.notify();
                }
                // Unparsed lines are silently dropped — they'd
                // show up on the trace via stderr anyway, and
                // the CLI emits well-formed JSON for events we
                // care about. New event types we don't recognise
                // yet (rate_limit_event was one such case) are
                // handled by parse_line returning None.
            }
        })
        .expect("spawning Claude reader thread");
}

fn spawn_stderr_reader(
    uuid: &SessionUuid,
    stderr: ChildStderr,
    tx_event: Sender<ClaudeEvent>,
    notify: Notifier,
) {
    let uuid = uuid.clone();
    let name = format!("led-claude-{}-stderr", uuid.as_str());
    thread::Builder::new()
        .name(name)
        .spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                let Ok(line) = line else {
                    return;
                };
                if tx_event
                    .send(ClaudeEvent::Stderr {
                        uuid: uuid.clone(),
                        line,
                    })
                    .is_err()
                {
                    return;
                }
                notify.notify();
            }
        })
        .expect("spawning Claude stderr thread");
}

fn spawn_waiter(
    uuid: &SessionUuid,
    child: Arc<Mutex<Child>>,
    tx_event: Sender<ClaudeEvent>,
    notify: Notifier,
) {
    let uuid = uuid.clone();
    let name = format!("led-claude-{}-wait", uuid.as_str());
    thread::Builder::new()
        .name(name)
        .spawn(move || {
            // `Child::wait()` consumes the child handle on
            // success; using Mutex<Child> + try_wait would also
            // work but blocking wait is what we want — the
            // thread sits idle until the process ends.
            let exit = match child.lock() {
                Ok(mut guard) => guard.wait(),
                Err(_) => return,
            };
            let info = match exit {
                Ok(status) => ExitInfo {
                    code: status.code(),
                    signal: signal_of(&status),
                },
                Err(_) => ExitInfo::default(),
            };
            let _ = tx_event.send(ClaudeEvent::Exited { uuid, exit: info });
            notify.notify();
        })
        .expect("spawning Claude waiter thread");
}

#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn signal_of(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// Send SIGINT to the child (Unix). On non-Unix platforms,
/// falls back to `Child::kill()` (SIGKILL semantics).
///
/// Used by Cancel cmd handling — SIGINT gives the CLI a chance
/// to flush trace + exit cleanly; if it doesn't respond the
/// caller can fall back to `kill_now`.
pub fn signal_interrupt(child: &Arc<Mutex<Child>>) {
    #[cfg(unix)]
    {
        if let Ok(guard) = child.lock() {
            let pid = guard.id() as i32;
            // Safety: pid is owned by us until wait() completes.
            // SIGINT (2) is the polite interrupt; CLI will write
            // a result/error event on its own and exit.
            unsafe {
                libc::kill(pid, libc::SIGINT);
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Ok(mut guard) = child.lock() {
            let _ = guard.kill();
        }
    }
}
