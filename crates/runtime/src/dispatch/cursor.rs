//! Cursor movement (M2, M6) — pure geometry over the rope.
//!
//! `apply_move` is the decision function; `move_cursor` is the
//! dispatch-facing wrapper that reads the right rope (edits first,
//! store second) and updates both cursor + scroll on the active tab.

use led_driver_buffers_core::{BufferStore, LoadState};
use led_driver_terminal_core::Terminal;
use led_state_buffer_edits::BufferEdits;
use led_state_tabs::{Cursor, Scroll, Tabs};
use ropey::Rope;
use std::sync::Arc;

use super::shared::{is_word_char, line_char_len, rope_char_at};

/// Logical cursor moves. Built from key events in `dispatch_key` and
/// applied by the pure [`apply_move`] helper so the geometry is unit
/// testable without any keyboard setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Move {
    Up,
    Down,
    Left,
    Right,
    LineStart,
    LineEnd,
    PageUp,
    PageDown,
    FileStart,
    FileEnd,
    WordLeft,
    WordRight,
}

/// Apply a move to the active tab: update cursor, then adjust scroll so
/// the cursor stays inside the body viewport. No-op when there is no
/// active tab or its buffer isn't loaded yet — the cursor has nothing
/// to clamp against.
///
/// Rope lookup prefers [`BufferEdits`] (the user's edited view); the
/// store fallback only matters before the runtime has seeded edits
/// for a just-loaded buffer.
pub(super) fn move_cursor(
    tabs: &mut Tabs,
    edits: &BufferEdits,
    store: &BufferStore,
    terminal: &Terminal,
    m: Move,
) {
    let Some(active) = tabs.active else {
        return;
    };
    let Some(idx) = tabs.open.iter().position(|t| t.id == active) else {
        return;
    };
    let path = &tabs.open[idx].path;
    let rope: Arc<Rope> = match edits.buffers.get(path) {
        Some(eb) => eb.rope.clone(),
        None => match store.loaded.get(path) {
            Some(LoadState::Ready(r)) => r.clone(),
            _ => return,
        },
    };

    let body_rows = terminal
        .dims
        .map(|d| d.rows.saturating_sub(1) as usize)
        .unwrap_or(0);

    let tab = &mut tabs.open[idx];
    tab.cursor = apply_move(tab.cursor, &rope, m, body_rows);
    tab.scroll = adjust_scroll(tab.scroll, tab.cursor, body_rows);
}

/// Pure cursor geometry over a rope. Clamps every output to valid
/// buffer coordinates given the current rope extent.
///
/// Vertical moves (`Up` / `Down` / `PageUp` / `PageDown`) carry
/// `preferred_col` forward and clamp `col` to the destination line —
/// so traversing a short line and landing on a long line later
/// restores the original goal column. Horizontal moves re-anchor
/// `preferred_col` to the new `col`.
pub(super) fn apply_move(c: Cursor, rope: &Rope, m: Move, body_rows: usize) -> Cursor {
    let line_count = rope.len_lines().max(1);
    let last_line = line_count - 1;
    let clamp_col = |line: usize, col: usize| col.min(line_char_len(rope, line));

    // Vertical move: pick `nl`, clamp goal col to it, keep preferred.
    let vertical = |nl: usize| -> Cursor {
        Cursor {
            line: nl,
            col: clamp_col(nl, c.preferred_col),
            preferred_col: c.preferred_col,
        }
    };
    // Horizontal move: anchor preferred_col to the new col.
    let horizontal = |line: usize, col: usize| -> Cursor {
        Cursor {
            line,
            col,
            preferred_col: col,
        }
    };

    match m {
        Move::Up => vertical(c.line.saturating_sub(1)),
        Move::Down => vertical((c.line + 1).min(last_line)),
        Move::PageUp => vertical(c.line.saturating_sub(body_rows.max(1))),
        Move::PageDown => vertical((c.line + body_rows.max(1)).min(last_line)),
        Move::Left => horizontal(c.line, c.col.saturating_sub(1)),
        Move::Right => horizontal(c.line, clamp_col(c.line, c.col.saturating_add(1))),
        Move::LineStart => horizontal(c.line, 0),
        Move::LineEnd => horizontal(c.line, line_char_len(rope, c.line)),
        Move::FileStart => horizontal(0, 0),
        Move::FileEnd => {
            let line = last_line;
            horizontal(line, line_char_len(rope, line))
        }
        Move::WordLeft => {
            let (line, col) = word_boundary_back(rope, c.line, c.col);
            horizontal(line, col)
        }
        Move::WordRight => {
            let (line, col) = word_boundary_fwd(rope, c.line, c.col);
            horizontal(line, col)
        }
    }
}

/// Word = run of alphanumeric-or-underscore chars. `word_boundary_fwd`
/// skips any trailing non-word chars from the current position, then
/// skips word chars to land at the start of the next non-word run.
///
/// Walks the rope directly with `RopeSlice::char_at`-style indexing —
/// no intermediate allocation, matches the allocation-discipline rule
/// for dispatch hot paths.
fn word_boundary_fwd(rope: &Rope, mut line: usize, mut col: usize) -> (usize, usize) {
    let line_count = rope.len_lines().max(1);
    let last_line = line_count - 1;
    loop {
        let len = line_char_len(rope, line);
        // 1. Skip non-word chars on the current line.
        while col < len && !is_word_char(rope_char_at(rope, line, col)) {
            col += 1;
        }
        if col >= len {
            // Ran off the end; advance to the next line's start.
            if line == last_line {
                return (line, len);
            }
            line += 1;
            col = 0;
            continue;
        }
        // 2. Skip word chars; we land at the first non-word after them.
        while col < line_char_len(rope, line) && is_word_char(rope_char_at(rope, line, col)) {
            col += 1;
        }
        return (line, col);
    }
}

fn word_boundary_back(rope: &Rope, mut line: usize, mut col: usize) -> (usize, usize) {
    loop {
        if col == 0 {
            if line == 0 {
                return (0, 0);
            }
            line -= 1;
            col = line_char_len(rope, line);
            continue;
        }
        // 1. Skip non-word chars immediately behind the cursor.
        while col > 0 && !is_word_char(rope_char_at(rope, line, col - 1)) {
            col -= 1;
        }
        if col == 0 {
            // Line ran out to the left; check if we landed on a word
            // boundary or need to cross to the previous line.
            if line == 0 {
                return (0, 0);
            }
            // Previous line hasn't been scanned yet; loop handles it.
            line -= 1;
            col = line_char_len(rope, line);
            continue;
        }
        // 2. Skip the word run itself — we land at its start.
        while col > 0 && is_word_char(rope_char_at(rope, line, col - 1)) {
            col -= 1;
        }
        return (line, col);
    }
}

/// Move scroll.top so that the cursor row stays within
/// `[top, top + body_rows)`.
pub(super) fn adjust_scroll(s: Scroll, c: Cursor, body_rows: usize) -> Scroll {
    if body_rows == 0 {
        return s;
    }
    if c.line < s.top {
        Scroll { top: c.line }
    } else if c.line >= s.top.saturating_add(body_rows) {
        Scroll {
            top: c.line + 1 - body_rows,
        }
    } else {
        s
    }
}
