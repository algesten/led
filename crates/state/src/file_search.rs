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

#[derive(Debug, Clone)]
pub struct FileSearchState {
    pub query: String,
    pub cursor_pos: usize,
    pub case_sensitive: bool,
    pub use_regex: bool,
    pub results: Vec<FileGroup>,
    pub flat_hits: Vec<FlatHit>,
    pub selected: usize,
    pub scroll_offset: usize,
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
        if self.selected >= self.flat_hits.len() {
            self.selected = self.flat_hits.len().saturating_sub(1);
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
        let flat = self.flat_hits.get(self.selected)?;
        let group = &self.results[flat.group_idx];
        let hit = &group.hits[flat.hit_idx];
        Some((group, hit))
    }
}
