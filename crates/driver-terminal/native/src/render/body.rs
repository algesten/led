use led_driver_terminal_core::{BodyModel, Rect, Style, Theme};

use crate::buffer::Buffer;

type BodyRow<'a> = (
    Option<&'a str>,
    &'a [led_driver_terminal_core::LineSpan],
    Option<led_state_diagnostics::DiagnosticSeverity>,
    Option<led_core::IssueCategory>,
    &'a [led_driver_terminal_core::BodyDiagnostic],
);

pub(crate) fn paint_body(body: &BodyModel, area: Rect, theme: &Theme, buf: &mut Buffer) {
    let ruler = theme
        .ruler_column
        .filter(|c| *c < area.cols)
        .filter(|_| !theme.ruler.is_default());

    let match_highlight = match body {
        BodyModel::Content { match_highlight, .. } => *match_highlight,
        _ => None,
    };

    let right_edge = area.x.saturating_add(area.cols);

    for row in 0..area.rows {
        let buf_row = area.y + row;
        let mut col = area.x;
        // Resolve the row's text + (for Content) syntax spans +
        // gutter-diagnostic severity + gutter-category (merged
        // LSP/git bar) + inline underlines. Non-Content variants
        // carry none of the extras.
        let (line, spans, gutter_diag, gutter_cat, row_diags): BodyRow<'_> = match body {
            BodyModel::Empty => (None, &[], None, None, &[]),
            BodyModel::Content { lines, .. } => match lines.get(row as usize) {
                Some(bl) => (
                    Some(bl.text.as_str()),
                    bl.spans.as_slice(),
                    bl.gutter_diagnostic,
                    bl.gutter_category,
                    bl.diagnostics.as_slice(),
                ),
                None => (None, &[], None, None, &[]),
            },
        };
        if let Some(line) = line {
            if spans.is_empty() {
                col = buf.put_str(buf_row, col, line, Style::default());
            } else {
                col = paint_syntax_line(line, spans, &theme.syntax, buf_row, col, buf);
            }
        }
        // Blank the rest of the row at terminal default — matches
        // the old `Clear(UntilNewLine)`.
        buf.fill_row(buf_row, col, right_edge, Style::default());

        // Git/PR/LSP change bar in gutter col 0: a single `▎`
        // (U+258E LEFT ONE EIGHTH BLOCK) coloured via
        // `category_style`. Matches legacy display.rs's col-1
        // positioning (our col 0 is display.rs's col 1 because
        // led's tab bar doesn't reserve the same leading column).
        // Painted before the diagnostic dot so the two cells are
        // independent.
        if let Some(cat) = gutter_cat {
            let style = theme.category_style(cat);
            buf.put_char(buf_row, area.x, '\u{258E}', style);
        }

        // Diagnostic gutter marker: a single ● in gutter col 1
        // (the second of the two gutter cells — matches legacy
        // display.rs positioning, so goldens line up). Overpaint
        // after the row text so it's not clobbered by syntax
        // styling.
        if let Some(severity) = gutter_diag {
            let style = *severity_style(&theme.diagnostics, severity);
            buf.put_char(buf_row, area.x + 1, '●', style);
        }

        // Diagnostic underlines: for each row-diagnostic, overpaint
        // the ranged cells with the severity style + underline attr.
        for d in row_diags {
            if d.col_end <= d.col_start {
                continue;
            }
            let Some(line) = line else { continue };
            let base = *severity_style(&theme.diagnostics, d.severity);
            let mut underlined = base;
            underlined.attrs.underline = true;
            let start_col = area.x + d.col_start;
            let take = (d.col_end - d.col_start) as usize;
            let mut c = start_col;
            for ch in line.chars().skip(d.col_start as usize).take(take) {
                if c >= right_edge {
                    break;
                }
                buf.put_char(buf_row, c, ch, underlined);
                c = c.saturating_add(1);
            }
        }

        // File-search match highlight: a single run of cells inside
        // one row. Overpaint the matched substring with
        // `theme.search_match` so the hit stands out the way it
        // does in the sidebar. Only active when the file-search
        // overlay's selected hit lives on this visible row.
        if let Some(mh) = match_highlight
            && mh.row == row
            && let Some(line) = line
            && mh.col_end > mh.col_start
        {
            let start_col = area.x + mh.col_start;
            let take = (mh.col_end - mh.col_start) as usize;
            let mut c = start_col;
            for ch in line.chars().skip(mh.col_start as usize).take(take) {
                if c >= right_edge {
                    break;
                }
                buf.put_char(buf_row, c, ch, theme.search_match);
                c = c.saturating_add(1);
            }
        }

        // Overpaint the ruler column on top of the row. A single
        // cell, styled with `theme.ruler`. If the row's text covers
        // that column the original character keeps its slot and
        // picks up the ruler style; otherwise we print a plain
        // space so the ruler renders as a vertical stripe.
        if let Some(rc) = ruler {
            let glyph: char = line
                .and_then(|l| l.chars().nth(rc as usize))
                .unwrap_or(' ');
            // Skip zero-width / control chars — safer to fall back
            // to a plain space than emit something that might push
            // the cursor.
            let painted = if glyph.is_control() { ' ' } else { glyph };
            buf.put_char(buf_row, area.x + rc, painted, theme.ruler);
        }
    }
}

