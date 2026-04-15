//! Process-wide sink for the goldens trace. Off by default; led's `run()`
//! installs a sink only when `--golden-trace <FILE>` is set.
//!
//! This is the seam that lets crates deep in the stack (e.g. the LSP
//! transport layer) emit trace lines without depending on the `led` crate.
//! The sink is set once at startup; emit calls are no-ops until then.

use std::sync::Arc;
use std::sync::OnceLock;

pub trait TraceSink: Send + Sync {
    fn emit(&self, category: &str, fields: &str);
}

static SINK: OnceLock<Arc<dyn TraceSink>> = OnceLock::new();

/// Install the global sink. Subsequent calls are silently ignored — the
/// goldens trace is a one-shot per process.
pub fn set_sink(sink: Arc<dyn TraceSink>) {
    let _ = SINK.set(sink);
}

/// Emit one trace line. No-op if no sink is installed.
pub fn emit(category: &str, fields: &str) {
    if let Some(sink) = SINK.get() {
        sink.emit(category, fields);
    }
}

/// Cheap check so callers can skip expensive field formatting when no
/// sink is installed.
pub fn is_active() -> bool {
    SINK.get().is_some()
}
