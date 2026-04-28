use led_driver_terminal_core::{Attrs, Color, Dims, Rect, RenamePopupModel, Style, Theme};

use crate::buffer::Buffer;

/// Draw the LSP rename overlay: a single-row box reading
/// "<space>Rename: <input><space>" followed by trailing padding
/// to `width`. Background: dark gray; foreground: bold white.
/// Anchored at `model.anchor`; clamped to the editor area on
/// the right so the box never overflows past the viewport edge.
pub(crate) fn paint_rename_popup(
    rp: &RenamePopupModel,
    editor_area: Rect,
    dims: Dims,
    _theme: &Theme,
    buf: &mut Buffer,
) {
    let (x, y) = rp.anchor;
    if x >= dims.cols || y >= dims.rows {
        return;
    }
    let area_right = editor_area.x.saturating_add(editor_area.cols);
    let term_right = dims.cols;
    let width = rp
        .width
        .min(area_right.saturating_sub(x))
        .min(term_right.saturating_sub(x));
    if width == 0 {
        return;
    }
    let style = Style {
        fg: Some(Color::Indexed(15)),
        bg: Some(Color::Indexed(236)),
        attrs: Attrs {
            bold: true,
            ..Attrs::default()
        },
    };
    // Compose the visible content: leading " Rename: " label,
    // the user's input, then space-fill out to `width`.
    let mut col = x;
    let right_edge = x.saturating_add(width);
    let put = |buf: &mut Buffer, col: &mut u16, ch: char| {
        if *col >= right_edge {
            return false;
        }
        buf.put_char(y, *col, ch, style);
        *col = col.saturating_add(1);
        true
    };
    for ch in " Rename: ".chars() {
        if !put(buf, &mut col, ch) {
            return;
        }
    }
    for ch in rp.input.chars() {
        if !put(buf, &mut col, ch) {
            return;
        }
    }
    while col < right_edge {
        buf.put_char(y, col, ' ', style);
        col = col.saturating_add(1);
    }
}
