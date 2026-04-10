use led_core::CanonPath;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use led_core::Doc;
use led_core::PanelSlot;
use led_core::git::{self, FileStatus, LineStatus};
use led_core::wrap::{chars_to_string, compute_chunks, expand_tabs, find_sub_line};
use led_core::{Col, PersistedContentHash, RedrawSeq, Row, SubLine};
use led_state::{AppState, BracketPair, Dimensions, EntryKind, HighlightSpan};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::style;

// ── Display lines ──

#[derive(Clone)]
pub struct DisplayInputs {
    buffer_path: Option<CanonPath>,
    doc: Arc<dyn Doc>,
    scroll_row: Row,
    scroll_sub_line: SubLine,
    text_width: usize,
    buffer_height: usize,
    gutter_style: Style,
    text_style: Style,
    /// Normalized selection range: ((start_row, start_col), (end_row, end_col)) where start <= end.
    selection: Option<((Row, Col), (Row, Col))>,
    selection_style: Style,
    // Search match highlighting
    search_matches: Vec<(Row, Col, usize)>,
    search_match_idx: Option<usize>,
    search_match_style: Style,
    search_current_style: Style,
    // File search (C-f) match highlight in preview buffer
    file_search_match: Option<(Row, Col, usize)>,
    file_search_match_style: Style,
    // Syntax highlighting
    syntax_highlights: Rc<Vec<(Row, HighlightSpan)>>,
    bracket_pairs: Rc<Vec<BracketPair>>,
    matching_bracket: Option<(Row, Col)>,
    cursor_row: Row,
    cursor_col: Col,
    content_hash: PersistedContentHash,
    syntax_styles: Rc<HashMap<String, Style>>,
    bracket_match_style: Style,
    rainbow_styles: [Style; 6],
    git_line_statuses: Vec<LineStatus>,
    pr_diff_line_statuses: Vec<LineStatus>,
    pr_comment_lines: Vec<usize>,
    gutter_added_style: Style,
    gutter_modified_style: Style,
    gutter_pr_diff_style: Style,
    gutter_pr_comment_style: Style,
    diagnostics: Vec<(Row, Col, Row, Col, led_lsp::DiagnosticSeverity)>,
    inlay_hints: Vec<(Row, Col, String)>,
    diagnostic_error_style: Style,
    diagnostic_warning_style: Style,
    diagnostic_info_style: Style,
    diagnostic_hint_style: Style,
    inlay_hint_style: Style,
    inlay_hints_enabled: bool,
    ruler_col: Option<usize>,
    ruler_style: Style,
    focused: bool,
}

impl PartialEq for DisplayInputs {
    fn eq(&self, other: &Self) -> bool {
        self.focused == other.focused
            && self.buffer_path == other.buffer_path
            && self.content_hash == other.content_hash
            && self.scroll_row == other.scroll_row
            && self.scroll_sub_line == other.scroll_sub_line
            && self.text_width == other.text_width
            && self.buffer_height == other.buffer_height
            && self.gutter_style == other.gutter_style
            && self.text_style == other.text_style
            && self.selection == other.selection
            && self.selection_style == other.selection_style
            && self.search_matches == other.search_matches
            && self.search_match_idx == other.search_match_idx
            && self.search_match_style == other.search_match_style
            && self.search_current_style == other.search_current_style
            && self.file_search_match == other.file_search_match
            && self.file_search_match_style == other.file_search_match_style
            && self.syntax_highlights == other.syntax_highlights
            && self.bracket_pairs == other.bracket_pairs
            && self.matching_bracket == other.matching_bracket
            && self.cursor_row == other.cursor_row
            && self.cursor_col == other.cursor_col
            && self.git_line_statuses == other.git_line_statuses
            && self.pr_diff_line_statuses == other.pr_diff_line_statuses
            && self.pr_comment_lines == other.pr_comment_lines
            && self.diagnostics == other.diagnostics
            && self.inlay_hints == other.inlay_hints
            && self.inlay_hints_enabled == other.inlay_hints_enabled
            && self.ruler_col == other.ruler_col
    }
}

pub fn display_inputs(s: &AppState) -> Option<DisplayInputs> {
    let dims = s.dims?;
    let config_theme = s.config_theme.as_ref()?;
    let path = s.active_tab.as_ref()?;
    let buf = s.buffers.get(path).filter(|b| b.is_materialized())?;
    let theme_arc = &config_theme.file;
    let theme = theme_arc.as_ref();

    // Compute normalized selection from mark + cursor
    let selection = buf.mark().map(|(mr, mc)| {
        let (cr, cc) = (buf.cursor_row(), buf.cursor_col());
        if (mr, mc) <= (cr, cc) {
            ((mr, mc), (cr, cc))
        } else {
            ((cr, cc), (mr, mc))
        }
    });

    let (search_matches, search_match_idx) = buf
        .isearch
        .as_ref()
        .map(|is| (is.matches.clone(), is.match_idx))
        .unwrap_or_default();

    let file_search_match = s.file_search.as_ref().and_then(|fs| {
        let (group, hit) = fs.selected_hit()?;
        if buf.path() != Some(&group.path) {
            return None;
        }
        let char_len = hit
            .line_text
            .get(hit.match_start..hit.match_end)
            .map(|s| s.chars().count())
            .unwrap_or(0);
        Some((hit.row, hit.col, char_len))
    });

    let git_line_statuses = buf.status().git_line_statuses().to_vec();
    let gutter_added_style = style::resolve_cached(theme, &theme.git.gutter_added);
    let gutter_modified_style = style::resolve_cached(theme, &theme.git.gutter_modified);

    let active_path = buf.path();
    let (pr_diff_line_statuses, pr_comment_lines) = s
        .git
        .pr
        .as_ref()
        .and_then(|pr| {
            let p = active_path?;
            let diff = pr.diff_files.get(p).cloned().unwrap_or_default();
            let mut comments: Vec<usize> = pr
                .comments
                .get(p)
                .map(|cs| cs.iter().map(|c| *c.line).collect())
                .unwrap_or_default();
            comments.sort_unstable();
            comments.dedup();
            Some((diff, comments))
        })
        .unwrap_or_default();

    let default_gray = Style::default().fg(ratatui::style::Color::DarkGray);
    let default_blue = Style::default().fg(ratatui::style::Color::Blue);
    let gutter_pr_diff_style = theme
        .pr
        .as_ref()
        .map(|pr| style::resolve_cached(theme, &pr.gutter_diff))
        .unwrap_or(default_gray);
    let gutter_pr_comment_style = theme
        .pr
        .as_ref()
        .map(|pr| style::resolve_cached(theme, &pr.gutter_comment))
        .unwrap_or(default_blue);

    let diagnostics: Vec<(Row, Col, Row, Col, led_lsp::DiagnosticSeverity)> = buf
        .status()
        .diagnostics()
        .iter()
        .map(|d| (d.start_row, d.start_col, d.end_row, d.end_col, d.severity))
        .collect();

    let inlay_hints: Vec<(Row, Col, String)> = buf
        .status()
        .inlay_hints()
        .iter()
        .map(|h| (h.row, h.col, h.label.clone()))
        .collect();

    let diagnostic_error_style = style::resolve_cached(theme, &theme.diagnostics.error);
    let diagnostic_warning_style = style::resolve_cached(theme, &theme.diagnostics.warning);
    let diagnostic_info_style = style::resolve_cached(theme, &theme.diagnostics.info);
    let diagnostic_hint_style = style::resolve_cached(theme, &theme.diagnostics.hint);
    let inlay_hint_style = theme
        .editor
        .inlay_hint
        .as_ref()
        .map(|sv| style::resolve_cached(theme, sv))
        .unwrap_or_else(|| Style::default().fg(ratatui::style::Color::DarkGray));
    let inlay_hints_enabled = s.lsp.inlay_hints_enabled;

    let ruler_col = dims.ruler_column;
    let ruler_style = theme
        .editor
        .ruler
        .as_ref()
        .map(|sv| style::resolve_cached(theme, sv))
        .unwrap_or_else(|| Style::default().fg(ratatui::style::Color::DarkGray));

    Some(DisplayInputs {
        buffer_path: buf.path().cloned(),
        doc: buf.doc().clone(),
        scroll_row: buf.scroll_row(),
        scroll_sub_line: buf.scroll_sub_line(),
        text_width: dims.text_width(),
        buffer_height: dims.buffer_height(),
        gutter_style: style::resolve_cached(theme, &theme.editor.gutter),
        text_style: style::resolve_cached(theme, &theme.editor.text),
        selection,
        selection_style: style::resolve_cached(theme, &theme.editor.selection),
        search_matches,
        search_match_idx,
        search_match_style: style::resolve_cached(theme, &theme.editor.search_match),
        search_current_style: style::resolve_cached(theme, &theme.editor.search_current),
        file_search_match,
        file_search_match_style: style::resolve_cached(theme, &theme.editor.file_search_match),
        syntax_highlights: buf.syntax_highlights().clone(),
        bracket_pairs: buf.bracket_pairs().clone(),
        matching_bracket: BracketPair::find_match(
            buf.bracket_pairs(),
            buf.cursor_row(),
            buf.cursor_col(),
        ),
        cursor_row: buf.cursor_row(),
        cursor_col: buf.cursor_col(),
        content_hash: buf.content_hash(),
        syntax_styles: style::resolve_syntax_map(theme_arc),
        bracket_match_style: style::resolve_cached(theme, &theme.brackets.match_),
        rainbow_styles: [
            style::resolve_cached(theme, &theme.brackets.rainbow_0),
            style::resolve_cached(theme, &theme.brackets.rainbow_1),
            style::resolve_cached(theme, &theme.brackets.rainbow_2),
            style::resolve_cached(theme, &theme.brackets.rainbow_3),
            style::resolve_cached(theme, &theme.brackets.rainbow_4),
            style::resolve_cached(theme, &theme.brackets.rainbow_5),
        ],
        git_line_statuses,
        pr_diff_line_statuses,
        pr_comment_lines,
        gutter_added_style,
        gutter_modified_style,
        gutter_pr_diff_style,
        gutter_pr_comment_style,
        diagnostics,
        inlay_hints,
        diagnostic_error_style,
        diagnostic_warning_style,
        diagnostic_info_style,
        diagnostic_hint_style,
        inlay_hint_style,
        inlay_hints_enabled,
        ruler_col,
        ruler_style,
        focused: s.focus == PanelSlot::Main && s.find_file.is_none() && s.file_search.is_none(),
    })
}

