//! Sync driver — the main-loop-facing half of the Claude driver.
//!
//! Owns the mpsc pair to the native subprocess worker:
//!
//! - [`ClaudeDriver::process`] drains incoming [`ClaudeEvent`]s
//!   and folds them into the two driver-owned sources
//!   ([`ChatLifecycle`], [`ChatTranscripts`]). Called once per
//!   main-loop iteration.
//! - [`ClaudeDriver::execute`] takes an iterator of
//!   [`ClaudeAction`]s (produced by the runtime's
//!   `subprocess_action` memo), writes intent into the lifecycle
//!   source synchronously, *then* dispatches the corresponding
//!   [`ClaudeCmd`] to the worker. The sync write closes the
//!   re-fire loop per EXAMPLE-ARCH §Actions as diffs — the next
//!   iteration's memo sees Spawning/Exiting and emits Noop until
//!   the worker completes.
//!
//! The mpsc pair is the **mock point** per EXAMPLE-ARCH §Testing
//! — synthetic peers play the role of the worker without spawning
//! any subprocess.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use led_core::SessionUuid;

use crate::abi::{ClaudeCmd, ClaudeEvent, Effort, ExitInfo, PermissionMode, SpawnMode};
use crate::parser::ParsedStdout;
use crate::sources::{ChatLifecycle, ChatTranscripts, LifecycleState, TimelineEvent};

// ── Trace ────────────────────────────────────────────────────────────

/// Observable hooks the runtime can implement to surface chat
/// activity in the unified trace log.
///
/// Default impls are no-ops so a consumer only writes the methods
/// they care about. [`NoopTrace`] is also provided for tests +
/// the path where tracing is unconditionally off.
pub trait Trace: Send + Sync {
    fn spawn(
        &self,
        _uuid: &SessionUuid,
        _mode: SpawnMode,
        _effort: Effort,
        _permission_mode: PermissionMode,
    ) {
    }
    fn init(&self, _uuid: &SessionUuid, _model: &str) {}
    fn user_message(&self, _uuid: &SessionUuid, _len_chars: usize) {}
    fn assistant_text(&self, _uuid: &SessionUuid, _len_chars: usize) {}
    fn tool_use(&self, _uuid: &SessionUuid, _name: &str) {}
    fn tool_result(&self, _uuid: &SessionUuid, _tool_use_id: &str) {}
    fn turn_complete(&self, _uuid: &SessionUuid, _num_turns: u32, _cost_usd: f64) {}
    fn turn_error(&self, _uuid: &SessionUuid, _errors: &[String]) {}
    fn session_not_found(&self, _uuid: &SessionUuid) {}
    fn rate_limit(&self, _uuid: &SessionUuid, _status: &str) {}
    fn cancel(&self, _uuid: &SessionUuid) {}
    fn shutdown(&self, _uuid: &SessionUuid) {}
    fn exited(&self, _uuid: &SessionUuid, _exit: ExitInfo) {}
    fn stderr(&self, _uuid: &SessionUuid, _line: &str) {}
}

pub struct NoopTrace;
impl Trace for NoopTrace {}

// ── Action ───────────────────────────────────────────────────────────

/// Diff result from the runtime's `subprocess_action` memo,
/// consumed by [`ClaudeDriver::execute`].
///
/// One-to-one with [`ClaudeCmd`] for now — the indirection lets
/// the memo speak in "what to do" terms while the driver owns
/// the transport. Empty `Vec` returns from the memo encode "no
/// action this tick" (no `Noop` variant needed).
#[derive(Debug, Clone, PartialEq)]
pub enum ClaudeAction {
    Spawn {
        uuid: SessionUuid,
        mode: SpawnMode,
        effort: Effort,
        permission_mode: PermissionMode,
    },
    UserMessage {
        uuid: SessionUuid,
        text: String,
    },
    Cancel {
        uuid: SessionUuid,
    },
    Shutdown {
        uuid: SessionUuid,
    },
}

