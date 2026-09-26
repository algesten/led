//! Manager thread.
//!
//! Single thread owns the `HashMap<SessionUuid, SubprocessSlot>`
//! and drains [`ClaudeCmd`]s from the sync driver. Per cmd:
//!
//! - `Spawn`: launch a new `claude -p` process + wire its four
//!   helper threads. Idempotent — re-spawning a uuid that already
//!   has a live slot is a no-op (the runtime's
//!   `subprocess_action` memo treats Spawning/Running as in-flight
//!   so this should never fire, but the dedupe is here for safety
//!   per [[feedback_materialization]]).
//! - `UserMessage`: forward to the slot's writer.
//! - `Cancel`: SIGINT the child (graceful). The waiter thread
//!   posts `Exited` when the process actually dies; the slot
//!   removes itself only after that.
//! - `Shutdown`: drop the writer's Sender → stdin closes →
//!   child exits naturally → waiter fires Exited.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};
use std::thread;

use led_core::{Notifier, SessionUuid};
use led_driver_claude_core::{ClaudeCmd, ClaudeEvent, ExitInfo};

use crate::command::{build_argv, encode_user_message};
use crate::subprocess::{SubprocessSlot, signal_interrupt, wire_subprocess};

/// Spawn the manager thread. Returns immediately — the manager
/// loop runs until the `rx_cmd` sender is dropped (i.e. the
/// runtime is shutting down).
pub fn spawn_manager(
    bin: String,
    rx_cmd: Receiver<ClaudeCmd>,
    tx_event: Sender<ClaudeEvent>,
    notify: Notifier,
) {
    thread::Builder::new()
        .name("led-claude-manager".into())
        .spawn(move || manager_loop(bin, rx_cmd, tx_event, notify))
        .expect("spawning Claude manager thread");
}

fn manager_loop(
    bin: String,
    rx_cmd: Receiver<ClaudeCmd>,
    tx_event: Sender<ClaudeEvent>,
    notify: Notifier,
) {
    let mut slots: HashMap<SessionUuid, SubprocessSlot> = HashMap::new();

    while let Ok(cmd) = rx_cmd.recv() {
        match cmd {
            ClaudeCmd::Spawn {
                uuid,
                mode,
                effort,
                permission_mode,
            } => {
                if slots.contains_key(&uuid) {
                    continue;
                }
                let argv = build_argv(&bin, &uuid, mode, effort, permission_mode);
                let mut child = match Command::new(&argv[0])
                    .args(&argv[1..])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                {
                    Ok(c) => c,
                    Err(e) => {
                        // Spawn failed (binary missing, perms,
                        // etc.). Emit a synthetic Exited so the
                        // lifecycle source records the failure
                        // and the action memo doesn't loop on
                        // respawning per
                        // [[feedback_driver_failure_state]].
                        let _ = tx_event.send(ClaudeEvent::Stderr {
                            uuid: uuid.clone(),
                            line: format!("spawn failed: {e}"),
                        });
                        let _ = tx_event.send(ClaudeEvent::Exited {
                            uuid: uuid.clone(),
                            exit: ExitInfo {
                                code: Some(-1),
                                signal: None,
                            },
                        });
                        notify.notify();
                        continue;
                    }
                };
                let stdin = child.stdin.take().expect("piped stdin");
                let stdout = child.stdout.take().expect("piped stdout");
                let stderr = child.stderr.take().expect("piped stderr");
                let slot = wire_subprocess(
                    uuid.clone(),
                    child,
                    stdin,
                    stdout,
                    stderr,
                    tx_event.clone(),
                    notify.clone(),
                );
                slots.insert(uuid, slot);
            }
            ClaudeCmd::UserMessage { uuid, text } => {
                if let Some(slot) = slots.get(&uuid) {
                    let _ = slot.tx_stdin.send(encode_user_message(&text));
                }
                // If the slot's gone, the message is dropped —
                // the runtime's subprocess_action memo holds
                // the pending queue; on next iteration it'll
                // see no Running session and either Spawn or
                // hold the message until a Running state.
            }
            ClaudeCmd::Cancel { uuid } => {
                if let Some(slot) = slots.get(&uuid) {
                    signal_interrupt(&slot.child);
                }
                // Don't remove the slot here — the waiter
                // thread will fire Exited and the manager's
                // next iteration of this cmd-handler scope is
                // not where slot lifetime ends. (The slot is
                // dropped on Shutdown or on manager exit.)
            }
            ClaudeCmd::Shutdown { uuid } => {
                // Drop the writer's Sender → stdin pipe drops →
                // child reads EOF on stdin → exits on the
                // current turn boundary → waiter fires Exited.
                slots.remove(&uuid);
            }
        }
    }
    // Channel closed. Drop all slots — every writer's Sender
    // dies, every child reads EOF, every waiter eventually
    // fires Exited (and the receivers may already be gone,
    // which is fine — their sends are best-effort).
    drop(slots);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    use led_driver_claude_core::{Effort, PermissionMode, SpawnMode};

    /// Manager attempts to spawn `/nonexistent/claude` for the
    /// uuid; we expect a Stderr line + a synthetic Exited with
    /// `code: Some(-1)` and no respawn loop. This validates the
    /// spawn-failure path without touching a real CLI.
    #[test]
    fn spawn_failure_emits_stderr_and_exited() {
        let (tx_cmd, rx_cmd) = mpsc::channel::<ClaudeCmd>();
        let (tx_event, rx_event) = mpsc::channel::<ClaudeEvent>();
        spawn_manager(
            "/definitely/not/a/real/path/to/claude".into(),
            rx_cmd,
            tx_event,
            Notifier::noop(),
        );

        let uuid = SessionUuid::new("u1");
        tx_cmd
            .send(ClaudeCmd::Spawn {
                uuid: uuid.clone(),
                mode: SpawnMode::Fresh,
                effort: Effort::XHigh,
                permission_mode: PermissionMode::Auto,
            })
            .unwrap();

        let mut saw_stderr = false;
        let mut saw_exited = false;
        // Collect events for a short window — manager handles
        // the cmd on its own thread.
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline && !(saw_stderr && saw_exited) {
            if let Ok(ev) = rx_event.recv_timeout(Duration::from_millis(100)) {
                match ev {
                    ClaudeEvent::Stderr { .. } => saw_stderr = true,
                    ClaudeEvent::Exited { exit, .. } => {
                        assert_eq!(exit.code, Some(-1));
                        saw_exited = true;
                    }
                    ClaudeEvent::Parsed { .. } => panic!("unexpected Parsed on failed spawn"),
                }
            }
        }
        assert!(saw_stderr, "manager should report spawn failure on stderr channel");
        assert!(saw_exited, "manager should synthesise Exited so the runtime stops retrying");

        drop(tx_cmd);
    }
}
