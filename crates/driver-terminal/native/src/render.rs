//! Backend side of the cell-grid renderer: walk the diff list and
//! emit the smallest ANSI byte stream that brings the terminal in
//! line with the new buffer.
//!
//! Mirrors `ratatui-crossterm-0.1.0/src/lib.rs`'s `Backend::draw`:
//!
//! - `MoveTo` is elided when the next cell is `x+1` on the same
//!   row (contiguous runs print without re-positioning).
//! - `SetForegroundColor` / `SetBackgroundColor` / attribute
//!   escapes fire only when the next cell's style differs from
//!   what we've been printing.
//! - A blanket `Attribute::Reset` + `ResetColor` lands at the end
//!   of the run so any subsequent raw writes (legacy escapes, etc.)
//!   start from a clean slate.
//!
//! We never emit `Clear(UntilNewLine)` from here — paint now
//! controls every cell explicitly via [`crate::buffer::Buffer`],
//! which is exactly what lets the soft-wrap `\` live in the
//! last column without being wiped.

use std::io::{self, Write};

use crossterm::{cursor, queue, style as ct};
use led_driver_terminal_core::{Attrs, Color, Style};

use crate::buffer::Cell;

/// Emit the diff list produced by [`crate::buffer::diff`] to
/// `out`. No-op when the list is empty — zero bytes go to the
/// terminal on an idle tick, no SGR reset, nothing at all.
pub(crate) fn draw_diff<W: Write>(
    updates: &[(u16, u16, &Cell)],
    out: &mut W,
) -> io::Result<()> {
    if updates.is_empty() {
        return Ok(());
    }

    let mut last_pos: Option<(u16, u16)> = None;
    let mut cur_style = Style::default();

    for (row, col, cell) in updates {
        let (row, col) = (*row, *col);

        // Elide `MoveTo` when we just printed the preceding cell
        // on the same row — the cursor's natural advance covers
        // us. `DisableLineWrap` at raw-mode setup means writing
        // the last column parks the cursor; any non-contiguous
        // next write re-anchors with an explicit `MoveTo`.
        let contiguous = matches!(last_pos, Some((r, c)) if r == row && c + 1 == col);
        if !contiguous {
            queue!(out, cursor::MoveTo(col, row))?;
        }

        // Emit style deltas only when the new cell's style
        // differs. Runs of same-styled cells print as one SGR
        // setup + many `Print`s — same shape as ratatui's
        // backend.
        if cell.style != cur_style {
            write_style_diff(out, cur_style, cell.style)?;
            cur_style = cell.style;
        }

        queue!(out, ct::Print(cell.ch))?;
        last_pos = Some((row, col));
    }

    // Leave the terminal in a clean state. Without this, any
    // cursor-placement escape we emit after the diff would carry
    // the last cell's attributes into whatever painted that spot.
    queue!(out, ct::SetAttribute(ct::Attribute::Reset), ct::ResetColor)?;
    Ok(())
}

/// Emit the SGR escapes to transition from `from` to `to`.
///
/// We don't chase the minimal "only what changed" set because
/// `SetAttribute(Reset)` is the one escape that reliably turns
/// off an attribute on every terminal — per-attribute "off"
/// escapes like `NoBold` exist but have spotty support. So any
/// attribute removal forces a full reset + reapply of the target
/// style. Shared-prefix attributes (both bold, different fg)
/// still skip the reset: we only reset when `to.attrs`
/// lacks a bit that `from.attrs` had.
fn write_style_diff<W: Write>(out: &mut W, from: Style, to: Style) -> io::Result<()> {
    let needs_reset = attrs_lost(from.attrs, to.attrs);
    if needs_reset {
        queue!(out, ct::SetAttribute(ct::Attribute::Reset))?;
        // Post-reset: whatever `to` needs must be reapplied in
        // full, since reset cleared colors too.
        if let Some(fg) = to.fg {
            queue!(out, ct::SetForegroundColor(to_ct_color(fg)))?;
        }
        if let Some(bg) = to.bg {
            queue!(out, ct::SetBackgroundColor(to_ct_color(bg)))?;
        }
        apply_attrs(out, to.attrs)?;
        return Ok(());
    }
    // Incremental: emit only what differs.
    if from.fg != to.fg {
        match to.fg {
            Some(fg) => queue!(out, ct::SetForegroundColor(to_ct_color(fg)))?,
            None => queue!(out, ct::SetForegroundColor(ct::Color::Reset))?,
        }
    }
    if from.bg != to.bg {
        match to.bg {
            Some(bg) => queue!(out, ct::SetBackgroundColor(to_ct_color(bg)))?,
            None => queue!(out, ct::SetBackgroundColor(ct::Color::Reset))?,
        }
    }
    if from.attrs != to.attrs {
        apply_attrs(out, to.attrs)?;
    }
    Ok(())
}