pub fn build_display_lines(d: &DisplayInputs) -> Rc<Vec<Line<'static>>> {
    led_core::with_line_buf(|line_buf| build_display_lines_inner(d, line_buf))
}

fn build_display_lines_inner(d: &DisplayInputs, line_buf: &mut String) -> Rc<Vec<Line<'static>>> {
    let mut display_lines: Vec<Line<'static>> = Vec::with_capacity(d.buffer_height);
    let line_count = d.doc.line_count();
    let mut screen_row: usize = 0;
    let mut line_idx = *d.scroll_row;
    let mut skip_sub_lines = *d.scroll_sub_line;

    while screen_row < d.buffer_height && line_idx < line_count {
        d.doc.line(led_core::Row(line_idx), line_buf);
        let (display, char_map) = expand_tabs(&line_buf);
        let chunks = compute_chunks(display.len(), d.text_width);

        // Compute selected display-column range for this line
        let sel_dcols = match d.selection {
            Some(((sr, sc), (er, ec))) if line_idx >= *sr && line_idx <= *er => {
                let sd = if line_idx == *sr {
                    char_map.get(*sc).copied().unwrap_or(display.len())
                } else {
                    0
                };
                let ed = if line_idx == *er {
                    char_map.get(*ec).copied().unwrap_or(display.len())
                } else {
                    display.len()
                };
                if sd < ed { Some((sd, ed)) } else { None }
            }
            _ => None,
        };

        // Whether this line is within the selection and selection continues past its end
        // (used for padding the full line width with selection style)
        let line_selected_through = match d.selection {
            Some(((sr, _), (er, _))) => line_idx >= *sr && line_idx < *er,
            None => false,
        };

        for (chunk_idx, &(cs, ce)) in chunks.iter().enumerate() {
            if skip_sub_lines > 0 {
                skip_sub_lines -= 1;
                continue;
            }
            if screen_row >= d.buffer_height {
                break;
            }

            let is_last = chunk_idx == chunks.len() - 1;
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);

            // Gutter col 1: change marker (git diff > PR comment > PR diff)
            let change_char = if chunk_idx == 0 {
                let git = led_core::git::line_status_at(&d.git_line_statuses, line_idx);
                let pr_comment = d.pr_comment_lines.binary_search(&line_idx).is_ok();
                let pr_diff =
                    led_core::git::line_status_at(&d.pr_diff_line_statuses, line_idx).is_some();
                match (git, pr_comment, pr_diff) {
                    (Some(led_core::git::LineStatusKind::GitAdded), _, _) => {
                        Span::styled("\u{258E}", d.gutter_added_style)
                    }
                    (Some(led_core::git::LineStatusKind::GitModified), _, _) => {
                        Span::styled("\u{258E}", d.gutter_modified_style)
                    }
                    (_, true, _) => Span::styled("\u{258E}", d.gutter_pr_comment_style),
                    (_, _, true) => Span::styled("\u{258E}", d.gutter_pr_diff_style),
                    _ => Span::styled(" ", d.gutter_style),
                }
            } else {
                Span::styled(" ", d.gutter_style)
            };
            spans.push(change_char);

            // Gutter col 2: diagnostic severity indicator (most severe on this line)
            let diag_sev = if chunk_idx == 0 {
                d.diagnostics
                    .iter()
                    .filter(|(sr, _, _, _, _)| **sr == line_idx)
                    .map(|(_, _, _, _, s)| s)
                    .min_by_key(|s| match s {
                        led_lsp::DiagnosticSeverity::Error => 0,
                        led_lsp::DiagnosticSeverity::Warning => 1,
                        led_lsp::DiagnosticSeverity::Info => 2,
                        led_lsp::DiagnosticSeverity::Hint => 3,
                    })
            } else {
                None
            };
            let diag_char = match diag_sev {
                Some(led_lsp::DiagnosticSeverity::Error) => {
                    Span::styled("\u{25CF}", d.diagnostic_error_style)
                }
                Some(led_lsp::DiagnosticSeverity::Warning) => {
                    Span::styled("\u{25CF}", d.diagnostic_warning_style)
                }
                Some(led_lsp::DiagnosticSeverity::Info) => {
                    Span::styled("\u{25CF}", d.diagnostic_info_style)
                }
                Some(led_lsp::DiagnosticSeverity::Hint) => {
                    Span::styled("\u{25CF}", d.diagnostic_hint_style)
                }
                None => Span::styled(" ", d.gutter_style),
            };
            spans.push(diag_char);

            // Per-column style pipeline
            let chunk_len = ce - cs;

            // 1. Base: text_style
            let mut col_styles = vec![d.text_style; chunk_len];

            // 2. Syntax captures: sort by span size descending (larger first)
            //    so inner (smaller) spans overwrite outer ones
            let has_syntax = !d.syntax_highlights.is_empty();
            if has_syntax {
                let mut spans_for_line: Vec<&HighlightSpan> = d
                    .syntax_highlights
                    .iter()
                    .filter(|(l, _)| **l == line_idx)
                    .map(|(_, span)| span)
                    .collect();
                spans_for_line.sort_by_key(|s| std::cmp::Reverse(*s.char_end - *s.char_start));

                for span in spans_for_line {
                    let span_style = style::resolve_capture_style(
                        &span.capture_name,
                        &d.syntax_styles,
                        d.text_style,
                    );
                    let s_dcol = char_map
                        .get(*span.char_start)
                        .copied()
                        .unwrap_or(display.len());
                    let e_dcol = char_map
                        .get(*span.char_end)
                        .copied()
                        .unwrap_or(display.len());
                    let s = s_dcol.max(cs).saturating_sub(cs);
                    let e = e_dcol.min(ce).saturating_sub(cs);
                    for st in col_styles.get_mut(s..e).into_iter().flatten() {
                        *st = span_style;
                    }
                }
            }

            // 3. Rainbow brackets
            for bp in d.bracket_pairs.iter() {
                if let Some(ci) = bp.color_index {
                    let rainbow_style = d.rainbow_styles[ci % 6];
                    // Open bracket
                    if *bp.open_line == line_idx {
                        let dcol = char_map.get(*bp.open_col).copied().unwrap_or(display.len());
                        if dcol >= cs && dcol < ce {
                            col_styles[dcol - cs] = rainbow_style;
                        }
                    }
                    // Close bracket
                    if *bp.close_line == line_idx {
                        let dcol = char_map
                            .get(*bp.close_col)
                            .copied()
                            .unwrap_or(display.len());
                        if dcol >= cs && dcol < ce {
                            col_styles[dcol - cs] = rainbow_style;
                        }
                    }
                }
            }

            // 4. Matching bracket at cursor
            if let Some((match_row, match_col)) = d.matching_bracket {
                // Highlight the bracket under cursor
                if *d.cursor_row == line_idx {
                    let dcol = char_map
                        .get(*d.cursor_col)
                        .copied()
                        .unwrap_or(display.len());
                    if dcol >= cs && dcol < ce {
                        col_styles[dcol - cs] = d.bracket_match_style;
                    }
                }
                // Highlight the matching bracket
                if *match_row == line_idx {
                    let dcol = char_map.get(*match_col).copied().unwrap_or(display.len());
                    if dcol >= cs && dcol < ce {
                        col_styles[dcol - cs] = d.bracket_match_style;
                    }
                }
            }

            // 5. Selection overlay
            if let Some((ss, se)) = sel_dcols {
                if ss < ce && se > cs {
                    let s = ss.max(cs) - cs;
                    let e = se.min(ce) - cs;
                    for st in &mut col_styles[s..e] {
                        *st = d.selection_style;
                    }
                }
            }

            // 6. Search matches on top
            for (mi, &(mr, mc, mlen)) in d.search_matches.iter().enumerate() {
                if *mr != line_idx {
                    continue;
                }
                let ms = char_map.get(*mc).copied().unwrap_or(display.len());
                let me = char_map.get(*mc + mlen).copied().unwrap_or(display.len());
                if ms >= ce || me <= cs {
                    continue;
                }
                let is_current = d.search_match_idx == Some(mi);
                let match_style = if is_current {
                    d.search_current_style
                } else {
                    d.search_match_style
                };
                for i in ms.max(cs)..me.min(ce) {
                    col_styles[i - cs] = match_style;
                }
            }

            // 6b. File search match highlight (C-f preview)
            if let Some((fr, fc, flen)) = d.file_search_match {
                if *fr == line_idx {
                    let fs = char_map.get(*fc).copied().unwrap_or(display.len());
                    let fe = char_map.get(*fc + flen).copied().unwrap_or(display.len());
                    if fs < ce && fe > cs {
                        for i in fs.max(cs)..fe.min(ce) {
                            col_styles[i - cs] = d.file_search_match_style;
                        }
                    }
                }
            }

            // 6.5. Diagnostic underlines
            for &(dr_start, dc_start, dr_end, dc_end, ref sev) in &d.diagnostics {
                if line_idx < *dr_start || line_idx > *dr_end {
                    continue;
                }
                let diag_style = match sev {
                    led_lsp::DiagnosticSeverity::Error => d.diagnostic_error_style,
                    led_lsp::DiagnosticSeverity::Warning => d.diagnostic_warning_style,
                    led_lsp::DiagnosticSeverity::Info => d.diagnostic_info_style,
                    led_lsp::DiagnosticSeverity::Hint => continue,
                };
                let ds = if line_idx == *dr_start {
                    char_map.get(*dc_start).copied().unwrap_or(display.len())
                } else {
                    0
                };
                let de = if line_idx == *dr_end {
                    char_map.get(*dc_end).copied().unwrap_or(display.len())
                } else {
                    display.len()
                };
                for i in ds.max(cs)..de.min(ce) {
                    let idx = i - cs;
                    col_styles[idx] = col_styles[idx]
                        .fg(diag_style.fg.unwrap_or(ratatui::style::Color::Red))
                        .add_modifier(ratatui::style::Modifier::UNDERLINED);
                }
            }

            // 7. Group consecutive same-style columns into spans
            let mut pos = 0;
            while pos < chunk_len {
                let col_style = col_styles[pos];
                let mut end = pos + 1;
                while end < chunk_len && col_styles[end] == col_style {
                    end += 1;
                }
                spans.push(Span::styled(
                    chars_to_string(&display[cs + pos..cs + end]),
                    col_style,
                ));
                pos = end;
            }

            // Selection padding: on the last visual chunk of a line,
            // if selection continues to the next line, pad to text_width.
            // This covers both lines with content and empty lines.
            if is_last && line_selected_through {
                let pad = d.text_width.saturating_sub(chunk_len);
                if pad > 0 {
                    spans.push(Span::styled(" ".repeat(pad), d.selection_style));
                }
            }

            // Wrap indicator on non-last chunks
            if !is_last {
                spans.push(Span::styled("\\", d.gutter_style));
            }

            // Inlay hints: ghost text at end of line
            let mut hint_width = 0usize;
            if is_last && d.inlay_hints_enabled {
                for (hr, _hc, label) in &d.inlay_hints {
                    if **hr == line_idx {
                        let text = format!(" {}", label);
                        hint_width += text.len();
                        spans.push(Span::styled(text, d.inlay_hint_style));
                    }
                }
            }

            // Ruler: vertical line at ruler column, hidden when text/hints reach it
            if is_last && !line_selected_through {
                if let Some(ruler_col) = d.ruler_col {
                    let occupied = chunk_len + hint_width;
                    if ruler_col < d.text_width && occupied <= ruler_col {
                        let pad = ruler_col - occupied;
                        if pad > 0 {
                            spans.push(Span::styled(" ".repeat(pad), d.text_style));
                        }
                        spans.push(Span::styled("\u{2502}", d.ruler_style));
                    }
                }
            }

            display_lines.push(Line::from(spans));
            screen_row += 1;
        }

        line_idx += 1;
        skip_sub_lines = 0;
    }

    // Past-EOF rows
    while screen_row < d.buffer_height {
        let mut eof_spans = vec![Span::styled("~ ", d.gutter_style)];
        if let Some(ruler_col) = d.ruler_col {
            if ruler_col < d.text_width {
                if ruler_col > 0 {
                    eof_spans.push(Span::styled(" ".repeat(ruler_col), d.text_style));
                }
                eof_spans.push(Span::styled("\u{2502}", d.ruler_style));
            }
        }
        display_lines.push(Line::from(eof_spans));
        screen_row += 1;
    }

    Rc::new(display_lines)
}

