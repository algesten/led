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
            // status letter (legacy display.rs:1396-1417). The
            // name region fills the remaining `cols - 1`; status
            // letter is painted separately below so it keeps the
            // category style even on non-selected rows whose name
            // is uncoloured.
            let reserve_status = matches!(panel.mode, SidePanelMode::Browser);
            let name_width = if reserve_status {
                cols.saturating_sub(1)
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
            if entry.selected {
                // Selection + category composition (legacy
                // display.rs:1381-1389):
                //   - focused selection → pure selection style
                //     (loud, wins over marker colour).
                //   - unfocused selection → selection bg
                //     patched with marker fg so the user still
                //     sees "this errored file is selected".
                let base_sel = if panel.focused {
                    theme.browser_selected_focused
                } else {
                    theme.browser_selected_unfocused
                };
                let sel_style = if !panel.focused && let Some(status) = entry.status {
                    let marker = theme.category_style(status.category);
                    Style {
                        fg: marker.fg.or(base_sel.fg),
                        bg: base_sel.bg,
                        attrs: base_sel.attrs,
                    }
                } else {
                    base_sel
                };
                buf.put_str(buf_row, row_x, &line, sel_style);
            } else if entry.replaced {
                // Replaced hit rows stay visible so the user can
                // Left-arrow back onto them to undo. Paint them
                // with the dim `search_hit_replaced` style so the
                // distinction is obvious.
                buf.put_str(buf_row, row_x, &line, theme.search_hit_replaced);
            } else if let Some((start, end)) = entry.match_range {
                // Split into three styled runs so the matched
                // substring picks up `theme.search_match` styling
                // without disturbing the surrounding row.
                paint_row_with_match(
                    &line,
                    start as usize,
                    end as usize,
                    theme,
                    buf_row,
                    row_x,
                    buf,
                );
            } else if let Some(status) = entry.status {
                // Category colouring: the whole name is painted in
                // the category's theme style so the user spots the
                // error/warn/git/PR row even without the letter.
                // Matches legacy display.rs:1387-1391 ("marker_style
                // as the row colour when not selected").
                let marker = theme.category_style(status.category);
                buf.put_str(buf_row, row_x, &line, marker);
            } else {
                buf.put_str(buf_row, row_x, &line, Style::default());
            }

            // Status letter in the right-most column (Browser mode
            // only). When the row is selected, the letter keeps the
            // selection-row style so the highlighted bar reads
            // continuous across the whole row (legacy
            // display.rs:1420-1425). Otherwise the letter uses the
            // category style (coloured fg).
            if reserve_status {
                match entry.status {
                    Some(status) => {
                        if entry.selected {
                            let sel_style = if panel.focused {
                                theme.browser_selected_focused
                            } else {
                                theme.browser_selected_unfocused
                            };
                            buf.put_char(buf_row, name_end_col, status.letter, sel_style);
                        } else {
                            let marker = theme.category_style(status.category);
                            buf.put_char(buf_row, name_end_col, status.letter, marker);
                        }
                    }
                    None => {
                        // No category. Still honour selection bg
                        // so the highlight bar doesn't stop one
                        // col short of the panel edge.
                        if entry.selected {
                            let sel_style = if panel.focused {
                                theme.browser_selected_focused
                            } else {
                                theme.browser_selected_unfocused
                            };
                            buf.put_char(buf_row, name_end_col, ' ', sel_style);
                        } else {
                            buf.put_char(buf_row, name_end_col, ' ', Style::default());
                        }
                    }
                }
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
/// picks up `theme.search_match` styling. `start` / `end` are char
/// offsets inside `line` (the post-padded / post-truncated text the
/// sidebar will print). Clamps gracefully when the range is out of
/// bounds — mis-computed indices shouldn't crash the painter.
fn paint_row_with_match(
    line: &str,
    start: usize,
    end: usize,
    theme: &Theme,
    row: u16,
    col_start: u16,
    buf: &mut Buffer,
) {
    let total = line.chars().count();
    let start = start.min(total);
    let end = end.min(total).max(start);
    if end == start {
        buf.put_str(row, col_start, line, Style::default());
        return;
    }
    let prefix: String = line.chars().take(start).collect();
    let matched: String = line.chars().skip(start).take(end - start).collect();
    let suffix: String = line.chars().skip(end).collect();
    let mut col = col_start;
    if !prefix.is_empty() {
        col = buf.put_str(row, col, &prefix, Style::default());
    }
    col = buf.put_str(row, col, &matched, theme.search_match);
    if !suffix.is_empty() {
        buf.put_str(row, col, &suffix, Style::default());
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
