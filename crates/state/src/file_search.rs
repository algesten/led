use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub row: usize,
    pub col: usize,
    pub line_text: String,
    pub match_start: usize, // byte offset in line_text
    pub match_end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileGroup {
    pub path: PathBuf,
    pub relative: String,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlatHit {
    pub group_idx: usize,
    pub hit_idx: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileSearchRequest {
    pub query: String,
    pub root: PathBuf,
    pub case_sensitive: bool,
    pub use_regex: bool,
}

// ── Selection model ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSearchSelection {
    SearchInput,
    ReplaceInput,
    Result(usize), // index into flat_hits
}

// ── Replace undo tracking ──

#[derive(Debug, Clone)]
pub struct ReplaceEntry {
    pub flat_hit_idx: usize,
    pub path: PathBuf,
    pub row: usize,
    pub original_text: String,  // the matched text that was replaced
    pub match_start: usize,     // byte offset in line
    pub match_end: usize,       // byte offset in line
    pub replacement_len: usize, // byte length of replacement text
}

// ── Replace request (for driver) ──

#[derive(Debug, Clone, PartialEq)]
pub struct FileSearchReplaceRequest {
    pub query: String,
    pub replacement: String,
    pub root: PathBuf,
    pub case_sensitive: bool,
    pub use_regex: bool,
    pub scope: ReplaceScope,
    pub skip_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReplaceScope {
    Single {
        path: PathBuf,
        row: usize,
        match_start: usize,
        match_end: usize,
    },
    All,
}

// ── File search state ──

#[derive(Debug, Clone)]
pub struct FileSearchState {
    pub query: String,
    pub cursor_pos: usize,
    pub case_sensitive: bool,
    pub use_regex: bool,
    pub results: Vec<FileGroup>,
    pub flat_hits: Vec<FlatHit>,
    pub selection: FileSearchSelection,
    pub scroll_offset: usize,

    // Replace
    pub replace_mode: bool,
    pub replace_text: String,
    pub replace_cursor_pos: usize,
    pub replace_stack: Vec<ReplaceEntry>,
}

/// Pending replace-all state that outlives the file search panel.
/// Lives on AppState so it survives deactivate().
#[derive(Debug, Clone, Default)]
pub struct PendingReplaceAll {
    pub hits: HashMap<PathBuf, Vec<(usize, usize, usize, String)>>,
    pub replacement: String,
    pub query: String,
}

impl FileSearchState {
    pub fn rebuild_flat_hits(&mut self) {
        self.flat_hits.clear();
        for (gi, group) in self.results.iter().enumerate() {
            for (hi, _) in group.hits.iter().enumerate() {
                self.flat_hits.push(FlatHit {
                    group_idx: gi,
                    hit_idx: hi,
                });
            }
        }
        if let FileSearchSelection::Result(i) = self.selection {
            if i >= self.flat_hits.len() {
                self.selection = if self.flat_hits.is_empty() {
                    if self.replace_mode {
                        FileSearchSelection::ReplaceInput
                    } else {
                        FileSearchSelection::SearchInput
                    }
                } else {
                    FileSearchSelection::Result(self.flat_hits.len() - 1)
                };
            }
        }
    }

    /// Index into flat_hits when selection is on a result.
    pub fn selected_result_idx(&self) -> Option<usize> {
        match self.selection {
            FileSearchSelection::Result(i) => Some(i),
            _ => None,
        }
    }

    /// Convert a flat hit index to the display row it occupies,
    /// accounting for file header rows (one per group).
    pub fn flat_hit_to_row(&self, flat_idx: usize) -> usize {
        if flat_idx >= self.flat_hits.len() {
            return 0;
        }
        let target = &self.flat_hits[flat_idx];
        let mut row = 0;
        for (gi, group) in self.results.iter().enumerate() {
            row += 1; // file header
            if gi == target.group_idx {
                row += target.hit_idx;
                return row;
            }
            row += group.hits.len();
        }
        row
    }

    pub fn selected_hit(&self) -> Option<(&FileGroup, &SearchHit)> {
        let i = self.selected_result_idx()?;
        let flat = self.flat_hits.get(i)?;
        let group = &self.results[flat.group_idx];
        let hit = &group.hits[flat.hit_idx];
        Some((group, hit))
    }
}