// ── Cursor position ──

#[derive(Clone)]
pub struct CursorInputs {
    buffer_path: Option<CanonPath>,
    doc: Arc<dyn Doc>,
    cursor_row: Row,
    cursor_col: Col,
    scroll_row: Row,
    scroll_sub_line: SubLine,
    text_width: usize,
    gutter_width: u16,
}

impl PartialEq for CursorInputs {
    fn eq(&self, other: &Self) -> bool {
        self.buffer_path == other.buffer_path
            && self.cursor_row == other.cursor_row
            && self.cursor_col == other.cursor_col
            && self.scroll_row == other.scroll_row
            && self.scroll_sub_line == other.scroll_sub_line
            && self.text_width == other.text_width
            && self.gutter_width == other.gutter_width
    }
}

pub fn cursor_inputs(s: &AppState) -> Option<CursorInputs> {
    if s.file_search.is_some() {
        return None;
    }
    if s.find_file.is_some() {
        return None;
    }
    let dims = s.dims?;
    let path = s.active_tab.as_ref()?;
    let buf = s.buffers.get(path)?;
    Some(CursorInputs {
        buffer_path: buf.path().cloned(),
        doc: buf.doc().clone(),
        cursor_row: buf.cursor_row(),
        cursor_col: buf.cursor_col(),
        scroll_row: buf.scroll_row(),
        scroll_sub_line: buf.scroll_sub_line(),
        text_width: dims.text_width(),
        gutter_width: dims.gutter_width,
    })
}

