use crate::{Buffer, ISearchState};
use led_core::TextDoc;

impl Buffer {
    /// Scan rope line by line for case-insensitive substring matches.
    /// Returns (row, col, char_len) triples.
    pub(crate) fn find_all_matches(
        &self,
        doc: &TextDoc,
        query: &str,
    ) -> Vec<(usize, usize, usize)> {
        if query.is_empty() {
            return Vec::new();
        }
        let query_lower: String = query.to_lowercase();
        let total = doc.line_count();
        let mut results = Vec::new();
        for row in 0..total {
            let line = doc.line(row);
            let line_lower = line.to_lowercase();
            let mut start = 0;
            while let Some(pos) = line_lower[start..].find(&query_lower) {
                let byte_offset = start + pos;
                // Convert byte offset to char col
                let col = line[..byte_offset].chars().count();
                let char_len = query.chars().count();
                results.push((row, col, char_len));
                // Advance past this match (by at least one byte)
                start = byte_offset
                    + line_lower[byte_offset..]
                        .chars()
                        .next()
                        .map_or(1, |c| c.len_utf8());
            }
        }
        results
    }

    /// Begin incremental search.
    pub(crate) fn start_search(&mut self) {
        self.isearch = Some(ISearchState {
            query: String::new(),
            origin: (self.cursor_row, self.cursor_col),
            origin_scroll: self.scroll_offset,
            origin_sub_line: self.scroll_sub_line,
            failed: false,
            matches: Vec::new(),
            match_idx: None,
        });
    }

    /// Find index of first match at or after (row, col), or None.
    fn first_match_from(
        matches: &[(usize, usize, usize)],
        row: usize,
        col: usize,
    ) -> Option<usize> {
        matches
            .iter()
            .position(|&(mr, mc, _)| mr > row || (mr == row && mc >= col))
    }

    /// Recalculate matches and jump cursor to first match at or after current position.
    /// If no forward match exists, enter failed state (don't wrap).
    pub(crate) fn update_search(&mut self, doc: &TextDoc) {
        let query = match self.isearch.as_ref() {
            Some(s) => s.query.clone(),
            None => return,
        };
        let matches = self.find_all_matches(doc, &query);
        let pos = (self.cursor_row, self.cursor_col);

        let (match_idx, failed) = if query.is_empty() {
            (None, false)
        } else if matches.is_empty() {
            (None, true)
        } else if let Some(idx) = Self::first_match_from(&matches, pos.0, pos.1) {
            (Some(idx), false)
        } else {
            // Matches exist but all before cursor — failed
            (None, true)
        };

        if let Some(idx) = match_idx {
            let (row, col, _) = matches[idx];
            self.cursor_row = row;
            self.cursor_col = col;
        }

        let isearch = self.isearch.as_mut().unwrap();
        isearch.matches = matches;
        isearch.match_idx = match_idx;
        isearch.failed = failed;
    }

    /// Advance to next match. If query is empty, recall last search.
    /// If failed (no forward hit), wrap to first match.
    pub(crate) fn search_next(&mut self, doc: &TextDoc) {
        // Recall last search query when current query is empty (Emacs C-s C-s)
        let query_empty = self.isearch.as_ref().map_or(true, |s| s.query.is_empty());
        if query_empty {
            if let Some(ref last) = self.last_search {
                let last = last.clone();
                if let Some(is) = self.isearch.as_mut() {
                    is.query = last;
                }
                self.update_search(doc);
                return;
            }
            return;
        }

        let isearch = match self.isearch.as_ref() {
            Some(s) if !s.matches.is_empty() => s,
            _ => return,
        };

        if isearch.failed {
            // Wrap to first match
            let (row, col, _) = isearch.matches[0];
            self.cursor_row = row;
            self.cursor_col = col;
            let isearch = self.isearch.as_mut().unwrap();
            isearch.match_idx = Some(0);
            isearch.failed = false;
            return;
        }

        if let Some(idx) = isearch.match_idx {
            let next = idx + 1;
            if next < isearch.matches.len() {
                let (row, col, _) = isearch.matches[next];
                self.cursor_row = row;
                self.cursor_col = col;
                let isearch = self.isearch.as_mut().unwrap();
                isearch.match_idx = Some(next);
            } else {
                // Past last match — enter failed state
                let isearch = self.isearch.as_mut().unwrap();
                isearch.failed = true;
            }
        }
    }

    /// Save query to last_search if non-empty.
    fn save_last_search(&mut self) {
        if let Some(ref is) = self.isearch {
            if !is.query.is_empty() {
                self.last_search = Some(is.query.clone());
            }
        }
    }

    /// Cancel search — restore origin cursor/scroll.
    pub(crate) fn search_cancel(&mut self) {
        self.save_last_search();
        if let Some(isearch) = self.isearch.take() {
            self.cursor_row = isearch.origin.0;
            self.cursor_col = isearch.origin.1;
            self.scroll_offset = isearch.origin_scroll;
            self.scroll_sub_line = isearch.origin_sub_line;
        }
    }

    /// Accept search — keep cursor position, end search.
    pub(crate) fn search_accept(&mut self) {
        self.save_last_search();
        self.isearch = None;
    }
}
