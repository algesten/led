//! `IsearchState` — in-buffer incremental search.
//!
//! Legacy semantics (`docs/spec/search.md` § "In-buffer isearch"):
//!
//! - `Ctrl-s` starts the search, capturing the current cursor +
//!   scroll as the `origin`.
//! - Typing appends to the query; each keystroke recomputes the
//!   match list case-insensitively and jumps to the first match
//!   at-or-after the current cursor position.
//! - No forward match flips a `failed` flag. A second `Ctrl-s`
//!   from `failed` wraps to match index 0.
//! - `Ctrl-s` with an empty query recalls `last_query` (if any).
//! - `Enter` accepts — clears the overlay, keeps cursor where it
//!   is, stashes the query into `last_query`, and pushes a
//!   `JumpRecord` for the origin if the cursor moved.
//! - `Esc` / `Ctrl-g` aborts — restores the origin and saves
//!   `last_query` for next time.
//! - Any editing key or arrow key while isearch is active emits
//!   `SearchAccept` *and* runs its normal handler on the same
//!   tick ("accept on passthrough").

use led_core::TextInput;
use led_state_tabs::{Cursor, Scroll};

/// A single byte-range match inside the active buffer's rope.
///
/// `char_start` / `char_end` are absolute rope-char indices — the
/// highlighter / cursor jumper converts these to `Cursor { line,
/// col }` as needed. Using char indices (not byte) is consistent
/// with how `Cursor` itself counts columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsearchMatch {
    pub char_start: usize,
    pub char_end: usize,
}

/// Whole-session state for an active isearch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsearchState {
    /// Editable query (text + cursor + transient hint via
    /// `TextInput`). Rendered after the `Search:` prompt in the
    /// status bar.
    pub query: TextInput,

    /// Cursor + scroll position at `start_search` time. `Esc`
    /// restores them; `Enter` pushes a JumpRecord for this
    /// position if the cursor has moved.
    pub origin_cursor: Cursor,
    pub origin_scroll: Scroll,

    /// All matches of `query` in the active buffer's rope, in
    /// document order. Recomputed on every query change.
    pub matches: Vec<IsearchMatch>,

    /// Index into `matches` of the currently-selected hit. `None`
    /// when the query has no matches (yet / any more).
    pub match_idx: Option<usize>,

    /// `true` when the last search couldn't find a forward match
    /// (all matches are before the cursor, or no matches at all).
    /// The next `Ctrl-s` wraps to match 0 and clears the flag.
    pub failed: bool,

    /// Recalled on `Ctrl-s` with an empty query. Persists across
    /// Esc/Enter so the user's previous search is available for
    /// re-triggering.
    pub last_query: Option<String>,
}

impl IsearchState {
    /// Start a fresh isearch session at `origin_cursor` /
    /// `origin_scroll`. `last_query` carries over from a prior
    /// session if the caller has one stashed.
    pub fn start(
        origin_cursor: Cursor,
        origin_scroll: Scroll,
        last_query: Option<String>,
    ) -> Self {
        Self {
            query: TextInput::default(),
            origin_cursor,
            origin_scroll,
            matches: Vec::new(),
            match_idx: None,
            failed: false,
            last_query,
        }
    }
}

led_core::impl_identity_to_static!(IsearchState);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_seeds_empty_query_and_records_origin() {
        let origin = Cursor { line: 3, col: 7, preferred_col: 7 };
        let scroll = Scroll { top: 0, top_sub_line: led_core::SubLine(0) };
        let s = IsearchState::start(origin, scroll, Some("prev".into()));
        assert_eq!(s.query.text, "");
        assert_eq!(s.origin_cursor, origin);
        assert_eq!(s.last_query.as_deref(), Some("prev"));
        assert!(s.matches.is_empty());
        assert!(!s.failed);
    }
}
