//! Cell grid backing the ratatui-style double-buffered renderer.
//!
//! A [`Buffer`] is a flat row-major grid of [`Cell`]s. The driver
//! keeps two of them: `current` is written by the paint functions
//! each frame; `prev` holds last frame's state. After paint,
//! [`diff`] yields the minimal `(row, col, &Cell)` updates between
//! the two, which the backend emits as ANSI. Swapping the buffers
//! is an index flip — no allocation or clone per frame, idle ticks
//! produce an empty update list.
//!
//! This is a direct port of the data shape ratatui uses in
//! `ratatui-core/src/buffer/{buffer.rs,cell.rs}`, simplified to
//! led's needs: a single `char` per cell (no grapheme clusters
//! wider than one char here yet), our own [`Style`] type (not
//! ratatui's), and the same `skip`-less diff semantics. The
//! architectural win over vt100-based diffing is the double-buffer
//! swap: idle-frame allocation drops from ~340 KB (vt100's
//! `Screen::clone`) to zero.
//!
//! Wide-character support is limited to ASCII + common BMP chars
//! at one cell each; `●` and friends in the theme's diagnostic
//! glyphs already work because they're single-cell-wide unicode.

use led_driver_terminal_core::Style;

/// One terminal cell: the character it renders and the style
/// (fg / bg / attrs) that character carries.
///
/// `BLANK` uses space + default style — i.e. "terminal background,
/// no attributes". The blank cell doubles as "nothing painted
/// here yet" on first render and as the reset state after a
/// [`Buffer::clear`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

impl Cell {
    pub const BLANK: Self = Self {
        ch: ' ',
        style: Style::default_const(),
    };
}

impl Default for Cell {
    fn default() -> Self {
        Self::BLANK
    }
}

/// Row-major cell grid, fixed dimensions per frame. Paint
/// functions write cells in by `(row, col)`; `diff` walks two
/// buffers of the same size and yields the cells that differ.
///
/// Dimensions live on the struct rather than being derived from
/// `cells.len()` so degenerate 0×N / N×0 buffers stay
/// representable (a freshly-constructed driver is 0×0 until the
/// first resize).
#[derive(Clone, Debug)]
pub struct Buffer {
    rows: u16,
    cols: u16,
    cells: Vec<Cell>,
}

#[allow(dead_code)] // public API of the buffer module — driver uses a subset; keep the rest nameable for tests + future paint sites.
impl Buffer {
    pub fn new(rows: u16, cols: u16) -> Self {
        let len = usize::from(rows) * usize::from(cols);
        Self {
            rows,
            cols,
            cells: vec![Cell::BLANK; len],
        }
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// Resize in place. Cell contents are reset to BLANK because
    /// any surviving old data would be meaningless at new
    /// coordinates; the caller (driver) issues a terminal
    /// `Clear(All)` on resize so the real screen matches.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        let len = usize::from(rows) * usize::from(cols);
        self.cells.clear();
        self.cells.resize(len, Cell::BLANK);
    }

    /// Reset every cell to BLANK without changing dims.
    pub fn clear(&mut self) {
        for c in self.cells.iter_mut() {
            *c = Cell::BLANK;
        }
    }

    /// Overwrite every cell with `other`'s cells. Dims must match
    /// (the driver keeps the two double-buffers resized together,
    /// so they always do). Used before partial paint to seed the
    /// write-into buffer with the previous frame's cells — skipped
    /// regions keep their prior-frame values that way without
    /// per-frame allocation.
    pub fn copy_from(&mut self, other: &Buffer) {
        debug_assert_eq!(
            (self.rows, self.cols),
            (other.rows, other.cols),
            "copy_from requires matching dims",
        );
        self.cells.copy_from_slice(&other.cells);
    }

    fn idx(&self, row: u16, col: u16) -> Option<usize> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        Some(usize::from(row) * usize::from(self.cols) + usize::from(col))
    }

    /// Read one cell. Out-of-range returns `None`; callers always
    /// get a BLANK fallback from [`cell_or_blank`] if they prefer
    /// not to branch.
    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        self.idx(row, col).map(|i| &self.cells[i])
    }

    pub fn cell_or_blank(&self, row: u16, col: u16) -> Cell {
        self.cell(row, col).copied().unwrap_or(Cell::BLANK)
    }

    /// Write one cell. Out-of-range writes are silently dropped so
    /// paint code can be a little sloppy about area bounds.
    pub fn put_char(&mut self, row: u16, col: u16, ch: char, style: Style) {
        if let Some(i) = self.idx(row, col) {
            self.cells[i] = Cell { ch, style };
        }
    }

    /// Write a run of cells starting at `(row, col)`. Returns the
    /// column AFTER the last written cell — callers chain this to
    /// track where to write next (gutter → content → continuation
    /// glyph, etc.). Chars past the row's right edge are dropped.
    pub fn put_str(&mut self, row: u16, col: u16, s: &str, style: Style) -> u16 {
        let mut c = col;
        for ch in s.chars() {
            if c >= self.cols {
                break;
            }
            self.put_char(row, c, ch, style);
            c = c.saturating_add(1);
        }
        c
    }

    /// Fill `[col_start, col_end)` on `row` with blank cells at
    /// `style` (usually `Style::default()` for "reset background").
    /// Takes the place of `Clear(UntilNewLine)` in the old
    /// ANSI-emitting paint path.
    pub fn fill_row(&mut self, row: u16, col_start: u16, col_end: u16, style: Style) {
        let end = col_end.min(self.cols);
        for c in col_start..end {
            self.put_char(row, c, ' ', style);
        }
    }
}

