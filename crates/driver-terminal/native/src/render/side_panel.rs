use led_driver_terminal_core::{Rect, SidePanelModel, Style, Theme};

use crate::buffer::Buffer;

pub(crate) fn paint_side_panel(panel: &SidePanelModel, area: Rect, theme: &Theme, buf: &mut Buffer) {
    use led_driver_terminal_core::SidePanelMode;

    let cols = area.cols as usize;

    for row in 0..area.rows {
        let buf_row = area.y + row;
        let row_x = area.x;
        // File-search mode: row 0 is the toggle header. Paint it
        // with per-glyph styling so users can tell which of
        // `Aa` / `.*` / `=>` are on, then skip the usual row-print
        // path for that row.
        if row == 0
            && let SidePanelMode::FileSearch {
                case_sensitive,
                use_regex,
                replace_mode,
            } = panel.mode
        {
            paint_file_search_header(FileSearchHeaderPaint {
                col_start: row_x,
                row: buf_row,
                cols,
                case_sensitive,
                use_regex,
                replace_mode,
                theme,
                buf,
            });
            continue;
        }
        if let Some(entry) = panel.rows.get(row as usize) {
            // Two-space indent per depth, then chevron, then name.
            let mut line = String::with_capacity(cols);
            match panel.mode {
                SidePanelMode::Browser => {
                    for _ in 0..entry.depth {
                        line.push_str("  ");
                    }
                    match entry.chevron {
                        Some(true) => line.push_str("\u{25bd} "),  // ▽
                        Some(false) => line.push_str("\u{25b7} "), // ▷
                        None => line.push_str("  "),
                    }
                }
                SidePanelMode::Completions | SidePanelMode::FileSearch { .. } => {
                    // No indent + no chevron column: the leaf name
                    // starts at col 0.
                }
            }
            line.push_str(&entry.name);
            // Browser mode reserves the right-most column for the
            // status letter (legacy display.rs:1396-1417), plus one
            // blank gap column to its left so the letter doesn't sit
            // flush against the file name. The name region fills the
            // remaining `cols - 2`; status letter is painted
            // separately below so it keeps the category style even on
            // non-selected rows whose name is uncoloured.
            let reserve_status = matches!(panel.mode, SidePanelMode::Browser);
            let name_width = if reserve_status {
                cols.saturating_sub(2)
            } else {
                cols
            };
            let ch_count = line.chars().count();
            if ch_count < name_width {
                for _ in 0..(name_width - ch_count) {
                    line.push(' ');
                }
            } else if ch_count > name_width {
                let truncated: String = line.chars().take(name_width).collect();
                line = truncated;
            }
            let name_end_col = row_x + name_width as u16;
            // The runtime memo has pre-resolved the row's base
            // style; the painter just stamps it. The match-range
            // overlay still happens here because it requires
            // splitting the printed string into styled runs — the
            // theme.search_match style is read from theme rather
            // than stamped per-row to keep `SidePanelRow` from
            // duplicating every theme slot.
            if let Some((start, end)) = entry.match_range {
                paint_row_with_match(
                    PaintMatchArgs {
                        line: &line,
                        start: start as usize,
                        end: end as usize,
                        base_style: entry.name_style,
                        match_style: theme.search_match,
                        row: buf_row,
                        col_start: row_x,
                    },
                    buf,
                );
            } else {
                buf.put_str(buf_row, row_x, &line, entry.name_style);
            }

            // Status letter in the right-most column (Browser mode
            // only). The memo has pre-resolved gap + letter styles
            // into `status_cell`; non-Browser modes leave it
            // `None`. The column at `name_end_col` is a blank gap
            // so the letter doesn't sit flush against the file
            // name.
            if let Some(cell) = entry.status_cell.as_ref() {
                let letter_col = name_end_col + 1;
                buf.put_char(buf_row, name_end_col, ' ', cell.gap_style);
                let letter_ch = entry.status.map(|s| s.letter).unwrap_or(' ');
                buf.put_char(buf_row, letter_col, letter_ch, cell.letter_style);
            }
        } else {
            // Fill `cols` spaces — scoped to the side-panel area.
            // NOT `Clear(UntilNewLine)`: that would wipe the body
            // columns too. With the cell-grid model we can just
            // blank the panel's cells directly.
            buf.fill_row(buf_row, row_x, row_x + cols as u16, Style::default());
        }
    }
}