// ── Driver ───────────────────────────────────────────────────────────

pub struct ClaudeDriver {
    tx_cmd: Sender<ClaudeCmd>,
    rx_event: Receiver<ClaudeEvent>,
    trace: Arc<dyn Trace>,
}

impl ClaudeDriver {
    pub fn new(
        tx_cmd: Sender<ClaudeCmd>,
        rx_event: Receiver<ClaudeEvent>,
        trace: Arc<dyn Trace>,
    ) -> Self {
        Self {
            tx_cmd,
            rx_event,
            trace,
        }
    }

    /// Drain all pending events from the worker into the driver's
    /// two sources. Called once per main-loop iteration after
    /// `recv_timeout` unblocks (idle case is `Vec::new()`-equivalent
    /// — no alloc, no work).
    pub fn process(&self, life: &mut ChatLifecycle, ts: &mut ChatTranscripts) {
        while let Ok(event) = self.rx_event.try_recv() {
            match event {
                ClaudeEvent::Parsed { uuid, parsed } => {
                    self.fold_parsed(&uuid, parsed, life, ts);
                }
                ClaudeEvent::Stderr { uuid, line } => {
                    self.trace.stderr(&uuid, &line);
                }
                ClaudeEvent::Exited { uuid, exit } => {
                    life.per_session.insert(uuid.clone(), LifecycleState::Exited(exit));
                    self.trace.exited(&uuid, exit);
                }
            }
        }
    }