/// Returns cursor position relative to buffer area: (x_offset, y_offset).
pub fn compute_cursor_pos(c: &CursorInputs) -> Option<(u16, u16)> {
    led_core::with_line_buf(|line_buf| compute_cursor_pos_inner(c, line_buf))
}

fn compute_cursor_pos_inner(c: &CursorInputs, line_buf: &mut String) -> Option<(u16, u16)> {
    c.doc.line(c.cursor_row, line_buf);
    let (cursor_display, char_map) = expand_tabs(&line_buf);
    let cursor_dcol = char_map
        .get(*c.cursor_col)
        .copied()
        .unwrap_or_else(|| char_map.last().copied().unwrap_or(0));
    let cursor_chunks = compute_chunks(cursor_display.len(), c.text_width);
    let cursor_sub = find_sub_line(&cursor_chunks, cursor_dcol);
    let (cs, _ce) = cursor_chunks[cursor_sub];

    // Compute visual row from scroll position
    let mut vrow: usize = 0;
    let line_count = c.doc.line_count();
    let mut line_idx = *c.scroll_row;
    let mut skip_sub_lines = *c.scroll_sub_line;

    while line_idx < line_count {
        // Reuse precomputed data for the cursor line; use lightweight
        // line_display_width for all other lines to avoid allocations.
        let chunks = if line_idx == *c.cursor_row {
            &cursor_chunks
        } else {
            let dw = c.doc.line_display_width(led_core::Row(line_idx));
            &compute_chunks(dw, c.text_width)
        };

        for (chunk_idx, _) in chunks.iter().enumerate() {
            if skip_sub_lines > 0 {
                skip_sub_lines -= 1;
                continue;
            }
            if line_idx == *c.cursor_row && chunk_idx == cursor_sub {
                let cx = c.gutter_width + (cursor_dcol - cs) as u16;
                return Some((cx, vrow as u16));
            }
            vrow += 1;
        }

        line_idx += 1;
        skip_sub_lines = 0;
    }

    None
}

// ── Status bar ──

#[derive(Clone, PartialEq)]
pub struct StatusInputs {
    pub file_name: String,
    pub is_dirty: bool,
    pub cursor_row: Row,
    pub cursor_col: Col,
    pub info: Option<String>,
    pub warn: Option<String>,
    pub viewport_width: u16,
    pub search_prompt: Option<String>,
    pub find_file_prompt: Option<(String, usize, led_state::FindFileMode)>,
    pub branch: Option<String>,
    pub pr: Option<(led_core::PrNumber, led_state::PrStatus)>,
    pub lsp_server_name: String,
    pub lsp_busy: bool,
    pub lsp_detail: Option<String>,
    pub spinner_tick: u32,
    pub recording_macro: bool,
}

pub fn status_inputs(s: &AppState) -> StatusInputs {
    let (file_name, is_dirty, cursor_row, cursor_col) = s
        .active_tab
        .as_ref()
        .and_then(|path| s.buffers.get(path).filter(|b| b.is_materialized()))
        .map(|buf| {
            let fname = buf
                .path()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            (fname, buf.is_dirty(), buf.cursor_row(), buf.cursor_col())
        })
        .unwrap_or((String::new(), false, Row(0), Col(0)));

    let file_name = if file_name.is_empty() {
        "led".to_string()
    } else {
        file_name
    };

    let viewport_width = s.dims.map_or(0, |d| d.viewport_width);

    let search_prompt = s
        .active_tab
        .as_ref()
        .and_then(|path| s.buffers.get(path))
        .and_then(|buf| {
            buf.isearch.as_ref().map(|is| {
                if is.failed {
                    format!("Failing search: {}", is.query)
                } else {
                    format!("Search: {}", is.query)
                }
            })
        });

    let find_file_prompt = s
        .find_file
        .as_ref()
        .map(|ff| (ff.input.clone(), ff.cursor, ff.mode));
    let branch = s.git.branch.clone();
    let pr = s.git.pr.as_ref().map(|p| (p.number, p.status.clone()));

    let lsp_detail = s.lsp.progress.as_ref().map(|p| {
        if let Some(ref msg) = p.message {
            format!("{} {}", p.title, msg)
        } else {
            p.title.clone()
        }
    });

    StatusInputs {
        file_name,
        is_dirty,
        cursor_row,
        cursor_col,
        info: s.alerts.info.clone(),
        warn: s.alerts.warn().map(|s| s.to_string()),
        viewport_width,
        search_prompt,
        find_file_prompt,
        branch,
        pr,
        lsp_server_name: s.lsp.server_name.clone(),
        lsp_busy: s.lsp.busy,
        lsp_detail,
        spinner_tick: s.lsp.spinner_tick,
        recording_macro: s.kbd_macro.recording,
    }
}

fn format_lsp_status(server_name: &str, busy: bool, detail: Option<&str>) -> String {
    if server_name.is_empty() {
        return String::new();
    }
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let spinner_char = |offset: u128| -> char {
        const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        FRAMES[((tick + offset) / 80) as usize % FRAMES.len()]
    };
    let spinner = if busy {
        format!("{} ", spinner_char(0))
    } else {
        String::new()
    };
    let detail_str = detail
        .filter(|d| !d.is_empty())
        .map(|d| {
            if busy {
                format!("  {} {d}", spinner_char(400))
            } else {
                format!("  {d}")
            }
        })
        .unwrap_or_default();
    format!("  {spinner}{server_name}{detail_str}")
}

#[derive(Clone, PartialEq)]
pub struct StatusContent {
    pub text: String,
    /// True when showing a persistent warning (render with warn style).
    pub is_warn: bool,
}

