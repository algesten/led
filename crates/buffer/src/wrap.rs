/// Expand tabs to 4 spaces, returning display chars and char-index-to-display-column map.
/// The map has len = num_source_chars + 1 (sentinel at end == display.len()).
pub(crate) fn expand_tabs(line: &str) -> (Vec<char>, Vec<usize>) {
    let mut display: Vec<char> = Vec::with_capacity(line.len());
    let mut char_map = Vec::with_capacity(line.len() + 1);
    for ch in line.chars() {
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
pub(crate) fn chars_to_string(chars: &[char]) -> String {
    chars.iter().collect()
}

/// How many screen rows a line of `display_width` occupies at the given `text_width`.
pub(crate) fn visual_line_count(display_width: usize, text_width: usize) -> usize {
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
pub(crate) fn compute_chunks(display_width: usize, text_width: usize) -> Vec<(usize, usize)> {
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
pub(crate) fn find_sub_line(chunks: &[(usize, usize)], dcol: usize) -> usize {
    for (i, &(_cs, ce)) in chunks.iter().enumerate() {
        if dcol < ce || i == chunks.len() - 1 {
            return i;
        }
    }
    0
}

/// Reverse map from display column to logical char index.
pub(crate) fn display_col_to_char_idx(char_map: &[usize], target_dcol: usize) -> usize {
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
