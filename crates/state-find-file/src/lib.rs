//! Find-file / save-as overlay state.
//!
//! `FindFileState` is held behind an `Option` on `Atoms`. `None` means
//! the overlay isn't active; `Some` means dispatch is in overlay mode —
//! the `[find_file]` keymap context takes over, the status bar shows
//! the prompt (`Find file:` / `Save as:`), and the side panel displays
//! completions when `show_side` is set.
//!
//! Completions come from `driver-find-file` as `Vec<FindFileEntry>`;
//! the driver is prefix-filter-aware (case-insensitive on the leaf
//! name) and dir-sorted-first. The entry kind (`is_dir`) drives
//! whether Tab appends `/` and whether Enter descends or opens.

use std::path::{Path, PathBuf};

// `FindFileEntry` is the driver ABI type — state re-exports it so
// overlay consumers (dispatch, rendering) import a single name.
pub use led_driver_find_file_core::FindFileEntry;

/// Which mode the overlay is in. Opens and Save-as share the same
/// input editor + completions UI but differ in activation input
/// seeding and Enter semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindFileMode {
    /// `Ctrl+x Ctrl+f`. Enter opens (or creates) a file.
    Open,
    /// `Ctrl+x Ctrl+w`. Enter writes the active buffer to the input
    /// path.
    SaveAs,
}

/// Overlay state.
///
/// All fields are mutated by dispatch on every keystroke while the
/// overlay is active; the `Option<FindFileState>` on `Atoms` toggles
/// the overlay off at the top level when dispatch deactivates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindFileState {
    pub mode: FindFileMode,

    /// Current input buffer. Rendered after the `Find file: ` /
    /// `Save as: ` prompt in the status bar.
    pub input: String,

    /// Cursor position within `input`, as a byte offset at a char
    /// boundary. Moves via arrow keys / Home / End / edits.
    pub cursor: usize,

    /// `input` as it was before the user last arrow-navigated into
    /// the completions list. Arrow-nav rewrites `input` to
    /// `base_input_dir_prefix(base_input) + selected.name`; cancelling
    /// via further typing resets back to this prefix. `request_completions`
    /// refreshes this to match the current `input` on every edit.
    pub base_input: String,

    /// Current completion rows, driver-sorted (dirs first, alpha).
    pub completions: Vec<FindFileEntry>,

    /// Arrow-driven selection. `None` = no active preview. Set on
    /// `MoveUp` / `MoveDown`, cleared on any input edit.
    pub selected: Option<usize>,

    /// Whether the completions list should be visible in the side
    /// panel. Set by `Tab`, `MoveUp` / `MoveDown`; cleared on edit.
    pub show_side: bool,

    /// Queue of pending completion requests. Dispatch pushes one
    /// `FindFileCmd` per activation / input edit; the main loop
    /// drains + ships them to the driver + clears in order.
    ///
    /// Using a queue (rather than a single-slot bit) lets us emit
    /// one `FsFindFile` trace line per keystroke when the runtime
    /// batches multiple input events per tick — matches legacy's
    /// dispatched.snap. Out-of-order completion arrivals at the
    /// runtime are dropped by `(dir, prefix)` match against the
    /// overlay's current input, so mid-tick re-fires don't paint
    /// stale data.
    pub pending_find_file_list: Vec<led_driver_find_file_core::FindFileCmd>,

    /// Captured on the first arrow-driven preview: the tab id that
    /// was active before the overlay started previewing
    /// completions. Deactivate uses it to restore focus after
    /// closing a preview tab. `None` means either the overlay has
    /// not yet previewed anything, or there was no active tab to
    /// preserve.
    pub previous_tab: Option<led_state_tabs::TabId>,
}

impl FindFileState {
    /// Build an Open-mode overlay with a pre-computed initial input.
    /// Caller is responsible for picking the input (parent of the
    /// active buffer with a trailing `/`, or `start_dir` / `/` if no
    /// active buffer — see `compute_activate_open` below).
    pub fn open(input: String) -> Self {
        let cursor = input.len();
        Self {
            mode: FindFileMode::Open,
            input: input.clone(),
            cursor,
            base_input: input,
            completions: Vec::new(),
            selected: None,
            show_side: false,
            pending_find_file_list: Vec::new(),
            previous_tab: None,
        }
    }

    /// Build a SaveAs-mode overlay. Input seeding is the caller's
    /// responsibility — typically the active buffer's full path so
    /// the user can edit the file name in place.
    pub fn save_as(input: String) -> Self {
        let cursor = input.len();
        Self {
            mode: FindFileMode::SaveAs,
            input: input.clone(),
            cursor,
            base_input: input,
            completions: Vec::new(),
            selected: None,
            show_side: false,
            pending_find_file_list: Vec::new(),
            previous_tab: None,
        }
    }

    /// Selected entry, if any. Out-of-range indices return `None` —
    /// defensive against races where completions refresh between a
    /// `MoveDown` and the selection read.
    pub fn selected_entry(&self) -> Option<&FindFileEntry> {
        self.selected.and_then(|i| self.completions.get(i))
    }

    /// Drop the overlay-side derived state that shouldn't outlive
    /// an input edit. Called from every input-changing action so a
    /// stale selection doesn't linger past a character delete.
    pub fn reset_selection(&mut self) {
        self.selected = None;
        self.show_side = false;
    }

