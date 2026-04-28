use led_driver_terminal_core::{Attrs, Color, CompletionPopupModel, Dims, Rect, Style, Theme};

use crate::buffer::Buffer;

/// Draw the LSP completion popup as a box anchored at (or above)
/// the cursor. Matches legacy's UX: dark-gray background for
/// unselected rows, blue highlight for the selected row; label
/// left-padded to the widest label in the window, then 2-space
/// separator, then detail (dim). Clamps to the editor area on
/// both axes.
pub(crate) fn paint_completion_popup(
    comp: &CompletionPopupModel,
    editor_area: Rect,
    dims: Dims,
    _theme: &Theme,
    buf: &mut Buffer,
) {
    if comp.rows.is_empty() {
        return;
    }

    // Dimensions. Outer width = label col + 2 (gap) + detail
    // col (when any row has a detail) + 2 (inner padding, 1 col
    // each side). Cap at the editor area so the popup never
    // overflows the sidebar / tab-bar region.
    let label_w = comp.label_width as usize;
    let detail_w = comp.detail_width as usize;
    let gap = if detail_w > 0 { 2 } else { 0 };
    let content_w = label_w + gap + detail_w;
    let outer_w = (content_w + 2)
        .min(editor_area.cols as usize)
        .max(3);
    let height = comp.rows.len();

    // X: clamp so the right edge doesn't leave the editor area.
    let area_right = editor_area.x.saturating_add(editor_area.cols);
    let max_x = area_right.saturating_sub(outer_w as u16);
    let x = comp.anchor.0.min(max_x).max(editor_area.x);
    // Y: prefer below the anchor. If it'd overflow the bottom
    // of the editor area, flip above.
    let below = comp.anchor.1.saturating_add(1);
    let area_bottom = editor_area.y.saturating_add(editor_area.rows);
    let y_below = below.min(area_bottom.saturating_sub(height as u16));
    let y = if below.saturating_add(height as u16) <= area_bottom {
        y_below
    } else if comp.anchor.1 >= editor_area.y.saturating_add(height as u16) {
        comp.anchor.1.saturating_sub(height as u16)
    } else {
        // Neither above nor below has room — paint what we can
        // starting at the top of the editor area.
        editor_area.y
    };

    // Guard: terminal smaller than our anchor.
    if x >= dims.cols || y >= dims.rows {
        return;
    }
    let outer_w = outer_w.min((dims.cols.saturating_sub(x)) as usize);
    if outer_w < 3 {
        return;
    }
    let height = height.min((dims.rows.saturating_sub(y)) as usize);
    if height == 0 {
        return;
    }

    // Styles: dark-gray normal, blue-bg selected. Hardcoded to
    // match legacy until `theme.completion_*` lands.
    let bg_normal = Color::Indexed(236); // dark gray
    let fg_normal = Color::Indexed(253); // near-white
    let bg_selected = Color::Indexed(24); // muted blue
    let fg_selected = Color::Indexed(231); // bright white
    let fg_detail = Color::Indexed(244); // dim gray

    for (i, row) in comp.rows.iter().take(height).enumerate() {
        let row_y = y + i as u16;
        let is_selected = i == comp.selected;
        let bg = if is_selected { bg_selected } else { bg_normal };
        let fg = if is_selected { fg_selected } else { fg_normal };
        let base = Style {
            fg: Some(fg),
            bg: Some(bg),
            attrs: Attrs::default(),
        };
        // Leading inner-padding space, label, label padding,
        // gap, detail + its pad, trailing inner-padding space.
        let mut col = x;
        buf.put_char(row_y, col, ' ', base);
        col = col.saturating_add(1);
        let label_chars: String = row.label.chars().take(label_w).collect();
        col = buf.put_str(row_y, col, &label_chars, base);
        // Pad label column to `label_w`.
        let label_printed = label_chars.chars().count();
        for _ in label_printed..label_w {
            buf.put_char(row_y, col, ' ', base);
            col = col.saturating_add(1);
        }
        // Gap.
        for _ in 0..gap {
            buf.put_char(row_y, col, ' ', base);
            col = col.saturating_add(1);
        }
        // Detail (dim fg except on selected row, where the
        // selection foreground wins so the whole row reads as
        // one highlighted band).
        let detail_style = if is_selected {
            base
        } else {
            Style {
                fg: Some(fg_detail),
                bg: Some(bg),
                attrs: Attrs::default(),
            }
        };
        let detail_printed = if let Some(d) = row.detail.as_ref() {
            let s: String = d.chars().take(detail_w).collect();
            col = buf.put_str(row_y, col, &s, detail_style);
            s.chars().count()
        } else {
            0
        };
        for _ in detail_printed..detail_w {
            buf.put_char(row_y, col, ' ', base);
            col = col.saturating_add(1);
        }
        // Trailing padding.
        let right_edge = x + outer_w as u16;
        while col < right_edge {
            buf.put_char(row_y, col, ' ', base);
            col = col.saturating_add(1);
        }
    }
}