pub fn build_status_content(s: &StatusInputs) -> Rc<StatusContent> {
    // During find-file or save-as, show prompt
    if let Some((ref input, _cursor, mode)) = s.find_file_prompt {
        let label = match mode {
            led_state::FindFileMode::Open => "Find file",
            led_state::FindFileMode::SaveAs => "Save as",
        };
        let left = format!(" {label}: {input}");
        let total = s.viewport_width as usize;
        let padding = total.saturating_sub(left.chars().count());
        return Rc::new(StatusContent {
            text: format!("{}{:padding$}", left, "", padding = padding),
            is_warn: false,
        });
    }

    // During search, show search prompt instead of normal status
    if let Some(ref prompt) = s.search_prompt {
        let left = format!(" {}", prompt);
        let total = s.viewport_width as usize;
        let padding = total.saturating_sub(left.chars().count());
        return Rc::new(StatusContent {
            text: format!("{}{:padding$}", left, "", padding = padding),
            is_warn: false,
        });
    }

    let modified = if s.is_dirty { " \u{25cf}" } else { "" };

    let branch_str = s.branch.as_deref().unwrap_or("");
    let pr_str = match &s.pr {
        Some((num, led_state::PrStatus::Open)) => format!(" (PR #{})", num),
        Some((num, led_state::PrStatus::Merged)) => format!(" (PR #{}, merged)", num),
        Some((num, led_state::PrStatus::Closed)) => format!(" (PR #{}, closed)", num),
        None => String::new(),
    };

    let lsp_str = format_lsp_status(&s.lsp_server_name, s.lsp_busy, s.lsp_detail.as_deref());

    let default_left = format!(" {branch_str}{modified}{pr_str}{lsp_str}");

    // Info (transient) takes priority over warn (persistent).
    let (left, is_warn) = if let Some(m) = s.info.as_deref() {
        (format!(" {}", m), false)
    } else if let Some(m) = s.warn.as_deref() {
        (format!(" {}", m), true)
    } else if s.recording_macro {
        (" Defining kbd macro...".to_string(), false)
    } else {
        (default_left, false)
    };

    let pos = format!("L{}:C{} ", *s.cursor_row + 1, *s.cursor_col + 1);

    let total = s.viewport_width as usize;
    let left_width = left.chars().count();
    let right_width = pos.chars().count();
    let padding = total.saturating_sub(left_width + right_width);
    Rc::new(StatusContent {
        text: format!("{}{:padding$}{}", left, "", pos, padding = padding),
        is_warn,
    })
}

// ── Overlay (completion popup, code action picker, rename input) ──

#[derive(Clone, PartialEq)]
pub enum OverlayContent {
    None,
    Completion {
        items: Vec<(String, Option<String>, bool)>, // (label, detail, selected)
        anchor_x: u16,
        anchor_y: u16,
    },
    CodeActions {
        items: Vec<(String, bool)>, // (title, selected)
        anchor_x: u16,
        anchor_y: u16,
    },
    Rename {
        input: String,
        cursor: usize,
        anchor_x: u16,
        anchor_y: u16,
    },
    Diagnostic {
        messages: Vec<(led_lsp::DiagnosticSeverity, String)>,
        anchor_x: u16,
        anchor_y: u16,
    },
}

pub fn overlay_inputs(s: &AppState) -> OverlayContent {
    let dims = match s.dims {
        Some(d) => d,
        None => return OverlayContent::None,
    };
    let (cursor_x, cursor_y) = match s.active_tab.as_ref().and_then(|path| s.buffers.get(path)) {
        Some(buf) => {
            let x = dims.side_width()
                + dims.gutter_width
                + (*buf.cursor_col() as u16).min(dims.text_width() as u16);
            let y = buf.cursor_row().0.saturating_sub(buf.scroll_row().0) as u16;
            (x, y)
        }
        None => return OverlayContent::None,
    };

    if let Some(ref comp) = s.lsp.completion {
        let max_items = 10usize;
        let start = comp.scroll_offset;
        let items: Vec<(String, Option<String>, bool)> = comp
            .items
            .iter()
            .enumerate()
            .skip(start)
            .take(max_items)
            .map(|(i, item)| (item.label.clone(), item.detail.clone(), i == comp.selected))
            .collect();
        return OverlayContent::Completion {
            items,
            anchor_x: cursor_x,
            anchor_y: cursor_y + 1,
        };
    }

    if let Some(ref picker) = s.lsp.code_actions {
        let items: Vec<(String, bool)> = picker
            .actions
            .iter()
            .enumerate()
            .map(|(i, title)| (title.clone(), i == picker.selected))
            .collect();
        return OverlayContent::CodeActions {
            items,
            anchor_x: cursor_x,
            anchor_y: cursor_y + 1,
        };
    }

    if let Some(ref rename) = s.lsp.rename {
        return OverlayContent::Rename {
            input: rename.input.clone(),
            cursor: rename.cursor,
            anchor_x: cursor_x,
            anchor_y: cursor_y + 1,
        };
    }

    // Diagnostic popover: show when cursor is on a line with diagnostics.
    if let Some(buf) = s.active_tab.as_ref().and_then(|path| s.buffers.get(path)) {
        let crow = buf.cursor_row();
        let messages: Vec<_> = buf
            .status()
            .diagnostics()
            .iter()
            .filter(|d| crow >= d.start_row && crow <= d.end_row)
            .map(|d| (d.severity, format_diagnostic_message(d)))
            .collect();
        if !messages.is_empty() {
            return OverlayContent::Diagnostic {
                messages,
                anchor_x: cursor_x,
                anchor_y: cursor_y,
            };
        }
    }

    OverlayContent::None
}

fn format_diagnostic_message(d: &led_lsp::Diagnostic) -> String {
    let mut prefix = String::new();
    if let Some(ref src) = d.source {
        prefix.push_str(src);
    }
    if let Some(ref code) = d.code {
        if prefix.is_empty() {
            prefix.push_str(code);
        } else {
            prefix.push('(');
            prefix.push_str(code);
            prefix.push(')');
        }
    }
    if prefix.is_empty() {
        d.message.clone()
    } else {
        format!("{}: {}", prefix, d.message)
    }
}

// ── Tab bar ──

#[derive(Clone, PartialEq)]
pub struct TabEntry {
    pub label: String,
    pub is_active: bool,
    pub style: Style,
}

#[derive(Clone, PartialEq)]
pub struct TabsInputs {
    pub entries: Vec<TabEntry>,
    pub inactive_style: Style,
    pub gutter_width: u16,
}

pub fn tabs_inputs(s: &AppState) -> Option<TabsInputs> {
    let theme = s.config_theme.as_ref()?;
    let dims = s.dims?;
    let theme = theme.file.as_ref();
    let active_style = style::resolve_cached(theme, &theme.tabs.active);
    let inactive_style = style::resolve_cached(theme, &theme.tabs.inactive);
    let preview_active_style = style::resolve_cached(theme, &theme.tabs.preview_active);
    let preview_inactive_style = style::resolve_cached(theme, &theme.tabs.preview_inactive);

    let entries = s
        .tabs
        .iter()
        .filter_map(|tab| {
            let buf = s.buffers.get(tab.path()).filter(|b| b.is_materialized())?;
            let name = buf
                .path()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "[untitled]".to_string());
            let dirty = buf.is_dirty();
            let label = format_tab_label(&name, dirty);
            let is_active = s.active_tab.as_ref() == Some(tab.path());
            let entry_style = if tab.is_preview() {
                if is_active {
                    preview_active_style
                } else {
                    preview_inactive_style
                }
            } else if is_active {
                active_style
            } else {
                inactive_style
            };
            Some(TabEntry {
                label,
                is_active,
                style: entry_style,
            })
        })
        .collect();

    Some(TabsInputs {
        entries,
        inactive_style,
        gutter_width: dims.gutter_width,
    })
}