    /// Queue a completion request derived from the current input.
    /// Dispatch calls this after every edit so the runtime's next
    /// `execute` ships one `FsFindFile` per change.
    ///
    /// Input → (dir, prefix) mapping:
    /// - Ends with `/`: directory is the expanded path itself, prefix
    ///   empty (exploring the directory).
    /// - Contains `/` but doesn't end with one: directory is the
    ///   leaf's parent; prefix is the leaf.
    /// - No `/` / empty: directory falls back to `/` — legacy's
    ///   "no parent" branch of `expected_dir`.
    ///
    /// `show_hidden` flips on when the leaf prefix starts with `.`.
    ///
    /// Also re-baselines `base_input` to the current input. `base_input`
    /// is the query prefix arrow-nav forms each rewritten input from
    /// via `dir_prefix(base_input) + entry.name`; it must track any
    /// input change that represents a new query — edits, Tab-descent,
    /// LCP extension — so post-descent arrow-nav lists the new
    /// directory, not the pre-descent one. Matches legacy's
    /// `request_completions` (where `base_input = input.clone()`
    /// fires on every input change).
    pub fn queue_request(&mut self) {
        self.base_input = self.input.clone();
        let (dir_part, prefix) = split_input(&self.input);
        // When no dir segment is present, the listing target is `/` —
        // matches legacy's `expected_dir` fallback for empty /
        // slash-less inputs.
        let expanded = if dir_part.is_empty() {
            PathBuf::from("/")
        } else {
            expand_path(dir_part)
        };
        let dir = led_core::UserPath::new(expanded).canonicalize();
        let show_hidden = prefix.starts_with('.');
        self.pending_find_file_list
            .push(led_driver_find_file_core::FindFileCmd {
                dir,
                prefix: prefix.to_string(),
                show_hidden,
            });
    }
}

/// Split `input` into `(dir_prefix, leaf_prefix)` for driver requests.
///
/// - If `input` ends with `/`, the whole thing is the directory and
///   the leaf prefix is empty (user is exploring the directory).
/// - Otherwise the last `/`-separated segment is the prefix.
/// - Returns `None` if `input` is empty.
///
/// The rewrite always keeps `input` in "user form" (may contain `~`);
/// callers run `expand_path` before handing the dir to the driver.
pub fn split_input(input: &str) -> (&str, &str) {
    if input.is_empty() {
        return ("", "");
    }
    if input.ends_with('/') {
        return (input, "");
    }
    match input.rfind('/') {
        Some(i) => (&input[..=i], &input[i + 1..]),
        None => ("", input),
    }
}

/// `dir_prefix` portion of `base_input` — used when arrow nav
/// rewrites `input` to `dir_prefix + selected.name`.
pub fn dir_prefix(input: &str) -> &str {
    split_input(input).0
}

/// Expand `~` (replaces with `$HOME`) and lexically collapse `.` /
/// `..` components. No filesystem I/O — canonicalization (symlink
/// resolution) is a separate step at the driver boundary.
///
/// This is a direct port of legacy `find_file::expand_path`. When
/// `HOME` is unset and the input starts with `~`, the tilde is left
/// as-is (preserves the original legacy fallback behaviour).
pub fn expand_path(input: &str) -> PathBuf {
    let expanded = if let Some(rest) = input.strip_prefix('~') {
        if let Some(home) = dirs::home_dir() {
            home.join(rest.trim_start_matches('/'))
                .to_string_lossy()
                .into_owned()
        } else {
            input.to_string()
        }
    } else {
        input.to_string()
    };

    let path = Path::new(&expanded);
    let mut result = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                result.pop();
            }
            other => result.push(other),
        }
    }
    result
}

/// Replace a leading `$HOME` in a path string with `~`.
///
/// Display-only: the overlay always shows paths in abbreviated form
/// so the status bar stays readable when the active file lives deep
/// in the user's home tree.
pub fn abbreviate_home(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy();
        if path.starts_with(home.as_ref()) {
            return format!("~{}", &path[home.len()..]);
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_input_trailing_slash_dir_only() {
        assert_eq!(split_input("~/foo/"), ("~/foo/", ""));
        assert_eq!(split_input("/"), ("/", ""));
    }

    #[test]
    fn split_input_no_slash_is_leaf_only() {
        assert_eq!(split_input("main.rs"), ("", "main.rs"));
    }

    #[test]
    fn split_input_mixed_path_splits_at_last_slash() {
        assert_eq!(split_input("src/main"), ("src/", "main"));
        assert_eq!(split_input("~/dev/led-rewrite/Cargo"), ("~/dev/led-rewrite/", "Cargo"));
    }

    #[test]
    fn split_input_empty_is_empty() {
        assert_eq!(split_input(""), ("", ""));
    }

    #[test]
    fn open_and_save_as_constructors_position_cursor_at_end() {
        let s = FindFileState::open("~/src/".into());
        assert_eq!(s.cursor, 6);
        assert_eq!(s.mode, FindFileMode::Open);

        let s = FindFileState::save_as("~/src/main.rs".into());
        assert_eq!(s.cursor, 13);
        assert_eq!(s.mode, FindFileMode::SaveAs);
    }

    #[test]
    fn reset_selection_clears_selected_and_side() {
        let mut s = FindFileState::open("~/src/".into());
        s.selected = Some(2);
        s.show_side = true;
        s.reset_selection();
        assert_eq!(s.selected, None);
        assert!(!s.show_side);
    }
}
