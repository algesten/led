//! Shared ABI types at the file-search driver boundary.
//!
//! [`FileSearchHit`] and [`FileSearchGroup`] are the wire shape the
//! file-search driver emits and the overlay state stores. Both
//! `driver-file-search/core` (producer) and `state-file-search`
//! (consumer) depend on this leaf crate so the tier-rule from
//! `EXAMPLE-ARCH.md` § "Cross-driver composition lives in a runtime
//! crate" stays clean: `state-*` never reaches into `driver-*`.

use led_core::CanonPath;

/// One match inside a file. Positions are all 1-indexed to match
/// ripgrep's output conventions; `match_start` / `match_end` are
/// byte offsets into `preview` (kept for later rendering of the
/// hit inside the preview line, and for the replace flow).
#[derive(Debug, Clone, PartialEq, Eq, drv::Input)]
pub struct FileSearchHit {
    pub path: CanonPath,
    /// 1-indexed line number.
    pub line: usize,
    /// 1-indexed column of the first char of the match.
    pub col: usize,
    /// Single-line preview (the matched line with its newline
    /// trimmed). The UI renders this as-is.
    pub preview: String,
    /// Byte offsets inside `preview` — the highlight span.
    pub match_start: usize,
    pub match_end: usize,
}

/// All hits in a single file. `relative` is the file's path
/// rendered relative to the search root; the UI shows this as the
/// group header.
#[derive(Debug, Clone, PartialEq, Eq, drv::Input)]
pub struct FileSearchGroup {
    pub path: CanonPath,
    pub relative: String,
    pub hits: Vec<FileSearchHit>,
}
