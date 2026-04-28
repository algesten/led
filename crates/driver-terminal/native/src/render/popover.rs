use led_driver_terminal_core::{
    Attrs, Color, Dims, PopoverModel, PopoverSeverity, Rect, Style, Theme,
};

use crate::buffer::Buffer;

/// Draw the cursor-line diagnostic popover — a floating box anchored
/// near the cursor. Matches legacy's UX exactly: dark-gray fill, no
/// border, one inner-padding column on each side, Y prefers above the
/// anchor line, X clamps so the box stays on screen.
pub(crate) fn paint_popover(
    pop: &PopoverModel,
    editor_area: Rect,
    dims: Dims,
    theme: &Theme,
    buf: &mut Buffer,
) {
    if pop.lines.is_empty() {
        return;
    }

    // Max content width across all non-rule lines; rule lines take
    // the full content width implicitly.
    let content_w = pop
        .lines
        .iter()
        .filter(|l| l.severity.is_some())
        .map(|l| l.text.chars().count())
        .max()
        .unwrap_or(1);
    // Outer width = content + 1-char inner padding on each side.
    let outer_w = (content_w + 2).min(editor_area.cols as usize).max(3);
    let height = pop
        .lines
        .len()
        .min(editor_area.rows as usize / 2)
        .max(1);
    let lines = &pop.lines[..height];

    // X: clamp so the right edge doesn't leave the editor area.
    let area_right = editor_area.x.saturating_add(editor_area.cols);
    let max_x = area_right.saturating_sub(outer_w as u16);
    let x = pop.anchor.0.min(max_x).max(editor_area.x);
    // Y: prefer above the anchor row, fall back to below if there
    // isn't room. The editor area's top edge is the clamp; rows
    // above the editor (tab bar) never receive popover content.
    let y = if pop.anchor.1 >= editor_area.y.saturating_add(height as u16) {
        pop.anchor.1.saturating_sub(height as u16)
    } else {
        let below = pop.anchor.1.saturating_add(1);
        let area_bottom = editor_area.y.saturating_add(editor_area.rows);
        below
            .min(area_bottom.saturating_sub(height as u16))
            .max(editor_area.y)
    };

    // Guard: never overflow the physical terminal.
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

    let bg = Color::Indexed(236); // dark gray, matches legacy

    for (i, line) in lines.iter().take(height).enumerate() {
        let row = y + i as u16;
        let mut col = x;
        match line.severity {
            None => {
                // Horizontal rule: fill outer width with ─.
                let fg = Color::Indexed(245);
                let rule_style = Style {
                    fg: Some(fg),
                    bg: Some(bg),
                    attrs: Attrs::default(),
                };
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
                let style = Style {
                    fg: sev_style.fg,
                    bg: Some(bg),
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
