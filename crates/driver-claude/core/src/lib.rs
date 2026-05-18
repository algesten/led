//! Sync core of the Claude driver.
//!
//! Stage 1 of the driver scope (see `Cargo.toml`): a pure stream-json
//! parser. Later stages add the `ChatLifecycle` + `ChatTranscripts`
//! sources, the `ClaudeCmd` / `ClaudeEvent` ABI, and the sync
//! driver's `process` / `execute` methods.

pub mod parser;

pub use parser::{ModelUsage, ParsedStdout, Usage, parse_line};