    fn fold_parsed(
        &self,
        uuid: &SessionUuid,
        parsed: ParsedStdout,
        life: &mut ChatLifecycle,
        ts: &mut ChatTranscripts,
    ) {
        match parsed {
            ParsedStdout::Init {
                model, ..
            } => {
                life.per_session
                    .insert(uuid.clone(), LifecycleState::Running);
                let timeline = ts.per_session.entry(uuid.clone()).or_default();
                timeline.model = Some(model.clone());
                self.trace.init(uuid, &model);
            }
            ParsedStdout::AssistantText { text, usage, .. } => {
                let timeline = ts.per_session.entry(uuid.clone()).or_default();
                self.trace.assistant_text(uuid, text.chars().count());
                timeline.events.push(TimelineEvent::AssistantText { text });
                if let Some(u) = usage {
                    timeline.latest_usage = Some(u);
                }
            }
            ParsedStdout::AssistantToolUse {
                tool_use_id,
                name,
                input,
                usage,
                ..
            } => {
                let timeline = ts.per_session.entry(uuid.clone()).or_default();
                self.trace.tool_use(uuid, &name);
                timeline.events.push(TimelineEvent::AssistantToolUse {
                    tool_use_id,
                    name,
                    input,
                });
                if let Some(u) = usage {
                    timeline.latest_usage = Some(u);
                }
            }
            ParsedStdout::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                let timeline = ts.per_session.entry(uuid.clone()).or_default();
                self.trace.tool_result(uuid, &tool_use_id);
                timeline.events.push(TimelineEvent::ToolResult {
                    tool_use_id,
                    content,
                });
            }
            ParsedStdout::Success {
                usage,
                total_cost_usd,
                num_turns,
                model_usage,
                ..
            } => {
                let timeline = ts.per_session.entry(uuid.clone()).or_default();
                timeline.latest_usage = Some(usage);
                // First non-zero contextWindow wins for the
                // session — the model doesn't change mid-session
                // (changing models cuts a new session).
                if timeline.context_window.is_none()
                    && let Some(cw) = pick_context_window(&model_usage, timeline.model.as_deref())
                {
                    timeline.context_window = Some(cw);
                }
                timeline.events.push(TimelineEvent::TurnComplete {
                    usage,
                    cost_usd: total_cost_usd,
                    num_turns,
                });
                self.trace.turn_complete(uuid, num_turns, total_cost_usd);
            }
            ParsedStdout::SessionNotFound { errors, .. } => {
                life.per_session
                    .insert(uuid.clone(), LifecycleState::NotFound);
                // Still record the error on the transcript so the
                // UI can show "session not found" inline.
                let timeline = ts.per_session.entry(uuid.clone()).or_default();
                timeline
                    .events
                    .push(TimelineEvent::TurnError { errors: errors.clone() });
                self.trace.session_not_found(uuid);
                self.trace.turn_error(uuid, &errors);
            }
            ParsedStdout::Error { errors, .. } => {
                let timeline = ts.per_session.entry(uuid.clone()).or_default();
                self.trace.turn_error(uuid, &errors);
                timeline.events.push(TimelineEvent::TurnError { errors });
            }
            ParsedStdout::RateLimit { status, .. } => {
                self.trace.rate_limit(uuid, &status);
            }
        }
    }

    /// Apply actions from the runtime's `subprocess_action` memo.
    ///
    /// Each variant writes intent into `life` (and, for
    /// UserMessage, mirrors into `ts`) **before** sending the cmd
    /// — so the next iteration's diff returns Noop until the
    /// worker completes the operation.
    pub fn execute<'a, I>(
        &self,
        actions: I,
        life: &mut ChatLifecycle,
        ts: &mut ChatTranscripts,
    ) where
        I: IntoIterator<Item = &'a ClaudeAction>,
    {
        for action in actions {
            match action {
                ClaudeAction::Spawn {
                    uuid,
                    mode,
                    effort,
                    permission_mode,
                } => {
                    life.per_session
                        .insert(uuid.clone(), LifecycleState::Spawning);
                    self.trace.spawn(uuid, *mode, *effort, *permission_mode);
                    let _ = self.tx_cmd.send(ClaudeCmd::Spawn {
                        uuid: uuid.clone(),
                        mode: *mode,
                        effort: *effort,
                        permission_mode: *permission_mode,
                    });
                }
                ClaudeAction::UserMessage { uuid, text } => {
                    let timeline = ts.per_session.entry(uuid.clone()).or_default();
                    timeline
                        .events
                        .push(TimelineEvent::UserSent { text: text.clone() });
                    self.trace.user_message(uuid, text.chars().count());
                    let _ = self.tx_cmd.send(ClaudeCmd::UserMessage {
                        uuid: uuid.clone(),
                        text: text.clone(),
                    });
                }
                ClaudeAction::Cancel { uuid } => {
                    if let Some(state) = life.per_session.get_mut(uuid)
                        && matches!(state, LifecycleState::Running | LifecycleState::Spawning)
                    {
                        *state = LifecycleState::Exiting;
                    }
                    self.trace.cancel(uuid);
                    let _ = self.tx_cmd.send(ClaudeCmd::Cancel { uuid: uuid.clone() });
                }
                ClaudeAction::Shutdown { uuid } => {
                    if let Some(state) = life.per_session.get_mut(uuid)
                        && matches!(state, LifecycleState::Running | LifecycleState::Spawning)
                    {
                        *state = LifecycleState::Exiting;
                    }
                    self.trace.shutdown(uuid);
                    let _ = self.tx_cmd.send(ClaudeCmd::Shutdown { uuid: uuid.clone() });
                }
            }
        }
    }
}

