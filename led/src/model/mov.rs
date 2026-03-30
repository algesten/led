use led_core::wrap::{
    compute_chunks, display_col_to_char_idx, expand_tabs, find_sub_line, visual_line_count,
};
use led_core::{Doc, DocVersion, Row};

use led_state::{AppState, BufferState, Dimensions};

// ── Scroll ──

/// Adjust scroll so the cursor is visible with scroll margin, accounting for line wrapping.
pub fn adjust_scroll(buf: &BufferState, dims: &Dimensions) -> (usize, usize) {
    let text_width = dims.text_width();
    let height = dims.buffer_height();

    if height == 0 || text_width == 0 {
        return (buf.scroll_row().0, buf.scroll_sub_line().0);
    }

    let margin = dims.scroll_margin.min(height / 2);
    let total = buf.doc().line_count();

    // Clamp scroll to valid range
    let mut sr = buf.scroll_row().0;
    let mut ssl = buf.scroll_sub_line().0;
    if sr >= total {
        sr = total.saturating_sub(1);
        ssl = 0;
    }
    let scroll_vl = visual_line_count(expand_tabs(&buf.doc().line(Row(sr))).0.len(), text_width);
    if ssl >= scroll_vl {
        ssl = scroll_vl.saturating_sub(1);
    }

    // Compute cursor's sub-line within its logical line
    let (cursor_sub, _cursor_vrow_count) = cursor_visual_position(
        &**buf.doc(),
        buf.cursor_row().0,
        buf.cursor_col().0,
        text_width,
    );

    // Compute cursor's visual row relative to scroll position
    let cursor_vrow = compute_cursor_vrow(
        &**buf.doc(),
        buf.cursor_row().0,
        cursor_sub,
        sr,
        ssl,
        text_width,
    );

    // Case 1: cursor too close to top — scroll up
    if let Some(vrow) = cursor_vrow {
        if vrow < margin {
            return scroll_to_place_cursor_at_vrow(
                &**buf.doc(),
                buf.cursor_row().0,
                cursor_sub,
                margin,
                text_width,
            );
        }
        // Case 2: cursor too close to bottom — scroll down
        if vrow >= height.saturating_sub(margin) {
            let target_vrow = height.saturating_sub(margin + 1);
            return scroll_to_place_cursor_at_vrow(
                &**buf.doc(),
                buf.cursor_row().0,
                cursor_sub,
                target_vrow,
                text_width,
            );
        }
        // Cursor is comfortably visible
        return (sr, ssl);
    }

    // Cursor is not in the visible range at all
    if buf.cursor_row().0 < sr || (buf.cursor_row().0 == sr && cursor_sub < ssl) {
        // Cursor above viewport — place at margin from top
        return scroll_to_place_cursor_at_vrow(
            &**buf.doc(),
            buf.cursor_row().0,
            cursor_sub,
            margin,
            text_width,
        );
    }

    // Cursor below viewport — place at margin from bottom
    let target_vrow = height.saturating_sub(margin + 1);
    scroll_to_place_cursor_at_vrow(
        &**buf.doc(),
        buf.cursor_row().0,
        cursor_sub,
        target_vrow,
        text_width,
    )
}

/// Compute cursor's visual row relative to the scroll position.
/// Returns None if cursor is outside the reasonable scan range.
fn compute_cursor_vrow(
    doc: &dyn Doc,
    cursor_row: usize,
    cursor_sub: usize,
    sr: usize,
    ssl: usize,
    text_width: usize,
) -> Option<usize> {
    if cursor_row < sr || (cursor_row == sr && cursor_sub < ssl) {
        return None;
    }

    let mut vrow: usize = 0;

    if cursor_row == sr {
        return Some(cursor_sub - ssl);
    }

    // First logical line: only count sub-lines from scroll_sub_line onward
    let scroll_vl = visual_line_count(expand_tabs(&doc.line(Row(sr))).0.len(), text_width);
    vrow += scroll_vl - ssl;

    // Intermediate lines
    for li in (sr + 1)..cursor_row {
        vrow += visual_line_count(expand_tabs(&doc.line(Row(li))).0.len(), text_width);
        if vrow > 10000 {
            return None; // Don't scan too far
        }
    }

    Some(vrow + cursor_sub)
}

/// Compute scroll position that places cursor at a specific visual row from the top.
fn scroll_to_place_cursor_at_vrow(
    doc: &dyn Doc,
    cursor_row: usize,
    cursor_sub: usize,
    target_vrow: usize,
    text_width: usize,
) -> (usize, usize) {
    let mut remaining = target_vrow;

    // First, consume sub-lines above cursor within same logical line
    if cursor_sub <= remaining {
        remaining -= cursor_sub;
    } else {
        // Cursor's own line is taller than target_vrow at cursor's sub-line
        return (cursor_row, cursor_sub - target_vrow);
    }

    let mut new_scroll = cursor_row;
    let mut new_sub: usize = 0;

    for li in (0..cursor_row).rev() {
        if remaining == 0 {
            break;
        }
        let vl = visual_line_count(expand_tabs(&doc.line(Row(li))).0.len(), text_width);
        if vl <= remaining {
            remaining -= vl;
            new_scroll = li;
            new_sub = 0;
        } else {
            new_scroll = li;
            new_sub = vl - remaining;
            break;
        }
    }

    (new_scroll, new_sub)
}

