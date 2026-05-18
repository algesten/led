//! Sync core of the Claude driver.
//!
//! Three layers, all portable and unit-testable without spawning a
//! subprocess:
//!
//! - [`parser`] — stream-json decoder mapping one CLI stdout line
//!   to a typed [`ParsedStdout`].
//! - [`sources`] — external-fact sources owned by the driver:
//!   [`ChatLifecycle`] (per-session subprocess state) and
//!   [`ChatTranscripts`] (per-session live event log + usage).
//! - [`abi`] — [`ClaudeCmd`] / [`ClaudeEvent`] crossing the mpsc
//!   to the native subprocess worker, plus the [`Effort`] /
//!   [`PermissionMode`] / [`SpawnMode`] vocabulary the worker
//!   translates into CLI flags.
//!
//! Later stages add the sync driver's `process` / `execute` methods
//! and the `Trace` trait.

pub mod abi;
pub mod driver;
pub mod parser;
pub mod sources;

pub use abi::{
    ClaudeCmd, ClaudeEvent, Effort, ExitInfo, PermissionMode, SpawnMode,
};
pub use driver::{ClaudeAction, ClaudeDriver, NoopTrace, Trace};
pub use parser::{ModelUsage, ParsedStdout, Usage, parse_line};
pub use sources::{ChatLifecycle, ChatTranscripts, LifecycleState, SessionTimeline, TimelineEvent};
