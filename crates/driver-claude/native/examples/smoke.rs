//! End-to-end smoke test for the Claude driver against a real
//! `claude` CLI install. Mints a fresh session, sends one user
//! message, prints every event as it arrives, and exits when
//! the turn completes.
//!
//! Run from the workspace root:
//!
//! ```sh
//! cargo run --example smoke -p led-driver-claude-native -- "Reply HI"
//! cargo run --example smoke -p led-driver-claude-native       # uses a default prompt
//! ```
//!
//! Environment variables:
//! - `LED_CLAUDE_BIN` — override the `claude` binary path
//!   (default: `claude` on `PATH`).
//! - `EFFORT` — one of low|medium|high|xhigh|max (default: low,
//!   so the smoke test stays cheap).
//! - `PERMISSION_MODE` — auto|acceptEdits|... (default: auto).
//!
//! Exits 0 on a successful turn, 1 on anything else.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use led_core::{Effort, Notifier, PermissionMode, SessionUuid};
use led_driver_claude_core::{
    ChatLifecycle, ChatTranscripts, ClaudeAction, ExitInfo, SpawnMode, Trace,
};
use led_driver_claude_native::spawn;

/// Trace impl that mirrors lifecycle + error events to stderr so
/// the smoke test can show what's happening behind the scenes.
struct DebugTrace;

impl Trace for DebugTrace {
    fn spawn(
        &self,
        uuid: &SessionUuid,
        mode: SpawnMode,
        effort: Effort,
        perm: PermissionMode,
    ) {
        eprintln!(
            "[trace] spawn uuid={} mode={mode:?} effort={} perm={}",
            uuid.as_str(),
            effort.as_flag(),
            perm.as_flag(),
        );
    }
    fn init(&self, uuid: &SessionUuid, model: &str) {
        eprintln!("[trace] init uuid={} model={model}", uuid.as_str());
    }
    fn user_message(&self, uuid: &SessionUuid, len: usize) {
        eprintln!("[trace] user_message uuid={} len={len}", uuid.as_str());
    }
    fn cancel(&self, uuid: &SessionUuid) {
        eprintln!("[trace] cancel uuid={}", uuid.as_str());
    }
    fn shutdown(&self, uuid: &SessionUuid) {
        eprintln!("[trace] shutdown uuid={}", uuid.as_str());
    }
    fn exited(&self, uuid: &SessionUuid, exit: ExitInfo) {
        eprintln!(
            "[trace] exited uuid={} code={:?} signal={:?}",
            uuid.as_str(),
            exit.code,
            exit.signal,
        );
    }
    fn session_not_found(&self, uuid: &SessionUuid) {
        eprintln!("[trace] session_not_found uuid={}", uuid.as_str());
    }
    fn stderr(&self, uuid: &SessionUuid, line: &str) {
        eprintln!("[trace] stderr uuid={}: {line}", uuid.as_str());
    }
}

const TIMEOUT: Duration = Duration::from_secs(120);

fn main() {
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Reply with the single word HI and nothing else.".to_string());

    let effort = std::env::var("EFFORT")
        .ok()
        .and_then(|s| Effort::from_flag(&s))
        .unwrap_or(Effort::Low);
    let permission_mode = std::env::var("PERMISSION_MODE")
        .ok()
        .and_then(|s| PermissionMode::from_flag(&s))
        .unwrap_or(PermissionMode::Auto);

    // Mint a v4-ish UUID. We don't have the `uuid` crate as a
    // dep here, so synthesise one from the system clock plus a
    // random-ish suffix — good enough for a smoke test.
    let uuid = mint_uuid();
    eprintln!("smoke: session_id = {}", uuid.as_str());
    eprintln!("smoke: effort = {}, permission_mode = {}", effort.as_flag(), permission_mode.as_flag());
    eprintln!("smoke: prompt = {prompt:?}");
    eprintln!();

    // Wake channel — main loop blocks on it so we don't burn CPU.
    let (tx_wake, rx_wake) = mpsc::channel::<()>();
    let notify = Notifier::new(tx_wake);

    let (driver, _native) = spawn(Arc::new(DebugTrace), notify);

    let mut life = ChatLifecycle::default();
    let mut ts = ChatTranscripts::default();

    // 1) Spawn + UserMessage in the same tick. The CLI doesn't
    // emit `Init` until it reads its first stdin line, so we
    // can't wait for Running before sending — the writer is a
    // buffered mpsc that holds the message until the
    // subprocess's stdin pipe is ready.
    driver.execute(
        [
            ClaudeAction::Spawn {
                uuid: uuid.clone(),
                mode: SpawnMode::Fresh,
                effort,
                permission_mode,
            },
            ClaudeAction::UserMessage {
                uuid: uuid.clone(),
                text: prompt,
            },
        ]
        .iter(),
        &mut life,
        &mut ts,
    );

    // 2) Drain events until TurnComplete / TurnError / Exited.
    let deadline = Instant::now() + TIMEOUT;
    let outcome = drain_until_turn_done(&driver, &rx_wake, &uuid, &mut life, &mut ts, deadline);

    // 5) Cleanly shutdown.
    driver.execute(
        [ClaudeAction::Shutdown { uuid: uuid.clone() }].iter(),
        &mut life,
        &mut ts,
    );
    // Give the subprocess a moment to exit.
    let _ = rx_wake.recv_timeout(Duration::from_secs(2));
    driver.process(&mut life, &mut ts);

    eprintln!();
    eprintln!("smoke: outcome = {outcome:?}");
    eprintln!(
        "smoke: latest_usage = {:?}",
        ts.per_session
            .get(&uuid)
            .and_then(|t| t.latest_usage)
            .map(|u| u.total_prompt())
    );
    eprintln!(
        "smoke: context_window = {:?}",
        ts.per_session
            .get(&uuid)
            .and_then(|t| t.context_window)
    );
    eprintln!(
        "smoke: lifecycle = {:?}",
        life.per_session.get(&uuid)
    );

    std::process::exit(match outcome {
        Outcome::Success => 0,
        _ => 1,
    });
}

