use led_driver_terminal_core::{
    Dims, PopoverModel, PopoverSeverity, Rect, Style, Theme,
};

use crate::buffer::Buffer;

/// Draw the cursor-line diagnostic popover — a floating box anchored
/// near the cursor. Matches legacy's UX exactly: dark-gray fill, no
/// border, one inner-padding column on each side, Y prefers above the
/// anchor line, X clamps so the box stays on screen.
pub(crate) fn paint_popover(
    pop: &PopoverModel,
    _editor_area: Rect,
    dims: Dims,
    theme: &Theme,
    buf: &mut Buffer,
) {
    if pop.lines.is_empty() {
        return;
    }

    // Placement (x, y, outer_w, height) is fully decided by the
    // runtime memo. The painter only re-clamps to the physical
    // terminal — the editor area is already accounted for by the
    // memo, but the OS terminal might be smaller (mid-resize).
    let x = pop.placement.x;
    let y = pop.placement.y;
    let outer_w_model = pop.outer_width as usize;
    let height_model = pop.height as usize;
    let lines = &pop.lines[..height_model.min(pop.lines.len())];

    // Guard: never overflow the physical terminal.
    if x >= dims.cols || y >= dims.rows {
        return;
    }
    let outer_w = outer_w_model.min((dims.cols.saturating_sub(x)) as usize);
    if outer_w < 3 {
        return;
    }
    let height = height_model.min((dims.rows.saturating_sub(y)) as usize);
    if height == 0 {
        return;
    }

    // Background carries through every row of the box — pulled
    // from theme so users can tint the popover via theme.toml.
    let bg_style = theme.popover_bg;
    let rule_style = theme.popover_rule;

    for (i, line) in lines.iter().take(height).enumerate() {
        let row = y + i as u16;
        let mut col = x;
        match line.severity {
            None => {
                // Horizontal rule: fill outer width with ─.
                for _ in 0..outer_w {
                    col = buf.put_str(row, col, "─", rule_style);
                }
            }
            Some(sev) => {
                let sev_style = match sev {
                    PopoverSeverity::Error => theme.diagnostics.error,
                    PopoverSeverity::Warning => theme.diagnostics.warning,
                    PopoverSeverity::Info => theme.diagnostics.info,
                    PopoverSeverity::Hint => theme.diagnostics.hint,
                };
                // Compose: per-severity fg (with popover_text fg as
                // fallback) + per-severity attrs, on the popover bg.
                let style = Style {
                    fg: sev_style.fg.or(theme.popover_text.fg),
                    bg: bg_style.bg,
                    attrs: sev_style.attrs,
                };
                // Clip text to inner width (outer_w - 2), then
                // right-pad with spaces so the box fills even when
                // the message is shorter than the widest line.
                let inner_w = outer_w.saturating_sub(2);
                col = buf.put_str(row, col, " ", style);
                let mut written = 0usize;
                for ch in line.text.chars().take(inner_w) {
                    buf.put_char(row, col, ch, style);
                    col = col.saturating_add(1);
                    written += 1;
                }
                for _ in written..inner_w {
                    buf.put_char(row, col, ' ', style);
                    col = col.saturating_add(1);
                }
                buf.put_str(row, col, " ", style);
            }
        }
    }
}