/// Split-print a non-selected hit row so the matched substring
/// picks up the match-highlight style. `start` / `end` (in `args`)
/// are char offsets inside `line` — clamps gracefully when the
/// range is out of bounds so mis-computed indices don't crash the
/// painter.
///
/// `base_style` paints the prefix + suffix runs (typically
/// `Style::default()` for non-selected hit rows; the runtime memo
/// pre-resolves this onto the row). `match_style` paints the
/// matched substring (typically `theme.search_match`).
struct PaintMatchArgs<'a> {
    line: &'a str,
    start: usize,
    end: usize,
    base_style: Style,
    match_style: Style,
    row: u16,
    col_start: u16,
}

fn paint_row_with_match(args: PaintMatchArgs<'_>, buf: &mut Buffer) {
    let PaintMatchArgs {
        line,
        start,
        end,
        base_style,
        match_style,
        row,
        col_start,
    } = args;
    let total = line.chars().count();
    let start = start.min(total);
    let end = end.min(total).max(start);
    if end == start {
        buf.put_str(row, col_start, line, base_style);
        return;
    }
    let prefix: String = line.chars().take(start).collect();
    let matched: String = line.chars().skip(start).take(end - start).collect();
    let suffix: String = line.chars().skip(end).collect();
    let mut col = col_start;
    if !prefix.is_empty() {
        col = buf.put_str(row, col, &prefix, base_style);
    }
    col = buf.put_str(row, col, &matched, match_style);
    if !suffix.is_empty() {
        buf.put_str(row, col, &suffix, base_style);
    }
}

pub(crate) fn paint_side_border(x: u16, rows: u16, theme: &Theme, buf: &mut Buffer) {
    for row in 0..rows {
        buf.put_char(row, x, '\u{2502}', theme.browser_border); // │
    }
}

/// File-search header row. Prints `" Aa   .*   =>"` with each of
/// the three two-char glyph pairs styled via `theme.search_toggle_on`
/// when the corresponding flag is set (plain otherwise). The leading
/// space and gaps between glyphs stay unstyled so the eye can
/// separate the three toggles at a glance. Pads with spaces to the
/// full panel width.
/// Bundle of layout coords + UI flags + theme + buffer for
/// [`paint_file_search_header`]. Carved out so the helper takes
/// a single argument instead of an 8-positional-arg list.
struct FileSearchHeaderPaint<'a> {
    col_start: u16,
    row: u16,
    cols: usize,
    case_sensitive: bool,
    use_regex: bool,
    replace_mode: bool,
    theme: &'a Theme,
    buf: &'a mut Buffer,
}

fn paint_file_search_header(args: FileSearchHeaderPaint<'_>) {
    let FileSearchHeaderPaint {
        col_start,
        row,
        cols,
        case_sensitive,
        use_regex,
        replace_mode,
        theme,
        buf,
    } = args;
    let on = theme.search_toggle_on;
    let mut printed = 0usize;
    let mut col = col_start;

    // Matches the text query.rs builds for row 0 of the overlay
    // (`" Aa   .*   =>"`), segment-for-segment. If that text
    // changes, update both sites.
    let segments: [(&str, bool); 6] = [
        (" ", false),
        ("Aa", case_sensitive),
        ("   ", false),
        (".*", use_regex),
        ("   ", false),
        ("=>", replace_mode),
    ];
    for (text, active) in segments {
        if printed >= cols {
            break;
        }
        let budget = cols - printed;
        let slice: String = text.chars().take(budget).collect();
        let style = if active { on } else { Style::default() };
        for ch in slice.chars() {
            buf.put_char(row, col, ch, style);
            col = col.saturating_add(1);
        }
        printed += slice.chars().count();
    }
    // Pad to the right edge so the row is fully repainted.
    for _ in printed..cols {
        buf.put_char(row, col, ' ', Style::default());
        col = col.saturating_add(1);
    }
}