/// Pick the `contextWindow` from `model_usage` for the spawned
/// model.
///
/// The CLI runs Haiku for short auxiliary calls (auto-mode
/// classification etc.) and may include both Haiku and the
/// user-selected model in `modelUsage`. The window we care about
/// is the one for the *spawned* model, falling back to the
/// largest reported window if the spawned model isn't keyed
/// (defensive — observed shape includes the spawned model on
/// every successful turn).
fn pick_context_window(
    model_usage: &std::collections::HashMap<String, crate::parser::ModelUsage>,
    spawned_model: Option<&str>,
) -> Option<u32> {
    if let Some(model) = spawned_model
        && let Some(mu) = model_usage.get(model)
        && mu.context_window > 0
    {
        return Some(mu.context_window);
    }
    model_usage
        .values()
        .map(|mu| mu.context_window)
        .filter(|w| *w > 0)
        .max()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc;

    use crate::parser::{ModelUsage, Usage};

    use super::*;

    // ── Test plumbing ────────────────────────────────────────────────

    /// Trace that records every call for inspection in tests.
    #[derive(Default)]
    struct CapturingTrace {
        calls: Mutex<Vec<String>>,
    }

    impl CapturingTrace {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn record(&self, s: String) {
            self.calls.lock().unwrap().push(s);
        }
    }

    impl Trace for CapturingTrace {
        fn spawn(
            &self,
            uuid: &SessionUuid,
            mode: SpawnMode,
            _effort: Effort,
            _perm: PermissionMode,
        ) {
            self.record(format!("spawn:{}:{:?}", uuid.as_str(), mode));
        }
        fn init(&self, uuid: &SessionUuid, model: &str) {
            self.record(format!("init:{}:{}", uuid.as_str(), model));
        }
        fn user_message(&self, uuid: &SessionUuid, len: usize) {
            self.record(format!("user:{}:{}", uuid.as_str(), len));
        }
        fn assistant_text(&self, uuid: &SessionUuid, len: usize) {
            self.record(format!("assist:{}:{}", uuid.as_str(), len));
        }
        fn cancel(&self, uuid: &SessionUuid) {
            self.record(format!("cancel:{}", uuid.as_str()));
        }
        fn shutdown(&self, uuid: &SessionUuid) {
            self.record(format!("shutdown:{}", uuid.as_str()));
        }
        fn exited(&self, uuid: &SessionUuid, exit: ExitInfo) {
            self.record(format!("exit:{}:{:?}", uuid.as_str(), exit.code));
        }
        fn session_not_found(&self, uuid: &SessionUuid) {
            self.record(format!("not_found:{}", uuid.as_str()));
        }
    }

    fn rig() -> (
        ClaudeDriver,
        Sender<ClaudeEvent>,
        Receiver<ClaudeCmd>,
        Arc<CapturingTrace>,
    ) {
        let (tx_cmd, rx_cmd) = mpsc::channel();
        let (tx_event, rx_event) = mpsc::channel();
        let trace = Arc::new(CapturingTrace::default());
        let driver = ClaudeDriver::new(tx_cmd, rx_event, trace.clone());
        (driver, tx_event, rx_cmd, trace)
    }

    fn uuid(s: &str) -> SessionUuid {
        SessionUuid::new(s)
    }

    // ── execute writes intent sync + dispatches cmd ──────────────────

    #[test]
    fn execute_spawn_writes_spawning_and_sends_cmd() {
        let (driver, _tx_event, rx_cmd, trace) = rig();
        let mut life = ChatLifecycle::default();
        let mut ts = ChatTranscripts::default();
        let u = uuid("u1");

        driver.execute(
            [ClaudeAction::Spawn {
                uuid: u.clone(),
                mode: SpawnMode::Fresh,
                effort: Effort::XHigh,
                permission_mode: PermissionMode::Auto,
            }]
            .iter(),
            &mut life,
            &mut ts,
        );

        // (1) Sync intent: lifecycle = Spawning.
        assert_eq!(life.per_session.get(&u), Some(&LifecycleState::Spawning));
        // (2) Cmd dispatched.
        match rx_cmd.try_recv().expect("cmd dispatched") {
            ClaudeCmd::Spawn { uuid, mode, effort, permission_mode } => {
                assert_eq!(uuid, u);
                assert!(matches!(mode, SpawnMode::Fresh));
                assert_eq!(effort, Effort::XHigh);
                assert_eq!(permission_mode, PermissionMode::Auto);
            }
            other => panic!("unexpected cmd: {other:?}"),
        }
        // (3) Trace fired.
        assert!(trace.calls().iter().any(|c| c.starts_with("spawn:u1:")));
    }

    #[test]
    fn execute_user_message_mirrors_into_timeline_and_sends() {
        let (driver, _tx_event, rx_cmd, _) = rig();
        let mut life = ChatLifecycle::default();
        let mut ts = ChatTranscripts::default();
        let u = uuid("u1");
        life.per_session.insert(u.clone(), LifecycleState::Running);

        driver.execute(
            [ClaudeAction::UserMessage {
                uuid: u.clone(),
                text: "hi".into(),
            }]
            .iter(),
            &mut life,
            &mut ts,
        );

        let timeline = &ts.per_session[&u];
        assert_eq!(timeline.events.len(), 1);
        assert!(matches!(
            &timeline.events[0],
            TimelineEvent::UserSent { text } if text == "hi"
        ));
        assert!(matches!(rx_cmd.try_recv(), Ok(ClaudeCmd::UserMessage { .. })));
    }

    #[test]
    fn execute_cancel_transitions_running_to_exiting() {
        let (driver, _tx_event, rx_cmd, _) = rig();
        let mut life = ChatLifecycle::default();
        let mut ts = ChatTranscripts::default();
        let u = uuid("u1");
        life.per_session.insert(u.clone(), LifecycleState::Running);

        driver.execute(
            [ClaudeAction::Cancel { uuid: u.clone() }].iter(),
            &mut life,
            &mut ts,
        );

        assert_eq!(life.per_session.get(&u), Some(&LifecycleState::Exiting));
        assert!(matches!(rx_cmd.try_recv(), Ok(ClaudeCmd::Cancel { .. })));
    }

    #[test]
    fn execute_cancel_on_exited_session_does_not_resurrect() {
        let (driver, _tx_event, _rx_cmd, _) = rig();
        let mut life = ChatLifecycle::default();
        let mut ts = ChatTranscripts::default();
        let u = uuid("u1");
        life.per_session.insert(u.clone(), LifecycleState::Exited(ExitInfo::default()));

        driver.execute(
            [ClaudeAction::Cancel { uuid: u.clone() }].iter(),
            &mut life,
            &mut ts,
        );

        // Cancel on an already-exited session keeps the Exited
        // state — Cancel must not overwrite the death record.
        assert!(matches!(
            life.per_session.get(&u),
            Some(LifecycleState::Exited(_))
        ));
    }

    // ── process drains events into sources ───────────────────────────

    #[test]
    fn process_init_event_transitions_to_running_and_records_model() {
        let (driver, tx_event, _rx_cmd, trace) = rig();
        let mut life = ChatLifecycle::default();
        let mut ts = ChatTranscripts::default();
        let u = uuid("u1");
        life.per_session.insert(u.clone(), LifecycleState::Spawning);

        tx_event
            .send(ClaudeEvent::Parsed {
                uuid: u.clone(),
                parsed: ParsedStdout::Init {
                    session_id: "u1".into(),
                    model: "claude-opus-4-7[1m]".into(),
                    cwd: "/x".into(),
                    tools: vec![],
                    permission_mode: "auto".into(),
                },
            })
            .unwrap();
        driver.process(&mut life, &mut ts);

        assert_eq!(life.per_session.get(&u), Some(&LifecycleState::Running));
        assert_eq!(
            ts.per_session.get(&u).and_then(|t| t.model.as_deref()),
            Some("claude-opus-4-7[1m]")
        );
        assert!(trace
            .calls()
            .iter()
            .any(|c| c == "init:u1:claude-opus-4-7[1m]"));
    }

    #[test]
    fn process_assistant_text_appends_event_and_updates_usage() {
        let (driver, tx_event, _rx_cmd, _) = rig();
        let mut life = ChatLifecycle::default();
        let mut ts = ChatTranscripts::default();
        let u = uuid("u1");

        let usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        };
        tx_event
            .send(ClaudeEvent::Parsed {
                uuid: u.clone(),
                parsed: ParsedStdout::AssistantText {
                    session_id: "u1".into(),
                    text: "hello".into(),
                    usage: Some(usage),
                },
            })
            .unwrap();
        driver.process(&mut life, &mut ts);

        let timeline = &ts.per_session[&u];
        assert_eq!(timeline.events.len(), 1);
        assert!(matches!(
            &timeline.events[0],
            TimelineEvent::AssistantText { text } if text == "hello"
        ));
        assert_eq!(timeline.latest_usage, Some(usage));
    }

    #[test]
    fn process_session_not_found_marks_notfound_and_does_not_reset_on_later_events() {
        let (driver, tx_event, _rx_cmd, trace) = rig();
        let mut life = ChatLifecycle::default();
        let mut ts = ChatTranscripts::default();
        let u = uuid("u1");
        life.per_session.insert(u.clone(), LifecycleState::Spawning);

        tx_event
            .send(ClaudeEvent::Parsed {
                uuid: u.clone(),
                parsed: ParsedStdout::SessionNotFound {
                    emitted_session_id: "new-error-uuid".into(),
                    errors: vec!["No conversation found with session ID: u1".into()],
                },
            })
            .unwrap();
        driver.process(&mut life, &mut ts);

        assert_eq!(life.per_session.get(&u), Some(&LifecycleState::NotFound));
        assert!(trace.calls().iter().any(|c| c == "not_found:u1"));
        // The error is also recorded on the timeline so the UI
        // can surface "session not found" inline.
        let timeline = &ts.per_session[&u];
        assert!(matches!(timeline.events.last(), Some(TimelineEvent::TurnError { .. })));
    }

    #[test]
    fn process_success_picks_context_window_from_spawned_model() {
        let (driver, tx_event, _rx_cmd, _) = rig();
        let mut life = ChatLifecycle::default();
        let mut ts = ChatTranscripts::default();
        let u = uuid("u1");
        // Pre-set model from a prior Init.
        ts.per_session.entry(u.clone()).or_default().model =
            Some("claude-opus-4-7[1m]".into());

        let mut model_usage = std::collections::HashMap::new();
        model_usage.insert(
            "claude-haiku-4-5-20251001".into(),
            ModelUsage {
                context_window: 200_000,
                ..Default::default()
            },
        );
        model_usage.insert(
            "claude-opus-4-7[1m]".into(),
            ModelUsage {
                context_window: 1_000_000,
                ..Default::default()
            },
        );

        tx_event
            .send(ClaudeEvent::Parsed {
                uuid: u.clone(),
                parsed: ParsedStdout::Success {
                    session_id: "u1".into(),
                    result_text: "ok".into(),
                    usage: Usage::default(),
                    total_cost_usd: 0.01,
                    duration_ms: 100,
                    num_turns: 1,
                    model_usage,
                },
            })
            .unwrap();
        driver.process(&mut life, &mut ts);

        // The spawned model is opus-4-7[1m] (1M window), NOT
        // haiku (which is also present from auto-mode classifier).
        assert_eq!(ts.per_session[&u].context_window, Some(1_000_000));
    }

    #[test]
    fn process_exit_event_marks_exited() {
        let (driver, tx_event, _rx_cmd, trace) = rig();
        let mut life = ChatLifecycle::default();
        let mut ts = ChatTranscripts::default();
        let u = uuid("u1");

        let exit = ExitInfo { code: Some(0), signal: None };
        tx_event
            .send(ClaudeEvent::Exited {
                uuid: u.clone(),
                exit,
            })
            .unwrap();
        driver.process(&mut life, &mut ts);

        assert_eq!(life.per_session.get(&u), Some(&LifecycleState::Exited(exit)));
        assert!(trace.calls().iter().any(|c| c == "exit:u1:Some(0)"));
    }
}