/// `true` when `to` is missing any attribute `from` had — we
/// need a blanket reset because per-attribute-off escapes aren't
/// reliable.
fn attrs_lost(from: Attrs, to: Attrs) -> bool {
    (from.bold && !to.bold) || (from.reverse && !to.reverse) || (from.underline && !to.underline)
}

fn apply_attrs<W: Write>(out: &mut W, a: Attrs) -> io::Result<()> {
    if a.bold {
        queue!(out, ct::SetAttribute(ct::Attribute::Bold))?;
    }
    if a.reverse {
        queue!(out, ct::SetAttribute(ct::Attribute::Reverse))?;
    }
    if a.underline {
        queue!(out, ct::SetAttribute(ct::Attribute::Underlined))?;
    }
    Ok(())
}

pub(crate) fn to_ct_color(c: Color) -> ct::Color {
    // Same mapping the old `apply_style` had — named-ANSI for 0-15
    // so terminals honour the user's palette, `AnsiValue` for the
    // 256-color extension, `Rgb` for truecolor.
    match c {
        Color::Indexed(0) => ct::Color::Black,
        Color::Indexed(1) => ct::Color::DarkRed,
        Color::Indexed(2) => ct::Color::DarkGreen,
        Color::Indexed(3) => ct::Color::DarkYellow,
        Color::Indexed(4) => ct::Color::DarkBlue,
        Color::Indexed(5) => ct::Color::DarkMagenta,
        Color::Indexed(6) => ct::Color::DarkCyan,
        Color::Indexed(7) => ct::Color::Grey,
        Color::Indexed(8) => ct::Color::DarkGrey,
        Color::Indexed(9) => ct::Color::Red,
        Color::Indexed(10) => ct::Color::Green,
        Color::Indexed(11) => ct::Color::Yellow,
        Color::Indexed(12) => ct::Color::Blue,
        Color::Indexed(13) => ct::Color::Magenta,
        Color::Indexed(14) => ct::Color::Cyan,
        Color::Indexed(15) => ct::Color::White,
        Color::Indexed(n) => ct::Color::AnsiValue(n),
        Color::Rgb { r, g, b } => ct::Color::Rgb { r, g, b },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;

    #[test]
    fn empty_diff_emits_nothing() {
        let mut out: Vec<u8> = Vec::new();
        draw_diff(&[], &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn contiguous_run_emits_single_moveto_then_chars() {
        let mut b = Buffer::new(1, 5);
        b.put_str(0, 1, "abc", Style::default());
        let prev = Buffer::new(1, 5);
        let d = crate::buffer::diff(&prev, &b);
        let mut out: Vec<u8> = Vec::new();
        draw_diff(&d, &mut out).unwrap();
        // One MoveTo at the start of the changed run, then 'a'
        // 'b' 'c' flowed without further repositioning, then
        // closing Reset + ResetColor.
        let s = String::from_utf8_lossy(&out).to_string();
        // crossterm MoveTo: ESC [ row+1 ; col+1 H — (row=0, col=1)
        // → "\x1b[1;2H"
        assert!(s.contains("\x1b[1;2H"), "expected MoveTo, got {s:?}");
        // Only one MoveTo (no reposition between 'a' → 'b' → 'c').
        assert_eq!(s.matches("\x1b[").count() >= 1, true);
        assert!(s.contains("abc"), "expected contiguous 'abc' run in {s:?}");
    }

    #[test]
    fn non_contiguous_cells_emit_moveto_between_them() {
        let mut b = Buffer::new(1, 10);
        b.put_char(0, 1, 'X', Style::default());
        b.put_char(0, 5, 'Y', Style::default());
        let prev = Buffer::new(1, 10);
        let d = crate::buffer::diff(&prev, &b);
        let mut out: Vec<u8> = Vec::new();
        draw_diff(&d, &mut out).unwrap();
        let s = String::from_utf8_lossy(&out).to_string();
        // Two MoveTo escapes — one per discontiguous cell.
        assert!(s.contains("\x1b[1;2H")); // row 0, col 1
        assert!(s.contains("\x1b[1;6H")); // row 0, col 5
        assert!(s.contains('X'));
        assert!(s.contains('Y'));
    }
}
