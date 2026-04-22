//! Per-buffer LSP diagnostics — the state atom plus the domain
//! types that cross the LSP driver's ABI.
//!
//! # Scope
//!
//! Just the shapes. The propagation state machine (push vs pull,
//! window open/close, freeze discipline) lives in
//! `led-driver-lsp-core::DiagnosticSource` — that logic is driver-
//! specific bookkeeping, not buffer-level state. This crate is the
//! quiet side: "what diagnostics does buffer X have right now, if
//! any". The runtime's painter reads from here.
//!
//! # Version stamping — hard gate, no smear
//!
//! Each delivery carries a `BufferVersion` — the `eb.version` the
//! buffer was at when the pull was dispatched. The runtime accepts
//! a delivery only if the stamped version matches the buffer's
//! **current** version. Anything else is dropped silently; the
//! next `RequestDiagnostics` cycle re-pulls against the new
//! version.
//!
//! The painter also hides `BufferDiagnostics` whose `version` no
//! longer matches the buffer's current version — the gutter
//! blanks the instant the user edits and fills back in only when
//! the fresh pull response lands.
//!
//! No rebase. No save-point markers. No replay-through-edits. The
//! tradeoff is explicit: showing diagnostics for a version the
//! user has already edited past is actively misleading (it claims
//! errors on lines that have moved). Invisible is safer than
//! misleading. Syntax highlighting makes the opposite choice
//! because slightly-wrong colour beats flicker; diagnostics
//! because slightly-wrong error markers don't.

use imbl::HashMap;
use led_core::CanonPath;

// ── Domain types (ABI-shared between driver-lsp-core + runtime) ──

/// Severity of one diagnostic. Mirrors LSP's 1..=4 scale but kept
/// as a narrow enum so the rest of the code doesn't leak
/// `lsp-types`. The painter maps each variant to a style in
/// `theme.diagnostics.*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

/// A single diagnostic, in char-offset coordinates inside its
/// owning buffer. `source` is the LSP server's identifier
/// (`"rust-analyzer"`, `"typescript"`, …); `code` is the
/// diagnostic code (`"E0277"`) used by status-bar navigation to
/// match the same finding across push and pull deliveries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
}

/// Monotonic buffer version at pull-dispatch time. Same numeric
/// space as `led_state_buffer_edits::EditedBuffer::version`.
/// Wrapped so a stale stamp can't be confused with any other
/// `u64` counter in the codebase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BufferVersion(pub u64);

// ── Atom ──

/// Per-buffer diagnostics, keyed by canonical path. Populated by
/// the runtime when it accepts an [`LspEvent::Diagnostics`]
/// delivery whose stamped version is still reachable from the
/// current buffer state; cleared when the buffer closes or the
/// set becomes empty.
///
/// Wrapped in `imbl::HashMap` for the usual pointer-clone
/// cheap-equality discipline — painter memos only re-render when
/// the map identity changes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiagnosticsStates {
    pub by_path: HashMap<CanonPath, BufferDiagnostics>,
}

/// One buffer's diagnostic roster plus the version the roster was
/// computed against. The painter renders these ONLY when the
/// stored version equals the buffer's current version — see the
/// module docs for the "no smear" rationale.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BufferDiagnostics {
    pub version: BufferVersion,
    pub diagnostics: Vec<Diagnostic>,
}

/// One LSP server's live status. Populated by the runtime from
/// `LspEvent::Progress` and `LspEvent::Ready` deliveries; the
/// status bar renders the first `busy = true` entry it finds
/// (priority: servers indexing > idle > absent).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LspServerStatus {
    /// `true` while the server is mid-task (typing "indexing",
    /// "building", etc.). Derived from `$/progress` `begin` /
    /// `report` messages; cleared on `end`.
    pub busy: bool,
    /// Human-readable tail the server emitted last. Shown next
    /// to the server name when present.
    pub detail: Option<String>,
    /// `true` once the server has emitted `experimental/serverStatus
    /// quiescent=true` at least once (rust-analyzer) OR has
    /// finished its last progress cycle (generic servers).
    pub ready: bool,
}

/// Per-server LSP status map, keyed by the server name the
/// driver assigned (`format!("{:?}", language)` — e.g.
/// `"Rust"`, `"TypeScript"`). Kept separate from
/// `DiagnosticsStates` because its identity churns on a
/// different cadence (progress events, not diagnostic cycles)
/// and bundling would invalidate the diagnostic memos on every
/// keystroke.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LspStatuses {
    pub by_server: HashMap<String, LspServerStatus>,
}

impl LspStatuses {
    /// `true` if any server is currently mid-task. The main
    /// loop uses this to decide whether to schedule an 80ms
    /// wake so the status-bar spinner animates.
    pub fn any_busy(&self) -> bool {
        self.by_server.values().any(|s| s.busy)
    }
}

impl BufferDiagnostics {
    pub fn new(version: BufferVersion, diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            version,
            diagnostics,
        }
    }

    /// Count diagnostics matching a given severity — used by the
    /// status bar's `N errors, M warnings` indicator.
    pub fn count(&self, severity: DiagnosticSeverity) -> usize {
        self.diagnostics.iter().filter(|d| d.severity == severity).count()
    }

    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(severity: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 5,
            severity,
            message: String::new(),
            source: None,
            code: None,
        }
    }

    #[test]
    fn count_per_severity() {
        let b = BufferDiagnostics::new(
            BufferVersion(7),
            vec![
                diag(DiagnosticSeverity::Error),
                diag(DiagnosticSeverity::Error),
                diag(DiagnosticSeverity::Warning),
                diag(DiagnosticSeverity::Info),
            ],
        );
        assert_eq!(b.count(DiagnosticSeverity::Error), 2);
        assert_eq!(b.count(DiagnosticSeverity::Warning), 1);
        assert_eq!(b.count(DiagnosticSeverity::Info), 1);
        assert_eq!(b.count(DiagnosticSeverity::Hint), 0);
    }

    #[test]
    fn default_is_empty_at_version_zero() {
        let b = BufferDiagnostics::default();
        assert!(b.is_empty());
        assert_eq!(b.version, BufferVersion(0));
    }
}
