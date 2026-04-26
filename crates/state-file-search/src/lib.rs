//! `FileSearchState` — project-wide search + replace overlay (M14).
//!
//! `Option<FileSearchState>` on `Atoms` toggles the overlay on. When
//! active, the sidebar shows a toggle row + query input + (optional)
//! replace input + results tree, focus is on Side, and keystrokes
//! route through the `[file_search]` keymap context.
//!
//! Driven by `driver-file-search/` (ripgrep over the workspace root).
//! See `docs/spec/search.md` § "File-search overlay" for legacy
//! semantics.

use led_core::{CanonPath, TextInput};

// Hit / group types are the driver ABI — re-exported so the overlay
// state + renderer + dispatch use one shape end-to-end, matching the
// pattern used by `state-find-file` with `FindFileEntry`.
pub use led_driver_file_search_core::{FileSearchGroup, FileSearchHit};

// `TabId` reference is light — the state doesn't mutate `Tabs` itself,
// it only remembers which tab was active when the overlay opened so
// `deactivate` can restore focus after closing a preview.
use led_state_tabs::TabId;

/// Which row in the overlay currently has the caret / is selected.
///
/// `SearchInput` / `ReplaceInput` address the input rows at the top
/// of the overlay; `Result(i)` indexes into `flat_hits` and
/// highlights the corresponding hit row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, drv::Input)]
pub enum FileSearchSelection {
    #[default]
    SearchInput,
    ReplaceInput,
    Result(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Default, drv::Input)]
pub struct FileSearchState {
    /// The user's search pattern.
    pub query: TextInput,
    /// The replacement string (only relevant when `replace_mode`).
    pub replace: TextInput,

    /// Grouped-by-file results, as returned by the driver.
    pub results: Vec<FileSearchGroup>,
    /// Flattened hit list for single-cursor navigation across
    /// groups. `results[i].hits` concatenated in order.
    pub flat_hits: Vec<FileSearchHit>,
    /// Row index into the visible tree for the current cursor.
    pub selection: FileSearchSelection,
    /// Visible-window scroll offset (rows from the top).
    pub scroll_offset: usize,

    /// Toggles — displayed in the header as `Aa`, `.*`, `=>`.
    pub case_sensitive: bool,
    pub use_regex: bool,
    pub replace_mode: bool,

    /// Queue of pending ripgrep requests. Dispatch pushes one per
    /// input edit / toggle flip; the main loop drains + ships to
    /// the driver + clears in order. Mirrors find-file's queue
    /// pattern so multiple keystrokes per tick produce one
    /// `FileSearch` trace line each.
    pub pending_search: Vec<PendingSearch>,

    /// Tab that was active when the overlay opened. `deactivate`
    /// restores this after closing any preview tab the overlay
    /// created — same "snapshot on open" discipline find-file uses.
    pub previous_tab: Option<TabId>,

    /// Snapshot of the session-wide edit seq at the moment the
    /// overlay opened. Cross-buffer undo inside the overlay
    /// refuses to pop any group whose seq is `<= overlay_open_seq`
    /// — the user shouldn't be able to undo buffer edits made
    /// before entering Ctrl+F. `0` means "no floor" (e.g. freshly
    /// constructed, or no edits had happened yet).
    pub overlay_open_seq: u64,

    /// Per-hit replacement state, indexed parallel to `flat_hits`.
    /// `Some(entry)` means that hit has been replaced (Right-arrow);
    /// `None` means it's still pending. Rows stay visible in the
    /// tree either way — the painter styles replaced rows dimly so
    /// the user can see which ones they've hit and Left-arrow back
    /// onto a specific row to undo it.
    ///
    /// `hit_replacements.len()` always equals `flat_hits.len()`;
    /// the runtime resets this to `vec![None; len]` whenever the
    /// driver ships a fresh result set.
    pub hit_replacements: Vec<Option<ReplaceEntry>>,
}

