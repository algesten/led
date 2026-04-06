use led_core::{Col, Doc, Row};
use led_state::{BufferState, ISearchState};

/// Scan doc line by line for case-insensitive substring matches.
/// Returns (Row, Col, char_len) triples.
pub fn find_all_matches(doc: &dyn Doc, query: &str) -> Vec<(Row, Col, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    let query_lower: String = query.to_lowercase();
    let total = doc.line_count();
    let mut results = Vec::new();
    led_core::with_line_buf(|line| {
        for row in 0..total {
            doc.line(Row(row), line);
            let trimmed = line.trim_end_matches(&['\n', '\r'][..]).len();
            line.truncate(trimmed);
            let line_lower = line.to_lowercase();
            let mut start = 0;
            while let Some(pos) = line_lower[start..].find(&query_lower) {
                let byte_offset = start + pos;
                let col = line[..byte_offset].chars().count();
                let char_len = query.chars().count();
                results.push((Row(row), Col(col), char_len));
                // Advance past this match by at least one char
                start = byte_offset
                    + line_lower[byte_offset..]
                        .chars()
                        .next()
                        .map_or(1, |c| c.len_utf8());
            }
        }
    });
    results
}

/// Find index of first match at or after (row, col).
fn first_match_from(matches: &[(Row, Col, usize)], row: Row, col: Col) -> Option<usize> {
    matches
        .iter()
        .position(|&(mr, mc, _)| mr > row || (mr == row && mc >= col))
}

/// Begin incremental search, saving current cursor+scroll as origin.
/// If a mark is active, seed the query with the selected text.
pub fn start_search(buf: &mut BufferState) {
    let selected = super::edit::selected_text(buf);
    buf.clear_mark();

    let query = selected.unwrap_or_default();

    buf.isearch = Some(ISearchState {
        query: query.clone(),
        origin: (buf.cursor_row(), buf.cursor_col()),
        origin_scroll: buf.scroll_row(),
        origin_sub_line: buf.scroll_sub_line(),
        failed: false,
        matches: Vec::new(),
        match_idx: None,
    });

    if !query.is_empty() {
        update_search(buf);
    }
}

/// Recompute matches from query, jump cursor to first match at or after current position.
/// No forward match -> failed state (no wrap).
pub fn update_search(buf: &mut BufferState) {
    let query = match buf.isearch.as_ref() {
        Some(s) => s.query.clone(),
        None => return,
    };
    let matches = find_all_matches(&**buf.doc(), &query);
    let (row, col) = (buf.cursor_row(), buf.cursor_col());

    let (match_idx, failed) = if query.is_empty() {
        (None, false)
    } else if matches.is_empty() {
        (None, true)
    } else if let Some(idx) = first_match_from(&matches, row, col) {
        (Some(idx), false)
    } else {
        // Matches exist but all before cursor
        (None, true)
    };

    if let Some(idx) = match_idx {
        let (r, c, _) = matches[idx];
        buf.set_cursor(r, c, c);
    }

    let isearch = buf.isearch.as_mut().unwrap();
    isearch.matches = matches;
    isearch.match_idx = match_idx;
    isearch.failed = failed;
}

/// Advance to next match. If query is empty, recall last search.
/// If failed (no forward hit), wrap to first match.
pub fn search_next(buf: &mut BufferState) {
    let query_empty = buf.isearch.as_ref().map_or(true, |s| s.query.is_empty());
    if query_empty {
        if let Some(ref last) = buf.last_search {
            let last = last.clone();
            if let Some(is) = buf.isearch.as_mut() {
                is.query = last;
            }
            update_search(buf);
            return;
        }
        return;
    }

    let has_matches = buf
        .isearch
        .as_ref()
        .map_or(false, |s| !s.matches.is_empty());
    if !has_matches {
        return;
    }

    let failed = buf.isearch.as_ref().unwrap().failed;
    if failed {
        // Wrap to first match
        let (row, col, _) = buf.isearch.as_ref().unwrap().matches[0];
        buf.set_cursor(row, col, col);
        let isearch = buf.isearch.as_mut().unwrap();
        isearch.match_idx = Some(0);
        isearch.failed = false;
        return;
    }

    let match_idx = buf.isearch.as_ref().unwrap().match_idx;
    if let Some(idx) = match_idx {
        let next = idx + 1;
        let len = buf.isearch.as_ref().unwrap().matches.len();
        if next < len {
            let (row, col, _) = buf.isearch.as_ref().unwrap().matches[next];
            buf.set_cursor(row, col, col);
            let isearch = buf.isearch.as_mut().unwrap();
            isearch.match_idx = Some(next);
        } else {
            // Past last match -> enter failed state
            let isearch = buf.isearch.as_mut().unwrap();
            isearch.failed = true;
        }
    }
}

/// Save query to last_search if non-empty.
fn save_last_search(buf: &mut BufferState) {
    if let Some(ref is) = buf.isearch {
        if !is.query.is_empty() {
            buf.last_search = Some(is.query.clone());
        }
    }
}

/// Cancel search — restore origin cursor/scroll, save query.
pub fn search_cancel(buf: &mut BufferState) {
    save_last_search(buf);
    if let Some(isearch) = buf.isearch.take() {
        buf.set_cursor(isearch.origin.0, isearch.origin.1, isearch.origin.1);
        buf.set_scroll(isearch.origin_scroll, isearch.origin_sub_line);
    }
}

/// Accept search — keep cursor position, save query, end search.
pub fn search_accept(buf: &mut BufferState) {
    save_last_search(buf);
    buf.isearch = None;
}
