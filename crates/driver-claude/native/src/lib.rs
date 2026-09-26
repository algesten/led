//! Desktop-native async half of the Claude driver.
//!
//! Manager thread + per-subprocess thread groups, all
//! std::thread + std::sync::mpsc — no tokio per
//! [[feedback_no_tokio_for_drivers]].
//!
//! The public entry point is [`spawn`], which returns the sync
//! [`ClaudeDriver`] (held by the runtime) plus a [`ClaudeNative`]
//! lifecycle marker. Dropping the driver closes its `tx_cmd`,
//! which causes the manager thread to exit cleanly.

use std::sync::Arc;
use std::sync::mpsc;

use led_core::Notifier;
use led_driver_claude_core::{ClaudeCmd, ClaudeDriver, ClaudeEvent, Trace};

mod command;
mod manager;
mod subprocess;

pub use command::{build_argv, encode_user_message};

/// Lifecycle marker for the manager + spawned worker threads.
/// Kept alive by the runtime; on drop, the manager loop notices
/// its channel is gone and exits, which in turn drops every
/// subprocess slot and lets the waiters fire their final
/// Exited events.
pub struct ClaudeNative {
    _marker: (),
}

/// Resolve the `claude` binary. Defaults to `"claude"` (looked
/// up against `PATH`). Overridable via the `LED_CLAUDE_BIN`
/// env var — useful for tests that need a stub, and for users
/// who keep multiple `claude` builds installed.
pub fn claude_bin() -> String {
    std::env::var("LED_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string())
}

/// Wire up the Claude driver:
///
/// 1. Allocate `tx_cmd` / `rx_cmd` and `tx_event` / `rx_event`
///    mpsc pairs.
/// 2. Spawn the manager thread.
/// 3. Construct a [`ClaudeDriver`] over the cmd sender + event
///    receiver and the supplied trace.
///
/// `notify` is the runtime's wake handle — every event the
/// manager (or its worker threads) posts to `tx_event` is
/// followed by `notify.notify()` so the main loop unblocks
/// immediately.
pub fn spawn(trace: Arc<dyn Trace>, notify: Notifier) -> (ClaudeDriver, ClaudeNative) {
    spawn_with_bin(claude_bin(), trace, notify)
}

/// Explicit-bin variant — primarily for tests. Production code
/// calls [`spawn`] which reads `LED_CLAUDE_BIN`.
pub fn spawn_with_bin(
    bin: String,
    trace: Arc<dyn Trace>,
    notify: Notifier,
) -> (ClaudeDriver, ClaudeNative) {
    let (tx_cmd, rx_cmd) = mpsc::channel::<ClaudeCmd>();
    let (tx_event, rx_event) = mpsc::channel::<ClaudeEvent>();

    manager::spawn_manager(bin, rx_cmd, tx_event, notify);

    let driver = ClaudeDriver::new(tx_cmd, rx_event, trace);
    let native = ClaudeNative { _marker: () };
    (driver, native)
}
