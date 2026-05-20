use led_driver_terminal_core::{CompletionPopupModel, Dims, Rect, Style, Theme};

use crate::buffer::Buffer;

/// Draw the LSP completion popup as a box anchored at (or above)
/// the cursor. Matches legacy's UX: dark-gray background for
/// unselected rows, blue highlight for the selected row; label
/// left-padded to the widest label in the window, then 2-space
/// separator, then detail (dim). Clamps to the editor area on
/// both axes.
pub(crate) fn paint_completion_popup(
    comp: &CompletionPopupModel,
    _editor_area: Rect,
    dims: Dims,
    theme: &Theme,
    buf: &mut Buffer,
) {
    if comp.rows.is_empty() {
        return;
    }

    // Placement (x, y, outer_w) is fully decided by the runtime
    // memo. The painter only re-clamps to the physical terminal
    // — the editor area is already accounted for by the memo,
    // but the OS terminal might be smaller (mid-resize).
    let label_w = comp.label_width as usize;
    let detail_w = comp.detail_width as usize;
    let gap = if detail_w > 0 { 2 } else { 0 };
    let outer_w_model = comp.outer_width as usize;
    let x = comp.placement.x;
    let y = comp.placement.y;

    // Guard: terminal smaller than our anchor.
    if x >= dims.cols || y >= dims.rows {
        return;
    }
    let outer_w = outer_w_model.min((dims.cols.saturating_sub(x)) as usize);
    if outer_w < 3 {
        return;
    }
    let height = comp.rows.len().min((dims.rows.saturating_sub(y)) as usize);
    if height == 0 {
        return;
    }

    // Per-row base style: the bg-style for normal vs selected
    // rows is pulled from theme; the text style (label fg/attrs)
    // overlays on top so users can tint label fg independently of
    // the row's bg if they want.
    let base_normal = compose_row_style(theme.completion_bg_normal, theme.completion_text_normal);
    let base_selected =
        compose_row_style(theme.completion_bg_selected, theme.completion_text_selected);

    for (i, row) in comp.rows.iter().take(height).enumerate() {
        let row_y = y + i as u16;
        let is_selected = i == comp.selected;
        let base = if is_selected { base_selected } else { base_normal };
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
            // Detail-column fg overlays the normal row bg so the
            // dim-grey reads cleanly against the dark fill. Falls
            // back to the base fg if the user clears `completion_
            // detail.fg` from theme.toml.
            Style {
                fg: theme.completion_detail.fg.or(base.fg),
                bg: base.bg,
                attrs: theme.completion_detail.attrs,
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

/// Compose the bg-style and text-style for one completion row into
/// a single [`Style`]. The bg-style owns the row's bg + the
/// default fg; the text-style is an optional override that lets a
/// theme set a label colour distinct from the row's bg, without
/// having to repeat the bg in two slots. Attributes from the
/// text-style are merged on top of the bg-style's attrs.
fn compose_row_style(bg_style: Style, text_style: Style) -> Style {
    Style {
        fg: text_style.fg.or(bg_style.fg),
        bg: bg_style.bg,
        attrs: if text_style.attrs == Default::default() {
            bg_style.attrs
        } else {
            text_style.attrs
        },
    }
}