#[derive(Debug)]
#[allow(dead_code)]
enum Outcome {
    Success,
    Error(Vec<String>),
    SessionNotFound,
    Exited,
    Timeout,
}

fn drain_until_turn_done(
    driver: &led_driver_claude_core::ClaudeDriver,
    rx_wake: &Receiver<()>,
    uuid: &SessionUuid,
    life: &mut ChatLifecycle,
    ts: &mut ChatTranscripts,
    deadline: Instant,
) -> Outcome {
    use led_driver_claude_core::{LifecycleState, TimelineEvent};
    let mut prev_event_count = ts
        .per_session
        .get(uuid)
        .map(|t| t.events.len())
        .unwrap_or(0);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let _ = rx_wake.recv_timeout(remaining.min(Duration::from_secs(1)));
        driver.process(life, ts);

        let timeline = ts.per_session.get(uuid);
        if let Some(t) = timeline {
            // Print any new events that landed this iteration.
            for ev in &t.events[prev_event_count..] {
                print_event(ev);
            }
            prev_event_count = t.events.len();
            // Stop conditions — only inspect the most recent
            // event (we get re-called on every wake, so we
            // don't have to scan history).
            match t.events.last() {
                Some(TimelineEvent::TurnComplete { .. }) => return Outcome::Success,
                Some(TimelineEvent::TurnError { errors }) => {
                    return Outcome::Error(errors.clone());
                }
                _ => {}
            }
        }
        if matches!(life.per_session.get(uuid), Some(LifecycleState::NotFound)) {
            return Outcome::SessionNotFound;
        }
        if matches!(life.per_session.get(uuid), Some(LifecycleState::Exited(_))) {
            return Outcome::Exited;
        }
    }
    Outcome::Timeout
}

fn print_event(ev: &led_driver_claude_core::TimelineEvent) {
    use led_driver_claude_core::TimelineEvent;
    match ev {
        TimelineEvent::UserSent { text } => println!("> USER: {text}"),
        TimelineEvent::AssistantText { text } => println!("< ASSISTANT TEXT: {text}"),
        TimelineEvent::AssistantToolUse { name, input, .. } => {
            println!("< TOOL_USE {name}: {input}");
        }
        TimelineEvent::ToolResult { tool_use_id, content } => {
            println!("> TOOL_RESULT {tool_use_id}: {content}");
        }
        TimelineEvent::TurnComplete {
            usage,
            cost_usd,
            num_turns,
        } => {
            println!(
                "= TURN COMPLETE: {num_turns} turn(s), ${cost_usd:.6}, prompt={} out={}",
                usage.total_prompt(),
                usage.output_tokens,
            );
        }
        TimelineEvent::TurnError { errors } => {
            for e in errors {
                println!("! ERROR: {e}");
            }
        }
    }
}

// Trivially-random UUID for the smoke test. Format matches what
// the CLI accepts for `--session-id <uuid>` (8-4-4-4-12 hex).
fn mint_uuid() -> SessionUuid {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Splice the timestamp into the UUID layout; the smoke test
    // doesn't need RFC-4122 compliance, just unique-per-run.
    let s = format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (nanos >> 96) as u32,
        (nanos >> 80) as u16,
        (nanos >> 68) as u16 & 0x0fff,
        (nanos >> 56) as u16 & 0x0fff,
        nanos as u64 & 0xffff_ffff_ffff,
    );
    SessionUuid::new(s)
}

/// Drain `process` once at startup so we don't deadlock if the
/// channel buffer fills before we've called `recv` once. Kept as
/// a helper for clarity even though the main loop also calls it.
#[allow(dead_code)]
fn flush(
    driver: &led_driver_claude_core::ClaudeDriver,
    life: &mut ChatLifecycle,
    ts: &mut ChatTranscripts,
) {
    driver.process(life, ts);
}
