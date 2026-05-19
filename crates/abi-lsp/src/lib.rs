//! Shared ABI types at the LSP driver boundary that survive on
//! `state-lsp` across reconnects.
//!
//! - [`InlayHint`] — server-pushed ghost-text the runtime caches on
//!   `state-lsp::BufferInlayHints`.
//! - [`CodeActionSummary`] — picker rows the runtime stashes on
//!   `state-lsp::CodeActionPickerState`.
//! - [`RegistrationGlob`] — compiled `workspace/didChangeWatchedFiles`
//!   patterns the runtime keeps on `state-lsp::LspWatchedGlobs`.
//!
//! Both `driver-lsp/core` (producer) and `state-lsp` (consumer) depend
//! on this leaf crate so the tier-rule from `EXAMPLE-ARCH.md` §
//! "Cross-driver composition lives in a runtime crate" stays clean:
//! `state-*` never reaches into `driver-*`.

use std::sync::Arc;

/// One LSP inlay hint — a short label the server wants the editor to
/// render as ghost text at `(line, col)`. `padding_left` /
/// `padding_right` are the spec's optional flags for controlling
/// whether the label abuts or pads from the surrounding text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHint {
    pub line: u32,
    pub col: u32,
    pub label: Arc<str>,
    pub padding_left: bool,
    pub padding_right: bool,
}

/// One server-registered file-watch glob, parsed from the
/// `client/registerCapability` payload for
/// `workspace/didChangeWatchedFiles`. The compiled `GlobMatcher` is
/// held on the runtime source so per-event matching is alloc-free;
/// `pattern` round-trips for cheap `PartialEq` (the matcher itself
/// doesn't implement it).
///
/// `kinds` is a bitset of LSP `WatchKind` values: `Create=1`,
/// `Change=2`, `Delete=4`. These bit positions are deliberately the
/// same as `driver-file-watch`'s `ChangeKinds` (`CREATED=0b001`,
/// `MODIFIED=0b010`, `REMOVED=0b100`) so the runtime memo can `&`
/// them directly without translation. LSP's `WatchKind` field
/// defaults to all three when absent, so `kinds = 0b111` is the
/// typical value.
#[derive(Debug, Clone)]
pub struct RegistrationGlob {
    pub pattern: String,
    pub matcher: globset::GlobMatcher,
    pub kinds: u8,
}

impl PartialEq for RegistrationGlob {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern && self.kinds == other.kinds
    }
}

impl Eq for RegistrationGlob {}

/// Picker-facing summary of a `CodeAction` from the server. The
/// native driver stores the server's raw item alongside so selection
/// can round-trip through `codeAction/resolve` without the runtime
/// having to understand LSP shapes.
///
/// `action_id` is an opaque string the native driver assigns so
/// `LspCmd::SelectCodeAction` can look the raw item back up without
/// threading `lsp_types::CodeActionOrCommand` values through the
/// runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeActionSummary {
    pub title: Arc<str>,
    pub kind: Option<Arc<str>>,
    /// `true` when the action ships without an `edit` field — the
    /// native driver must issue `codeAction/resolve` on selection
    /// to obtain the edits.
    pub resolve_needed: bool,
    /// Driver-internal id. Carried through
    /// `LspCmd::SelectCodeAction` verbatim so the native driver can
    /// match it to its stored `lsp_types::CodeActionOrCommand`.
    pub action_id: Arc<str>,
}