const MAX_TAB_CHARS: usize = 15;

fn format_tab_label(name: &str, dirty: bool) -> String {
    let lead = if dirty { "\u{25cf}" } else { " " };
    let char_count = name.chars().count();
    let truncated = char_count + 1 > MAX_TAB_CHARS;
    let take = if truncated {
        MAX_TAB_CHARS - 2
    } else {
        char_count
    };
    lead.chars()
        .chain(name.chars().take(take))
        .chain(if truncated { Some('\u{2026}') } else { None })
        .chain(" ".chars())
        .collect()
}

pub fn build_tab_entries(t: &TabsInputs) -> Rc<TabsInputs> {
    // Tabs are already built in the inputs — just wrap in Rc for cheap cloning
    Rc::new(t.clone())
}

// ── Layout ──

#[derive(Clone, Copy, PartialEq)]
pub struct LayoutInputs {
    pub dims: Option<Dimensions>,
    pub has_theme: bool,
    pub force_redraw: RedrawSeq,
    pub side_border_style: Style,
    pub side_bg_style: Style,
    pub text_style: Style,
    pub status_style: Style,
}

pub fn layout_inputs(s: &AppState) -> LayoutInputs {
    let side_border_style = s
        .config_theme
        .as_ref()
        .map(|ct| {
            let t = ct.file.as_ref();
            if s.file_search.is_some() {
                style::resolve_cached(t, &t.file_search.border)
            } else {
                style::resolve_cached(t, &t.browser.border)
            }
        })
        .unwrap_or_default();

    let (side_bg_style, text_style, status_style) = s
        .config_theme
        .as_ref()
        .map(|ct| {
            let t = ct.file.as_ref();
            (
                style::resolve_cached(t, &t.browser.file),
                style::resolve_cached(t, &t.editor.text),
                style::resolve_cached(t, &t.status_bar.style),
            )
        })
        .unwrap_or_default();

    // Force side panel when file search or find-file completions should show
    let dims = match s.dims {
        Some(mut d) => {
            if s.file_search.is_some() {
                d.show_side_panel = true;
            }
            if s.find_file.as_ref().is_some_and(|ff| ff.show_side) {
                d.show_side_panel = true;
            }
            Some(d)
        }
        None => None,
    };

    LayoutInputs {
        dims,
        has_theme: s.config_theme.is_some(),
        force_redraw: s.force_redraw,
        side_border_style,
        side_bg_style,
        text_style,
        status_style,
    }
}

#[derive(Clone, Copy)]
pub struct LayoutInfo {
    pub dims: Dimensions,
    pub force_redraw: RedrawSeq,
    pub side_border_style: Style,
    pub side_bg_style: Style,
    pub text_style: Style,
    pub status_style: Style,
}

pub fn build_layout(l: &LayoutInputs) -> Option<LayoutInfo> {
    let dims = l.dims?;
    if !l.has_theme {
        return None;
    }
    Some(LayoutInfo {
        dims,
        force_redraw: l.force_redraw,
        side_border_style: l.side_border_style,
        side_bg_style: l.side_bg_style,
        text_style: l.text_style,
        status_style: l.status_style,
    })
}

// ── Browser ──

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BrowserSeverity {
    Warning,
    Error,
}

#[derive(Clone, PartialEq)]
pub struct BrowserInputs {
    pub entries: Rc<Vec<led_state::TreeEntry>>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub focused: bool,
    pub height: usize,
    pub side_width: u16,
    pub dir_style: Style,
    pub file_style: Style,
    pub selected_style: Style,
    pub selected_unfocused_style: Style,
    pub git_file_statuses: HashMap<CanonPath, std::collections::HashSet<FileStatus>>,
    pub git_modified_style: Style,
    pub git_added_style: Style,
    pub git_untracked_style: Style,
    pub diag_file_severities: HashMap<CanonPath, BrowserSeverity>,
    pub diag_error_style: Style,
    pub diag_warning_style: Style,
    pub pr_diff_files: std::collections::HashSet<CanonPath>,
    pub pr_comment_files: std::collections::HashSet<CanonPath>,
    pub pr_diff_style: Style,
    pub pr_comment_style: Style,
}

pub fn browser_inputs(s: &AppState) -> Option<BrowserInputs> {
    if s.file_search.is_some() {
        return None;
    }
    if s.find_file.as_ref().is_some_and(|ff| ff.show_side) {
        return None;
    }
    let dims = s.dims?;
    let theme = s.config_theme.as_ref()?.file.as_ref();

    let diag_file_severities = build_diag_severities(s);

    Some(BrowserInputs {
        entries: s.browser.entries.clone(),
        selected: s.browser.selected,
        scroll_offset: s.browser.scroll_offset,
        focused: s.focus == PanelSlot::Side,
        height: dims.buffer_height(),
        side_width: dims.side_panel_width,
        dir_style: style::resolve_cached(theme, &theme.browser.directory),
        file_style: style::resolve_cached(theme, &theme.browser.file),
        selected_style: style::resolve_cached(theme, &theme.browser.selected),
        selected_unfocused_style: style::resolve_cached(theme, &theme.browser.selected_unfocused),
        git_file_statuses: s.git.file_statuses.clone(),
        git_modified_style: style::resolve_cached(theme, &theme.git.modified),
        git_added_style: style::resolve_cached(theme, &theme.git.added),
        git_untracked_style: style::resolve_cached(theme, &theme.git.untracked),
        diag_file_severities,
        diag_error_style: style::resolve_cached(theme, &theme.diagnostics.error),
        diag_warning_style: style::resolve_cached(theme, &theme.diagnostics.warning),
        pr_diff_files: s
            .git
            .pr
            .as_ref()
            .map(|pr| pr.diff_files.keys().cloned().collect())
            .unwrap_or_default(),
        pr_comment_files: s
            .git
            .pr
            .as_ref()
            .map(|pr| pr.comments.keys().cloned().collect())
            .unwrap_or_default(),
        pr_diff_style: theme
            .pr
            .as_ref()
            .map(|pr| style::resolve_cached(theme, &pr.diff))
            .unwrap_or_else(|| Style::default().fg(ratatui::style::Color::DarkGray)),
        pr_comment_style: theme
            .pr
            .as_ref()
            .map(|pr| style::resolve_cached(theme, &pr.comment))
            .unwrap_or_else(|| Style::default().fg(ratatui::style::Color::Blue)),
    })
}

fn worst_severity(diags: &[led_lsp::Diagnostic]) -> Option<BrowserSeverity> {
    let mut worst: Option<BrowserSeverity> = None;
    for d in diags {
        let sev = match d.severity {
            led_lsp::DiagnosticSeverity::Error => BrowserSeverity::Error,
            led_lsp::DiagnosticSeverity::Warning => BrowserSeverity::Warning,
            _ => continue,
        };
        worst = Some(match worst {
            Some(w) => w.max(sev),
            None => sev,
        });
    }
    worst
}

fn build_diag_severities(s: &led_state::AppState) -> HashMap<CanonPath, BrowserSeverity> {
    let mut result = HashMap::new();
    // Open buffers
    for buf in s.buffers.values() {
        if let Some(path) = buf.path() {
            if let Some(w) = worst_severity(buf.status().diagnostics()) {
                result.insert(path.clone(), w);
            }
        }
    }
    result
}

