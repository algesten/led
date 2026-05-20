//! LSP diagnostic ABI shapes — the wire types the LSP driver
//! produces and the painter renders.
//!
//! # Scope
//!
//! This crate now holds **only** the shared domain shapes used at
//! the LSP driver boundary: [`Diagnostic`] + [`DiagnosticSeverity`].
//! The per-buffer roster (`DiagnosticsStates`, `BufferDiagnostics`)
//! and the per-server status (`LspStatuses`, `LspServerStatus`)
//! were wholly LSP-discovered state and have been relocated into
//! `led-driver-lsp-core` alongside the rest of the driver-owned
//! sources, per the EXAMPLE-ARCH audit ("wholly external-fact
//! sources belong with the driver that fills them").
//!
//! Keeping just the ABI shapes here lets `driver-terminal/{core,
//! native}` keep rendering `DiagnosticSeverity` without taking a
//! disallowed driver-to-driver dependency on `driver-lsp-core`.
//!
//! # Content-hash stamping + replay
//!
//! Each delivery (now held in `driver-lsp-core::BufferDiagnostics`)
//! carries a `PersistedContentHash` — a hash of the rope's byte
//! content at the moment the pull was dispatched (or the push was
//! cached). The runtime accepts a delivery when:
//!
//! - **Fast path**: the stamped hash equals the buffer's current
//!   ephemeral hash. The rope still holds exactly the bytes the
//!   server analysed; diagnostics are authoritative.
//! - **Replay path**: the buffer's history holds a save-point
//!   marker tagged with the stamped hash. The runtime reconstructs
//!   the save-time rope by inverting every edit since that marker,
//!   then walks forward to transform each diagnostic — dropping
//!   any whose row was touched (content changed, diag is stale)
//!   and shifting rows on structural edits.
//! - Otherwise dropped silently; the next `RequestDiagnostics`
//!   cycle re-pulls against the current hash.
//!
//! Why hash, not a monotonic version? Typing and then deleting
//! restores the original hash. A late cargo-check push for the
//! pre-typing content still lines up with the buffer and the
//! runtime can accept it or cleanly replay through the typing.
//! A counter-based version can never travel backwards, so late
//! deliveries after any undo-style round-trip are lost.

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