fn cursor_visual_position(
    doc: &dyn Doc,
    cursor_row: usize,
    cursor_col: usize,
    text_width: usize,
) -> (usize, usize) {
    let (cursor_display, cursor_cm) = expand_tabs(&doc.line(Row(cursor_row)));
    let cursor_dc = cursor_cm
        .get(cursor_col)
        .copied()
        .unwrap_or_else(|| cursor_cm.last().copied().unwrap_or(0));
    let cursor_chunks = compute_chunks(cursor_display.len(), text_width);
    let cursor_sub = find_sub_line(&cursor_chunks, cursor_dc);
    let cursor_vrow_count = cursor_chunks.len();
    (cursor_sub, cursor_vrow_count)
}

// ── Visual column helpers ──

/// Compute the visual column (display column within current sub-line chunk).
fn visual_col_of(doc: &dyn Doc, row: usize, col: usize, text_width: usize) -> usize {
    let (display, char_map) = expand_tabs(&doc.line(Row(row)));
    let dcol = char_map
        .get(col)
        .copied()
        .unwrap_or_else(|| char_map.last().copied().unwrap_or(0));
    let chunks = compute_chunks(display.len(), text_width);
    let sub = find_sub_line(&chunks, dcol);
    dcol - chunks[sub].0
}

/// Reset affinity to current visual column.
pub fn reset_affinity(buf: &BufferState, dims: &Dimensions) -> usize {
    visual_col_of(
        &**buf.doc(),
        buf.cursor_row().0,
        buf.cursor_col().0,
        dims.text_width(),
    )
}

// ── Movement ──

pub fn move_up(buf: &BufferState, dims: &Dimensions) -> (usize, usize, usize) {
    let tw = dims.text_width();
    let (row, col) = compute_move_up(
        buf.cursor_row().0,
        buf.cursor_col().0,
        buf.cursor_col_affinity().0,
        tw,
        &**buf.doc(),
    );
    let len = buf.doc().line_len(Row(row));
    let col = col.min(len);
    (row, col, buf.cursor_col_affinity().0)
}

pub fn move_down(buf: &BufferState, dims: &Dimensions) -> (usize, usize, usize) {
    let tw = dims.text_width();
    let (row, col) = compute_move_down(
        buf.cursor_row().0,
        buf.cursor_col().0,
        buf.cursor_col_affinity().0,
        tw,
        &**buf.doc(),
    );
    let len = buf.doc().line_len(Row(row));
    let col = col.min(len);
    (row, col, buf.cursor_col_affinity().0)
}

pub fn move_left(buf: &BufferState) -> (usize, usize, usize) {
    if buf.cursor_col().0 > 0 {
        let col = buf.cursor_col().0 - 1;
        (buf.cursor_row().0, col, col)
    } else if buf.cursor_row().0 > 0 {
        let row = buf.cursor_row().0 - 1;
        let col = buf.doc().line_len(Row(row));
        (row, col, col)
    } else {
        (0, 0, 0)
    }
}

pub fn move_right(buf: &BufferState) -> (usize, usize, usize) {
    let len = buf.doc().line_len(buf.cursor_row());
    if buf.cursor_col().0 < len {
        let col = buf.cursor_col().0 + 1;
        (buf.cursor_row().0, col, col)
    } else if buf.cursor_row().0 < buf.doc().line_count().saturating_sub(1) {
        let row = buf.cursor_row().0 + 1;
        (row, 0, 0)
    } else {
        (buf.cursor_row().0, len, len)
    }
}

pub fn line_start(buf: &BufferState) -> (usize, usize, usize) {
    (buf.cursor_row().0, 0, 0)
}

pub fn line_end(buf: &BufferState) -> (usize, usize, usize) {
    let col = buf.doc().line_len(buf.cursor_row());
    (buf.cursor_row().0, col, col)
}

pub fn page_up(buf: &BufferState, dims: &Dimensions) -> (usize, usize, usize) {
    let height = dims.buffer_height();
    let page = height.saturating_sub(1).max(1);
    let row = buf.cursor_row().0.saturating_sub(page);
    let len = buf.doc().line_len(Row(row));
    let col = buf.cursor_col_affinity().0.min(len);
    (row, col, buf.cursor_col_affinity().0)
}

pub fn page_down(buf: &BufferState, dims: &Dimensions) -> (usize, usize, usize) {
    let height = dims.buffer_height();
    let page = height.saturating_sub(1).max(1);
    let max_row = buf.doc().line_count().saturating_sub(1);
    let row = (buf.cursor_row().0 + page).min(max_row);
    let len = buf.doc().line_len(Row(row));
    let col = buf.cursor_col_affinity().0.min(len);
    (row, col, buf.cursor_col_affinity().0)
}