fn directory_severity(
    file_severities: &HashMap<CanonPath, BrowserSeverity>,
    dir: &CanonPath,
) -> Option<BrowserSeverity> {
    let mut worst: Option<BrowserSeverity> = None;
    for (path, &sev) in file_severities {
        if path.starts_with(dir) && path != dir {
            worst = Some(match worst {
                Some(w) => w.max(sev),
                None => sev,
            });
        }
    }
    worst
}

pub fn build_browser_lines(b: &BrowserInputs) -> Rc<Vec<Line<'static>>> {
    let offset = b.scroll_offset.min(b.entries.len());
    let end = (offset + b.height).min(b.entries.len());
    let visible = &b.entries[offset..end];
    // Usable width inside the side panel (subtract 1 for border)
    let max_width = (b.side_width as usize).saturating_sub(1);

    let lines: Vec<Line<'static>> = visible
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let abs_idx = offset + i;
            let is_selected = abs_idx == b.selected;

            let indent = "  ".repeat(entry.depth);
            let icon = match &entry.kind {
                EntryKind::Directory { expanded: true } => "\u{25bd} ",
                EntryKind::Directory { expanded: false } => "\u{25b7} ",
                EntryKind::File => "  ",
            };

            // Resolve git status for this entry
            let status_display = match &entry.kind {
                EntryKind::File => {
                    let statuses = b.git_file_statuses.get(&entry.path);
                    statuses.and_then(git::resolve_display)
                }
                EntryKind::Directory { .. } => {
                    let statuses = git::directory_statuses(&b.git_file_statuses, &entry.path);
                    git::resolve_display(&statuses)
                }
            };

            let git_style = status_display.as_ref().map(|sd| match sd.theme_key {
                "git.modified" => b.git_modified_style,
                "git.added" => b.git_added_style,
                "git.untracked" => b.git_untracked_style,
                _ => b.file_style,
            });

            // Resolve PR status for this entry (comment > diff)
            let pr_style = if b.pr_comment_files.contains(&entry.path) {
                Some(b.pr_comment_style)
            } else if b.pr_diff_files.contains(&entry.path) {
                Some(b.pr_diff_style)
            } else {
                None
            };

            // Resolve diagnostic severity for this entry
            let diag_sev = match &entry.kind {
                EntryKind::File => b.diag_file_severities.get(&entry.path).copied(),
                EntryKind::Directory { .. } => {
                    directory_severity(&b.diag_file_severities, &entry.path)
                }
            };

            let diag_style = diag_sev.map(|sev| match sev {
                BrowserSeverity::Error => b.diag_error_style,
                BrowserSeverity::Warning => b.diag_warning_style,
            });

            let entry_style = if is_selected && b.focused {
                b.selected_style
            } else if is_selected {
                b.selected_unfocused_style
                    .patch(diag_style.or(git_style).or(pr_style).unwrap_or_default())
            } else {
                diag_style
                    .or(git_style)
                    .or(pr_style)
                    .unwrap_or(match &entry.kind {
                        EntryKind::Directory { .. } => b.dir_style,
                        EntryKind::File => b.file_style,
                    })
            };

            // Determine status character and its style
            let status = diag_sev
                .map(|sev| {
                    let ch = match sev {
                        BrowserSeverity::Error => '\u{25CF}', // ●
                        BrowserSeverity::Warning => '\u{25CF}',
                    };
                    (ch, diag_style.unwrap_or(entry_style))
                })
                .or_else(|| {
                    status_display.as_ref().map(|sd| {
                        let ch = match &entry.kind {
                            EntryKind::Directory { .. } => '\u{2022}', // •
                            EntryKind::File => sd.letter,
                        };
                        (ch, git_style.unwrap_or(entry_style))
                    })
                })
                .or_else(|| {
                    pr_style.map(|sty| {
                        let ch = if b.pr_comment_files.contains(&entry.path) {
                            'C'
                        } else {
                            'P'
                        };
                        (ch, sty)
                    })
                });

            match status {
                Some((status_char, marker_style)) => {
                    let name_text = format!("{}{}{}", indent, icon, entry.name);
                    let name_width = name_text.chars().count();
                    // Reserve 1 column for status character
                    let avail = max_width.saturating_sub(1);
                    let pad = avail.saturating_sub(name_width);
                    let name_part: String = if name_width > avail {
                        name_text.chars().take(avail).collect()
                    } else {
                        name_text
                    };
                    Line::from(vec![
                        Span::styled(format!("{}{:pad$}", name_part, "", pad = pad), entry_style),
                        Span::styled(
                            status_char.to_string(),
                            if is_selected {
                                entry_style
                            } else {
                                marker_style
                            },
                        ),
                    ])
                }
                None => {
                    let text = format!("{}{}{}", indent, icon, entry.name);
                    Line::from(Span::styled(text, entry_style))
                }
            }
        })
        .collect();

    Rc::new(lines)
}

// ── Find file completions ──

#[derive(Clone, PartialEq)]
pub struct FindFileCompletionInputs {
    pub completions: Vec<led_fs::FindFileEntry>,
    pub selected: Option<usize>,
    pub height: usize,
    pub dir_style: Style,
    pub file_style: Style,
    pub selected_style: Style,
    pub side_width: u16,
}

pub fn find_file_completion_inputs(s: &AppState) -> Option<FindFileCompletionInputs> {
    let ff = s.find_file.as_ref()?;
    if !ff.show_side {
        return None;
    }
    let dims = s.dims?;
    let theme = s.config_theme.as_ref()?.file.as_ref();
    Some(FindFileCompletionInputs {
        completions: ff.completions.clone(),
        selected: ff.selected,
        height: dims.buffer_height(),
        dir_style: style::resolve_cached(theme, &theme.browser.directory),
        file_style: style::resolve_cached(theme, &theme.browser.file),
        selected_style: style::resolve_cached(theme, &theme.browser.selected),
        side_width: dims.side_panel_width,
    })
}

pub fn build_find_file_completion_lines(f: &FindFileCompletionInputs) -> Rc<Vec<Line<'static>>> {
    if f.completions.is_empty() {
        return Rc::new(Vec::new());
    }

    // Scroll to keep selected visible
    let scroll = if let Some(sel) = f.selected {
        if sel < f.height {
            0
        } else {
            sel - f.height + 1
        }
    } else {
        0
    };

    // Usable width inside the side panel (subtract 1 for border)
    let max_width = (f.side_width as usize).saturating_sub(1);

    let lines: Vec<Line<'static>> = f
        .completions
        .iter()
        .skip(scroll)
        .take(f.height)
        .enumerate()
        .map(|(i, comp)| {
            let is_selected = f.selected == Some(scroll + i);
            let entry_style = if is_selected {
                f.selected_style
            } else if comp.is_dir {
                f.dir_style
            } else {
                f.file_style
            };

            let name = &comp.name;
            let char_count = name.chars().count();
            let text = if char_count > max_width && max_width > 1 {
                let truncated: String = name.chars().take(max_width.saturating_sub(1)).collect();
                format!("{truncated}\u{2026}")
            } else {
                format!("{name:max_width$}")
            };

            Line::from(Span::styled(text, entry_style))
        })
        .collect();

    Rc::new(lines)
}

// ── File search ──