/// One recorded per-hit replacement, carrying enough information
/// to revert the edit. Stored at the same index the hit sits at in
/// `flat_hits` — no `flat_hit_idx` field needed.
#[derive(Debug, Clone, PartialEq, Eq, drv::Input)]
pub struct ReplaceEntry {
    /// The hit that was consumed. Carries its own path /
    /// position / preview + match span — we keep this around so
    /// the renderer can style the row and `unreplace_selected`
    /// can splice the original text back in.
    pub hit: FileSearchHit,
    /// The text the replace wrote in place of the original match —
    /// used on undo to splice the original back over this span.
    pub replacement_text: String,
    /// Character count of `replacement_text`.
    pub replacement_char_len: usize,
    /// Character count of the ORIGINAL match, recovered from
    /// `hit.preview[match_start..match_end]`.
    pub original_char_len: usize,
    /// Absolute index in the rope where the replacement was
    /// applied, in characters. `0` is a sentinel for on-disk
    /// entries where the runtime doesn't know a rope coordinate.
    pub rope_char_start: usize,
    /// Canonical path of the affected file.
    pub path: CanonPath,
}

/// One queued search request: the current query + toggle state at
/// the moment of the edit. The driver snaps these into a ripgrep
/// command; the runtime sync-clears the queue before execute.
#[derive(Debug, Clone, PartialEq, Eq, drv::Input)]
pub struct PendingSearch {
    pub query: String,
    pub case_sensitive: bool,
    pub use_regex: bool,
}

impl FileSearchState {
    /// Queue a search request from the current input + toggle
    /// state, or clear any previous results if the query is now
    /// empty.
    ///
    /// Empty-query case isn't a no-op: leaving the stale hits in
    /// the tree would be confusing after the user backspaced the
    /// query to nothing. We also skip dispatching a ripgrep
    /// command for the empty pattern — legacy's discipline, mostly
    /// so the driver doesn't try to match every byte in the tree.
    pub fn queue_search(&mut self) {
        // Any search-input edit invalidates the per-hit replacement
        // map — the indices point into a result set that's about
        // to be replaced wholesale (driver response) or cleared.
        self.hit_replacements.clear();
        if self.query.text.is_empty() {
            self.results.clear();
            self.flat_hits.clear();
            self.selection = FileSearchSelection::SearchInput;
            self.scroll_offset = 0;
            return;
        }
        self.pending_search.push(PendingSearch {
            query: self.query.text.clone(),
            case_sensitive: self.case_sensitive,
            use_regex: self.use_regex,
        });
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;

    fn canon(s: &str) -> led_core::CanonPath {
        UserPath::new(s).canonicalize()
    }

    #[test]
    fn queue_search_clears_stale_results_when_query_becomes_empty() {
        // Simulate a prior search that populated results + a
        // Result selection, then the user backspacing to empty.
        let path = canon("a.rs");
        let hit = FileSearchHit {
            path: path.clone(),
            line: 1,
            col: 1,
            preview: "foo".into(),
            match_start: 0,
            match_end: 3,
        };
        let mut state = FileSearchState {
            results: vec![FileSearchGroup {
                path: path.clone(),
                relative: "a.rs".into(),
                hits: vec![hit.clone()],
            }],
            flat_hits: vec![hit],
            selection: FileSearchSelection::Result(0),
            scroll_offset: 4,
            ..Default::default()
        };
        // Query is empty (nothing typed / user deleted it all).
        state.query.text.clear();
        state.queue_search();
        assert!(state.results.is_empty());
        assert!(state.flat_hits.is_empty());
        assert_eq!(state.selection, FileSearchSelection::SearchInput);
        assert_eq!(state.scroll_offset, 0);
        assert!(state.pending_search.is_empty());
    }

    #[test]
    fn queue_search_with_non_empty_query_pushes_pending_and_keeps_results() {
        let path = canon("a.rs");
        let hit = FileSearchHit {
            path: path.clone(),
            line: 1,
            col: 1,
            preview: "foo".into(),
            match_start: 0,
            match_end: 3,
        };
        let mut state = FileSearchState {
            results: vec![FileSearchGroup {
                path: path.clone(),
                relative: "a.rs".into(),
                hits: vec![hit.clone()],
            }],
            flat_hits: vec![hit],
            ..Default::default()
        };
        state.query.set("bar");
        state.queue_search();
        // Old results stay (they get replaced when the driver
        // response arrives); pending_search picked up the new query.
        assert_eq!(state.results.len(), 1);
        assert_eq!(state.pending_search.len(), 1);
        assert_eq!(state.pending_search[0].query, "bar");
    }
}
