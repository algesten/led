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

/// Which row in the overlay currently has the caret / is selected.
///
/// `SearchInput` / `ReplaceInput` address the input rows at the top
/// of the overlay; `Result(i)` indexes into `flat_hits` and
/// highlights the corresponding hit row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileSearchSelection {
    #[default]
    SearchInput,
    ReplaceInput,
    Result(usize),
}

/// One hit from the ripgrep driver. Kept as char offsets + display
/// text (the one-line context string) so the renderer doesn't need
/// to re-open the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchHit {
    pub path: CanonPath,
    /// 1-indexed (matches ripgrep output conventions).
    pub line: usize,
    pub col: usize,
    /// Rendered single-line preview text.
    pub preview: String,
}

/// Results grouped by file. Matches legacy's "tree" UI where hits
/// collapse under their file header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchGroup {
    pub path: CanonPath,
    pub hits: Vec<FileSearchHit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
}