#[derive(Clone, PartialEq)]
pub struct FileSearchInputs {
    pub query: String,
    pub cursor_pos: usize,
    pub case_sensitive: bool,
    pub use_regex: bool,
    pub results: Vec<led_state::file_search::FileGroup>,
    pub flat_hits: Vec<led_state::file_search::FlatHit>,
    pub selection: led_state::file_search::FileSearchSelection,
    pub scroll_offset: usize,
    pub focused: bool,
    pub height: usize,
    pub side_width: u16,
    // Replace
    pub replace_mode: bool,
    pub replace_text: String,
    // Styles
    pub input_style: Style,
    pub toggle_on_style: Style,
    pub toggle_off_style: Style,
    pub file_header_style: Style,
    pub hit_style: Style,
    pub match_style: Style,
    pub selected_style: Style,
    pub selected_unfocused_style: Style,
}

pub fn file_search_inputs(s: &AppState) -> Option<FileSearchInputs> {
    let fs = s.file_search.as_ref()?;
    let dims = s.dims?;
    let theme = s.config_theme.as_ref()?.file.as_ref();
    Some(FileSearchInputs {
        query: fs.query.clone(),
        cursor_pos: fs.cursor_pos,
        case_sensitive: fs.case_sensitive,
        use_regex: fs.use_regex,
        results: fs.results.clone(),
        flat_hits: fs.flat_hits.clone(),
        selection: fs.selection,
        scroll_offset: fs.scroll_offset,
        focused: s.focus == PanelSlot::Side,
        height: dims.buffer_height(),
        side_width: dims.side_panel_width,
        replace_mode: fs.replace_mode,
        replace_text: fs.replace_text.clone(),
        input_style: style::resolve_cached(theme, &theme.file_search.input),
        toggle_on_style: style::resolve_cached(theme, &theme.file_search.toggle_on),
        toggle_off_style: style::resolve_cached(theme, &theme.file_search.toggle_off),
        file_header_style: style::resolve_cached(theme, &theme.file_search.file_header),
        hit_style: style::resolve_cached(theme, &theme.file_search.hit),
        match_style: style::resolve_cached(theme, &theme.file_search.match_),
        selected_style: style::resolve_cached(theme, &theme.file_search.selected),
        selected_unfocused_style: style::resolve_cached(
            theme,
            &theme.file_search.selected_unfocused,
        ),
    })
}

pub fn build_file_search_lines(f: &FileSearchInputs) -> Rc<Vec<Line<'static>>> {
    let width = (f.side_width as usize).saturating_sub(1);
    let mut lines: Vec<Line<'static>> = Vec::new();

    let selected_result_idx = match f.selection {
        led_state::file_search::FileSearchSelection::Result(i) => Some(i),
        _ => None,
    };

    // Row 0: toggle buttons
    let case_style = if f.case_sensitive {
        f.toggle_on_style
    } else {
        f.toggle_off_style
    };
    let regex_style = if f.use_regex {
        f.toggle_on_style
    } else {
        f.toggle_off_style
    };
    let replace_toggle_style = if f.replace_mode {
        f.toggle_on_style
    } else {
        f.toggle_off_style
    };
    lines.push(Line::from(vec![
        Span::styled(" Aa ", case_style),
        Span::raw(" "),
        Span::styled(" .* ", regex_style),
        Span::raw(" "),
        Span::styled(" => ", replace_toggle_style),
    ]));

    // Row 1: query input (highlight when selected)
    let query_style = if f.selection == led_state::file_search::FileSearchSelection::SearchInput {
        f.selected_style
    } else {
        f.input_style
    };
    let display_query: String = if f.query.chars().count() > width {
        f.query.chars().take(width).collect()
    } else {
        format!("{:<w$}", f.query, w = width)
    };
    lines.push(Line::from(Span::styled(display_query, query_style)));

    // Row 2 (optional): replace input
    let header_rows = if f.replace_mode {
        let replace_style =
            if f.selection == led_state::file_search::FileSearchSelection::ReplaceInput {
                f.selected_style
            } else {
                f.input_style
            };
        let display_replace: String = if f.replace_text.chars().count() > width {
            f.replace_text.chars().take(width).collect()
        } else {
            format!("{:<w$}", f.replace_text, w = width)
        };
        lines.push(Line::from(Span::styled(display_replace, replace_style)));
        3
    } else {
        2
    };

    // Remaining rows: results
    let results_height = f.height.saturating_sub(header_rows);
    if results_height == 0 {
        return Rc::new(lines);
    }

    let selected_flat = selected_result_idx.and_then(|i| f.flat_hits.get(i));

    let mut display_row: usize = 0;
    let mut rendered: usize = 0;

    for (gi, group) in f.results.iter().enumerate() {
        // File header row
        if display_row >= f.scroll_offset {
            if rendered >= results_height {
                break;
            }
            let header_text: String = if group.relative.chars().count() > width {
                group.relative.chars().take(width).collect()
            } else {
                group.relative.clone()
            };
            let padded = format!("{:<w$}", header_text, w = width);
            lines.push(Line::from(Span::styled(padded, f.file_header_style)));
            rendered += 1;
        }
        display_row += 1;

        // Hit rows
        for (hi, hit) in group.hits.iter().enumerate() {
            if display_row >= f.scroll_offset {
                if rendered >= results_height {
                    break;
                }
                let is_selected =
                    selected_flat.map_or(false, |fl| fl.group_idx == gi && fl.hit_idx == hi);

                let base_style = if is_selected {
                    if f.focused {
                        f.selected_style
                    } else {
                        f.selected_unfocused_style
                    }
                } else {
                    f.hit_style
                };

                let match_s = if is_selected {
                    base_style
                } else {
                    f.match_style
                };

                let prefix = format!("{:>4}: ", *hit.row + 1);
                let avail = width.saturating_sub(prefix.chars().count());
                let spans = build_hit_spans(hit, &prefix, avail, base_style, match_s);
                lines.push(Line::from(spans));
                rendered += 1;
            }
            display_row += 1;
        }
    }

    Rc::new(lines)
}

fn build_hit_spans<'a>(
    hit: &led_state::file_search::SearchHit,
    prefix: &str,
    avail: usize,
    base_style: Style,
    match_style: Style,
) -> Vec<Span<'a>> {
    let line_chars: Vec<char> = hit.line_text.chars().collect();
    let match_char_start = hit.line_text[..hit.match_start].chars().count();
    let match_char_end = hit.line_text[..hit.match_end].chars().count();

    let match_len = match_char_end - match_char_start;
    let context_before = avail.saturating_sub(match_len) / 2;
    let win_start = match_char_start.saturating_sub(context_before);
    let win_end = (win_start + avail).min(line_chars.len());
    let win_start = if win_end.saturating_sub(avail) < win_start {
        win_end.saturating_sub(avail)
    } else {
        win_start
    };

    let visible: String = line_chars[win_start..win_end].iter().collect();
    let ms_in_win = match_char_start.saturating_sub(win_start);
    let me_in_win = (match_char_end.saturating_sub(win_start)).min(visible.chars().count());

    let before: String = visible.chars().take(ms_in_win).collect();
    let matched: String = visible
        .chars()
        .skip(ms_in_win)
        .take(me_in_win - ms_in_win)
        .collect();
    let after: String = visible.chars().skip(me_in_win).collect();

    let pad_needed = avail.saturating_sub(visible.chars().count());
    let after_padded = format!("{after}{:pad$}", "", pad = pad_needed);

    let mut spans = vec![Span::styled(prefix.to_string(), base_style)];
    if !before.is_empty() {
        spans.push(Span::styled(before, base_style));
    }
    if !matched.is_empty() {
        spans.push(Span::styled(matched, match_style));
    }
    if !after_padded.is_empty() {
        spans.push(Span::styled(after_padded, base_style));
    }
    spans
}
