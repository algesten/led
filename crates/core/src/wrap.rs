/// Count display width of a line (tabs expand to 4 columns) without allocating.
pub fn line_display_width(line: &str) -> usize {
    line.chars().map(|ch| if ch == '\t' { 4 } else { 1 }).sum()
}

/// Expand tabs to 4 spaces, returning display chars and char-index-to-display-column map.
/// The map has len = num_source_chars + 1 (sentinel at end == display.len()).
/// Trailing `\n`/`\r` from rope lines are skipped.
pub fn expand_tabs(line: &str) -> (Vec<char>, Vec<usize>) {
    let mut display: Vec<char> = Vec::with_capacity(line.len());
    let mut char_map = Vec::with_capacity(line.len() + 1);
    for ch in line.chars() {
        if ch == '\n' || ch == '\r' {
            continue;
        }
        char_map.push(display.len());
        if ch == '\t' {
            display.extend([' ', ' ', ' ', ' ']);
        } else {
            display.push(ch);
        }
    }
    char_map.push(display.len());
    (display, char_map)
}

/// Collect a slice of chars into a String.
pub fn chars_to_string(chars: &[char]) -> String {
    chars.iter().collect()
}

/// How many screen rows a line of `display_width` occupies at the given `text_width`.
pub fn visual_line_count(display_width: usize, text_width: usize) -> usize {
    if text_width <= 1 || display_width <= text_width {
        return 1;
    }
    let wrap_width = text_width - 1;
    let mut count = 0;
    let mut remaining = display_width;
    while remaining > text_width {
        count += 1;
        remaining -= wrap_width;
    }
    count + 1
}

/// Split a line into (start, end) display-column char ranges per visual line.
/// Non-last chunks have `wrap_width = text_width - 1` content columns (room for `\`).
pub fn compute_chunks(display_width: usize, text_width: usize) -> Vec<(usize, usize)> {
    if text_width <= 1 || display_width <= text_width {
        return vec![(0, display_width)];
    }
    let wrap_width = text_width - 1;
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < display_width {
        let remaining = display_width - start;
        if remaining <= text_width {
            chunks.push((start, display_width));
            break;
        }
        chunks.push((start, start + wrap_width));
        start += wrap_width;
    }
    chunks
}

/// Find which sub-line (chunk index) contains display column `dcol`.
pub fn find_sub_line(chunks: &[(usize, usize)], dcol: usize) -> usize {
    for (i, &(_cs, ce)) in chunks.iter().enumerate() {
        if dcol < ce || i == chunks.len() - 1 {
            return i;
        }
    }
    0
}

/// Reverse map from display column to logical char index.
pub fn display_col_to_char_idx(char_map: &[usize], target_dcol: usize) -> usize {
    let num_chars = char_map.len().saturating_sub(1);
    if num_chars > 0 && target_dcol >= char_map[num_chars] {
        return num_chars;
    }
    for i in (0..num_chars).rev() {
        if char_map[i] <= target_dcol {
            return i;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_display_width_basic() {
        assert_eq!(line_display_width("abc"), 3);
        assert_eq!(line_display_width("a\tb"), 6); // 1 + 4 + 1
        assert_eq!(line_display_width(""), 0);
        assert_eq!(line_display_width("\t\t"), 8);
    }

    #[test]
    fn expand_tabs_no_tabs() {
        let (display, char_map) = expand_tabs("abc");
        assert_eq!(chars_to_string(&display), "abc");
        assert_eq!(char_map, vec![0, 1, 2, 3]);
    }

    #[test]
    fn expand_tabs_with_tab() {
        let (display, char_map) = expand_tabs("a\tb");
        assert_eq!(chars_to_string(&display), "a    b");
        assert_eq!(char_map, vec![0, 1, 5, 6]);
    }

    #[test]
    fn expand_tabs_empty() {
        let (display, char_map) = expand_tabs("");
        assert!(display.is_empty());
        assert_eq!(char_map, vec![0]);
    }

    #[test]
    fn visual_line_count_no_wrap() {
        assert_eq!(visual_line_count(10, 20), 1);
        assert_eq!(visual_line_count(20, 20), 1);
    }

    #[test]
    fn visual_line_count_wraps() {
        // text_width=10, wrap_width=9
        // 19 chars: first chunk 9, remaining 10 fits in text_width
        assert_eq!(visual_line_count(19, 10), 2);
        // 20 chars: first chunk 9, remaining 11 > text_width, so 9 + 2 = 3 chunks
        assert_eq!(visual_line_count(20, 10), 3);
    }

    #[test]
    fn visual_line_count_edge() {
        assert_eq!(visual_line_count(0, 10), 1);
        assert_eq!(visual_line_count(5, 0), 1);
        assert_eq!(visual_line_count(5, 1), 1);
    }

    #[test]
    fn compute_chunks_no_wrap() {
        assert_eq!(compute_chunks(5, 10), vec![(0, 5)]);
        assert_eq!(compute_chunks(10, 10), vec![(0, 10)]);
    }

    #[test]
    fn compute_chunks_wraps() {
        // text_width=10, wrap_width=9
        // 19 chars: [0..9, 9..19]
        assert_eq!(compute_chunks(19, 10), vec![(0, 9), (9, 19)]);
    }

    #[test]
    fn compute_chunks_empty() {
        assert_eq!(compute_chunks(0, 10), vec![(0, 0)]);
    }

    #[test]
    fn find_sub_line_first() {
        let chunks = vec![(0, 9), (9, 19)];
        assert_eq!(find_sub_line(&chunks, 0), 0);
        assert_eq!(find_sub_line(&chunks, 8), 0);
    }

    #[test]
    fn find_sub_line_second() {
        let chunks = vec![(0, 9), (9, 19)];
        assert_eq!(find_sub_line(&chunks, 9), 1);
        assert_eq!(find_sub_line(&chunks, 18), 1);
    }

    #[test]
    fn find_sub_line_beyond_clamps_to_last() {
        let chunks = vec![(0, 9), (9, 19)];
        assert_eq!(find_sub_line(&chunks, 100), 1);
    }

    #[test]
    fn display_col_to_char_idx_basic() {
        // "abc" → char_map = [0, 1, 2, 3]
        let char_map = vec![0, 1, 2, 3];
        assert_eq!(display_col_to_char_idx(&char_map, 0), 0);
        assert_eq!(display_col_to_char_idx(&char_map, 1), 1);
        assert_eq!(display_col_to_char_idx(&char_map, 2), 2);
        assert_eq!(display_col_to_char_idx(&char_map, 3), 3);
    }

    #[test]
    fn display_col_to_char_idx_with_tab() {
        // "a\tb" → char_map = [0, 1, 5, 6], display "a    b"
        let char_map = vec![0, 1, 5, 6];
        assert_eq!(display_col_to_char_idx(&char_map, 0), 0);
        assert_eq!(display_col_to_char_idx(&char_map, 1), 1);
        assert_eq!(display_col_to_char_idx(&char_map, 3), 1); // inside tab expansion
        assert_eq!(display_col_to_char_idx(&char_map, 5), 2);
    }

    #[test]
    fn display_col_to_char_idx_beyond_end() {
        let char_map = vec![0, 1, 2, 3];
        assert_eq!(display_col_to_char_idx(&char_map, 100), 3);
    }
}