/// Minimal `(row, col, &Cell)` list describing how `prev` should
/// change to become `next`. Wraps a plain `Vec` because the
/// backend emit loop wants random-access slicing (to merge same-
/// style runs). Idle frame → empty list, zero byte writes.
///
/// This is the ratatui shape: references into `next`, no cell
/// clones, O(cells) walk with O(changes) output.
pub fn diff<'a>(prev: &Buffer, next: &'a Buffer) -> Vec<(u16, u16, &'a Cell)> {
    // Mismatched dims = full repaint against an empty `prev`
    // substitute. Shouldn't happen in steady state (driver resizes
    // both buffers together), but defend against the corner case
    // so the backend never panics on a resize race.
    if prev.rows != next.rows || prev.cols != next.cols {
        let mut out: Vec<(u16, u16, &Cell)> = Vec::with_capacity(next.cells.len());
        for row in 0..next.rows {
            for col in 0..next.cols {
                if let Some(cell) = next.cell(row, col) {
                    if *cell != Cell::BLANK {
                        out.push((row, col, cell));
                    }
                }
            }
        }
        return out;
    }
    let cols = next.cols;
    let mut out: Vec<(u16, u16, &Cell)> = Vec::new();
    for (i, (n, p)) in next.cells.iter().zip(prev.cells.iter()).enumerate() {
        if n != p {
            let row = (i / usize::from(cols)) as u16;
            let col = (i % usize::from(cols)) as u16;
            out.push((row, col, n));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_blank_grid_of_requested_size() {
        let b = Buffer::new(3, 4);
        assert_eq!(b.rows(), 3);
        assert_eq!(b.cols(), 4);
        assert_eq!(b.cells().len(), 12);
        assert!(b.cells().iter().all(|c| *c == Cell::BLANK));
    }

    #[test]
    fn put_str_writes_chars_and_returns_next_col() {
        let mut b = Buffer::new(1, 5);
        let end = b.put_str(0, 1, "abc", Style::default());
        assert_eq!(end, 4);
        assert_eq!(b.cell(0, 0), Some(&Cell::BLANK));
        assert_eq!(b.cell(0, 1).unwrap().ch, 'a');
        assert_eq!(b.cell(0, 3).unwrap().ch, 'c');
        assert_eq!(b.cell(0, 4), Some(&Cell::BLANK));
    }

    #[test]
    fn put_str_clips_past_right_edge() {
        let mut b = Buffer::new(1, 3);
        let end = b.put_str(0, 2, "abc", Style::default());
        assert_eq!(end, 3);
        assert_eq!(b.cell(0, 2).unwrap().ch, 'a');
        // Out-of-range writes silently dropped.
    }

    #[test]
    fn fill_row_blanks_range_at_given_style() {
        let mut b = Buffer::new(1, 5);
        b.put_str(0, 0, "xxxxx", Style::default());
        b.fill_row(0, 1, 4, Style::default());
        assert_eq!(b.cell(0, 0).unwrap().ch, 'x');
        assert_eq!(b.cell(0, 1).unwrap().ch, ' ');
        assert_eq!(b.cell(0, 3).unwrap().ch, ' ');
        assert_eq!(b.cell(0, 4).unwrap().ch, 'x');
    }

    #[test]
    fn diff_empty_when_buffers_match() {
        let a = Buffer::new(2, 3);
        let b = Buffer::new(2, 3);
        assert!(diff(&a, &b).is_empty());
    }

    #[test]
    fn diff_reports_only_changed_cells() {
        let a = Buffer::new(1, 4);
        let mut b = Buffer::new(1, 4);
        b.put_char(0, 2, 'X', Style::default());
        let d = diff(&a, &b);
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].0, d[0].1), (0, 2));
        assert_eq!(d[0].2.ch, 'X');
    }

    #[test]
    fn resize_blanks_all_cells() {
        let mut b = Buffer::new(1, 3);
        b.put_str(0, 0, "abc", Style::default());
        b.resize(2, 4);
        assert_eq!(b.rows(), 2);
        assert_eq!(b.cols(), 4);
        assert!(b.cells().iter().all(|c| *c == Cell::BLANK));
    }
}