pub fn file_start() -> (usize, usize, usize) {
    (0, 0, 0)
}

pub fn file_end(doc: &dyn Doc) -> (usize, usize, usize) {
    let row = doc.line_count().saturating_sub(1);
    let col = doc.line_len(Row(row));
    (row, col, col)
}

// ── Wrap-aware vertical movement ──

fn compute_move_up(
    cursor_row: usize,
    cursor_col: usize,
    visual_col_affinity: usize,
    tw: usize,
    doc: &dyn Doc,
) -> (usize, usize) {
    if tw == 0 {
        if cursor_row > 0 {
            return (cursor_row - 1, cursor_col);
        }
        return (cursor_row, cursor_col);
    }

    let (display, char_map) = expand_tabs(&doc.line(Row(cursor_row)));
    let cursor_dcol = char_map
        .get(cursor_col)
        .copied()
        .unwrap_or_else(|| char_map.last().copied().unwrap_or(0));
    let chunks = compute_chunks(display.len(), tw);
    let sub = find_sub_line(&chunks, cursor_dcol);

    if sub > 0 {
        // chunks[sub - 1] is always a non-last chunk; clamp to ce - 1 so
        // we don't land on the boundary that find_sub_line assigns to the
        // *next* chunk (which would keep the cursor on the same visual row).
        let (cs, ce) = chunks[sub - 1];
        let target_dcol = cs + visual_col_affinity.min((ce - cs).saturating_sub(1));
        let col = display_col_to_char_idx(&char_map, target_dcol);
        (cursor_row, col)
    } else if cursor_row > 0 {
        let new_row = cursor_row - 1;
        let (prev_display, prev_cm) = expand_tabs(&doc.line(Row(new_row)));
        let prev_chunks = compute_chunks(prev_display.len(), tw);
        let (cs, ce) = *prev_chunks.last().unwrap();
        let target_dcol = cs + visual_col_affinity.min(ce - cs);
        let col = display_col_to_char_idx(&prev_cm, target_dcol);
        (new_row, col)
    } else {
        (cursor_row, cursor_col)
    }
}

fn compute_move_down(
    cursor_row: usize,
    cursor_col: usize,
    visual_col_affinity: usize,
    tw: usize,
    doc: &dyn Doc,
) -> (usize, usize) {
    if tw == 0 {
        if cursor_row + 1 < doc.line_count() {
            return (cursor_row + 1, cursor_col);
        }
        return (cursor_row, cursor_col);
    }

    let (display, char_map) = expand_tabs(&doc.line(Row(cursor_row)));
    let cursor_dcol = char_map
        .get(cursor_col)
        .copied()
        .unwrap_or_else(|| char_map.last().copied().unwrap_or(0));
    let chunks = compute_chunks(display.len(), tw);
    let sub = find_sub_line(&chunks, cursor_dcol);

    if sub + 1 < chunks.len() {
        let (cs, ce) = chunks[sub + 1];
        let is_last_chunk = sub + 1 == chunks.len() - 1;
        // For non-last chunks, clamp to ce - 1 to avoid landing on the
        // boundary that find_sub_line assigns to the next chunk.
        let max_offset = if is_last_chunk {
            ce - cs
        } else {
            (ce - cs).saturating_sub(1)
        };
        let target_dcol = cs + visual_col_affinity.min(max_offset);
        let col = display_col_to_char_idx(&char_map, target_dcol);
        (cursor_row, col)
    } else if cursor_row + 1 < doc.line_count() {
        let new_row = cursor_row + 1;
        let (next_display, next_cm) = expand_tabs(&doc.line(Row(new_row)));
        let next_chunks = compute_chunks(next_display.len(), tw);
        let (cs, ce) = next_chunks[0];
        // For a single-chunk line, allow the full range (end-of-line cursor).
        // For a multi-chunk line, chunk 0 is non-last; clamp to ce - 1.
        let max_offset = if next_chunks.len() == 1 {
            ce - cs
        } else {
            (ce - cs).saturating_sub(1)
        };
        let target_dcol = cs + visual_col_affinity.min(max_offset);
        let col = display_col_to_char_idx(&next_cm, target_dcol);
        (new_row, col)
    } else {
        (cursor_row, cursor_col)
    }
}

// ── Annotation shift ──

/// Shift cached syntax highlights and diagnostics when lines are inserted
/// or removed.  Pure coordinate shift — the respective drivers send fresh
/// data within one frame.
/// `old_doc_version` is the `doc.version()` before the action ran.
/// Pass it so we can detect whether the document actually changed.
pub fn shift_annotations(
    state: &mut AppState,
    buf_path: &std::path::Path,
    edit_row: usize,
    old_line_count: usize,
    old_doc_version: DocVersion,
) {
    if let Some(buf) = state.buf_mut(buf_path) {
        buf.shift_annotations(edit_row, old_line_count, old_doc_version);
    }
}