/// Look up the style for a diagnostic severity.
pub(crate) fn severity_style(
    theme: &led_driver_terminal_core::DiagnosticsTheme,
    severity: led_state_diagnostics::DiagnosticSeverity,
) -> &Style {
    use led_state_diagnostics::DiagnosticSeverity::*;
    match severity {
        Error => &theme.error,
        Warning => &theme.warning,
        Info => &theme.info,
        Hint => &theme.hint,
    }
}

/// Paint one body row into `buf` slicing it into styled runs
/// according to the syntax spans the runtime computed. Gaps
/// between spans (and any suffix past the last span) render
/// **unstyled** — `Style::default()`, terminal default fg/bg —
/// so glyphs tree-sitter didn't capture inherit the user's
/// terminal palette rather than borrowing from `syntax.default`.
///
/// `syntax.default` is the slot for explicit `TokenKind::Default`
/// captures (`@text`, `@embedded`, etc.). It is NOT the body-text
/// fill colour: a theme might set it to flag those captures
/// distinctly, and using it as a gap fill would smear that into
/// every un-captured identifier in the file. Decoupling these two
/// concerns keeps user themes predictable.
///
/// Returns the column AFTER the last written cell so the caller
/// can continue filling the row.
///
/// Spans are assumed non-overlapping and ascending in `col_start`.
/// The caller guarantees `col_end <= line_char_count` (runtime
/// clamps against `content_cols`), so we never overshoot the row.
pub(crate) fn paint_syntax_line(
    line: &str,
    spans: &[led_driver_terminal_core::LineSpan],
    syntax: &led_driver_terminal_core::SyntaxTheme,
    row: u16,
    col_start: u16,
    buf: &mut Buffer,
) -> u16 {
    let style_for = |kind: led_state_syntax::TokenKind| -> &Style { syntax.style_for(kind) };
    let gap_style = Style::default();

    let mut cursor_col: usize = 0;
    let mut out_col = col_start;
    for span in spans {
        let span_col_start = span.col_start as usize;
        let span_col_end = span.col_end as usize;
        if span_col_end <= cursor_col {
            // Malformed / overlapping input — skip the offending span
            // so we don't go backwards.
            continue;
        }
        if span_col_start > cursor_col {
            for ch in line
                .chars()
                .skip(cursor_col)
                .take(span_col_start - cursor_col)
            {
                buf.put_char(row, out_col, ch, gap_style);
                out_col = out_col.saturating_add(1);
            }
            cursor_col = span_col_start;
        }
        let s = *style_for(span.kind);
        for ch in line
            .chars()
            .skip(cursor_col)
            .take(span_col_end - cursor_col)
        {
            buf.put_char(row, out_col, ch, s);
            out_col = out_col.saturating_add(1);
        }
        cursor_col = span_col_end;
    }
    for ch in line.chars().skip(cursor_col) {
        buf.put_char(row, out_col, ch, gap_style);
        out_col = out_col.saturating_add(1);
    }
    out_col
}
