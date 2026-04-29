//! Body slice of the render frame: the per-row content list, gutter
//! markers, cursor placement, and rebased syntax/diagnostic spans.

use led_driver_terminal_core::{BodyModel, Rect};
use led_state_diagnostics::{Diagnostic, DiagnosticSeverity};
use led_state_syntax::{TokenKind, TokenSpan};
use led_state_tabs::{Cursor, Scroll};
use ropey::Rope;
use std::sync::Arc;
use led_core::CanonPath;
use led_driver_buffers_core::LoadState;

use super::{GUTTER_WIDTH, TRAILING_RESERVED_COLS, chars_between};
use crate::query::inputs::*;

/// Bundled input for [`body_model`] — drv 0.4 nested-inputs
/// shape. Reduces the memo signature from 7 positional args to
/// one. Callers build one labelled struct literal; drv's
/// per-field equality walks into each projection normally.
#[derive(Copy, Clone, drv::Input)]
pub struct BodyInputs<'a> {
    pub edits: EditedBuffersInput<'a>,
    pub store: StoreLoadedInput<'a>,
    pub tabs: TabsActiveInput<'a>,
    pub overlays: OverlaysInput<'a>,
    pub syntax: SyntaxStatesInput<'a>,
    pub diagnostics: DiagnosticsStatesInput<'a>,
    pub git: GitStateInput<'a>,
    pub area: Rect,
}

/// Body slice of the render frame.
///
/// Reads the active tab's cursor + scroll to produce the visible line
/// slice and a body-relative cursor position. Scroll is source state
/// on [`Tab`]; dispatch maintains the "keep cursor visible" invariant
/// so the cursor is normally inside the returned window.
///
/// Prefers [`BufferEdits`] (the user-edited view) over [`BufferStore`]
/// (the disk snapshot). In steady state — loaded + seeded — the
/// edits branch always wins; the store fallback covers the brief
/// window between a load completion and the runtime's next
/// BufferEdits seed, plus Pending / Error paths that never made it
/// to `Ready`.
#[drv::memo(single)]
pub fn body_model<'a>(inputs: BodyInputs<'a>) -> BodyModel {
    let BodyInputs {
        edits,
        store,
        tabs,
        overlays,
        syntax,
        diagnostics,
        git,
        area,
    } = inputs;
    let Some(id) = *tabs.active else {
        return BodyModel::Empty;
    };
    let Some(tab) = tabs.open.iter().find(|t| t.id == id) else {
        return BodyModel::Empty;
    };
    if let Some(eb) = edits.buffers.get(&tab.path) {
        let highlight = active_body_match(&overlays, &tab.path, tab.scroll, area, &eb.rope);
        let spans = rebased_line_spans(syntax, edits, tab.path.clone());
        let selection = normalized_selection(tab.mark, tab.cursor);
        // Diagnostics + git markers carry an anchor hash they were
        // computed against. Renderer translates each marker's
        // anchor-row to a current-row via `eb.row_delta_for(anchor)`,
        // hiding markers whose anchor row was touched / deleted
        // since stamp. The 99% case (no edits since stamp) returns
        // an empty `RowDelta` and the lookup is O(1).
        let bd = diagnostics.by_path.get(&tab.path);
        let diag_row_delta = bd.and_then(|bd| eb.row_delta_for(bd.hash));
        let diags = if diag_row_delta.is_some() {
            bd.map(|bd| bd.diagnostics.as_slice())
        } else {
            None
        };
        let gls = git.line_statuses.get(&tab.path);
        let git_row_delta = gls.and_then(|gls| eb.row_delta_for(gls.anchor_hash));
        let line_statuses = gls
            .filter(|_| git_row_delta.is_some())
            .map(|gls| gls.statuses.as_slice());
        return render_content(RenderContentArgs {
            rope: &eb.rope,
            cursor: tab.cursor,
            selection,
            scroll: tab.scroll,
            area,
            match_highlight: highlight,
            rebased_tokens: spans.as_deref().map(|v: &Vec<TokenSpan>| v.as_slice()),
            diagnostics: diags,
            diag_row_delta: diag_row_delta.as_ref(),
            git_line_statuses: line_statuses,
            git_row_delta: git_row_delta.as_ref(),
        });
    }
    // No BufferEdits entry yet — the load hasn't been seeded
    // into the edit-view source. Fall back to what `BufferStore`
    // knows. On Pending / Error / absent we render a blank body
    // (tildes, no content), never an in-buffer placeholder or
    // error message — matches legacy's "empty buffer during the
    // brief load window" UX and keeps surface errors off the
    // user's editing canvas. M21 will surface genuine load
    // failures via the status-bar alert system instead.
    let empty_rope: Arc<Rope> = Arc::new(Rope::new());
    let rope_ref: &Rope = match store.loaded.get(&tab.path) {
        Some(LoadState::Ready(rope)) => rope.as_ref(),
        None | Some(LoadState::Pending) | Some(LoadState::Error(_)) => &empty_rope,
    };
    let highlight = active_body_match(&overlays, &tab.path, tab.scroll, area, rope_ref);
    // No EditedBuffer entry yet → no row_delta. The buffer hasn't
    // accepted any edits, so anchor coords == current coords.
    let line_statuses = git
        .line_statuses
        .get(&tab.path)
        .map(|gls| gls.statuses.as_slice());
    render_content(RenderContentArgs {
        rope: rope_ref,
        cursor: tab.cursor,
        selection: normalized_selection(tab.mark, tab.cursor),
        scroll: tab.scroll,
        area,
        match_highlight: highlight,
        rebased_tokens: None,
        diagnostics: None,
        diag_row_delta: None,
        git_line_statuses: line_statuses,
        git_row_delta: None,
    })
}

/// Normalize `tab.mark` + `tab.cursor` to an ordered selection
/// span. Returns `None` when no mark is set or the mark coincides
/// with the cursor (empty region).
fn normalized_selection(mark: Option<Cursor>, cursor: Cursor) -> Option<(Cursor, Cursor)> {
    let mark = mark?;
    let mk = (mark.line, mark.col);
    let ck = (cursor.line, cursor.col);
    if mk == ck {
        return None;
    }
    if mk <= ck {
        Some((mark, cursor))
    } else {
        Some((cursor, mark))
    }
}

/// Apply any edits the user made between the parse and now onto
/// the token list so spans still line up with current rope
/// offsets. Memoised on `(SyntaxStatesInput, EditedBuffersInput,
/// path)` — cursor moves, scrolls, overlay changes and resize
/// all invalidate `body_model` but not this memo, so the
/// rebased token list is reused as long as the tokens, the
/// parse-anchor rope and the current rope haven't changed. The
/// output is `Arc`-wrapped so a cache hit is a pointer clone.
///
/// Returns `None` when there's no syntax state yet, no tokens,
/// or no buffer for `path` — caller interprets each as
/// "render plain".
#[drv::memo(single)]
pub fn rebased_line_spans<'s, 'b>(
    syntax: SyntaxStatesInput<'s>,
    edits: EditedBuffersInput<'b>,
    path: CanonPath,
) -> Option<Arc<Vec<TokenSpan>>> {
    let state = syntax.by_path.get(&path)?;
    let eb = edits.buffers.get(&path)?;
    if state.tokens.is_empty() {
        return None;
    }
    // Drv-pure rebase: derive the ops from the two rope
    // snapshots the parser saw vs. the current rope. No
    // history-index counter that could drift across undo/redo.
    let Some(prev_rope) = state.tree_rope.as_ref() else {
        return Some(state.tokens.clone());
    };
    if Arc::ptr_eq(prev_rope, &eb.rope) {
        return Some(state.tokens.clone());
    }
    let Some(diff) = led_state_syntax::RopeDiff::between(prev_rope, &eb.rope) else {
        return Some(state.tokens.clone());
    };
    // Append-past-last-token fast path: if the diff sits
    // entirely past the last token's end (typing trailing
    // whitespace, appending at EOF, editing the tail of the
    // buffer past the highlighted region), no token positions
    // move and the existing Arc<Vec<TokenSpan>> is still
    // correct. Skip the to_vec() and the Arc::new wrap.
    let last_token_end = state
        .tokens
        .last()
        .map(|t| t.char_end)
        .unwrap_or(0);
    if diff.char_start >= last_token_end {
        return Some(state.tokens.clone());
    }
    Some(Arc::new(led_state_syntax::rebase_tokens(
        &state.tokens,
        diff.rebase_ops(),
    )))
}

/// Resolve the file-search overlay's current hit into a visible-row
/// match highlight for the active tab. Returns `None` unless the
/// overlay is open, has a Result selection pointing at a loaded hit,
/// and the hit's path matches `active_path`. The result coords are
/// body-visible (post-scroll, post-gutter) so the painter consumes
/// them directly.
fn active_body_match(
    overlays: &OverlaysInput<'_>,
    active_path: &CanonPath,
    scroll: Scroll,
    area: Rect,
    rope: &Rope,
) -> Option<led_driver_terminal_core::BodyMatch> {
    use led_core::{SubLine, col_to_sub_line, sub_line_count};
    let state = overlays.file_search.as_ref()?;
    let led_state_file_search::FileSearchSelection::Result(i) = state.selection else {
        return None;
    };
    let hit = state.flat_hits.get(i)?;
    if &hit.path != active_path {
        return None;
    }
    let line = hit.line.saturating_sub(1);
    let body_rows = area.rows as usize;
    if body_rows == 0 || line < scroll.top {
        return None;
    }
    let cols = area.cols as usize;
    let content_cols = cols
        .saturating_sub(GUTTER_WIDTH)
        .saturating_sub(TRAILING_RESERVED_COLS);
    let match_char_len = chars_between(&hit.preview, hit.match_start, hit.match_end);
    let col_start_char = hit.col.saturating_sub(1);
    if line >= rope.len_lines() {
        return None;
    }
    let hit_slice = rope.line(line);
    // The hit's `col` is a CHAR index from the file-search driver;
    // convert to grapheme col before consulting wrap geometry.
    let match_gcol = led_core::char_to_grapheme_col(hit_slice, col_start_char);
    let match_end_gcol =
        led_core::char_to_grapheme_col(hit_slice, col_start_char + match_char_len);
    let (match_sub, cells_within) =
        col_to_sub_line(match_gcol, hit_slice, content_cols);
    // Walk sub-line counts to find the visible-row index for
    // (line, match_sub).
    let mut row: usize = 0;
    let mut ln = scroll.top;
    let mut sub_start = scroll.top_sub_line.0;
    while ln < line {
        let subs = sub_line_count(rope.line(ln), content_cols);
        let remaining = subs.saturating_sub(sub_start);
        row = row.saturating_add(remaining);
        ln += 1;
        sub_start = 0;
    }
    if match_sub.0 < sub_start {
        return None;
    }
    row = row.saturating_add(match_sub.0 - sub_start);
    if row >= body_rows {
        return None;
    }
    // Columns of the match *within this sub-line*, in display cells,
    // clamped to the sub-line's content width.
    let (_, end_cells_within) =
        col_to_sub_line(match_end_gcol, hit_slice, content_cols);
    // If the end is on a later sub, clamp to content_cols.
    let rel_start = cells_within.min(content_cols);
    let rel_end = end_cells_within.min(content_cols).max(rel_start);
    if rel_end <= rel_start {
        return None;
    }
    let _ = SubLine(0); // keep import without warning in edge conditions
    Some(led_driver_terminal_core::BodyMatch {
        row: row as u16,
        col_start: (rel_start + GUTTER_WIDTH) as u16,
        col_end: (rel_end + GUTTER_WIDTH) as u16,
    })
}

/// Bundle of inputs for [`render_content`]. Plain helper struct
/// (not a `drv::Input` projection) — this is internal helper
/// plumbing called by the `body_model` memo, not a memo itself.
struct RenderContentArgs<'a> {
    rope: &'a Rope,
    cursor: Cursor,
    /// Active mark→cursor selection, normalized so `.0 <= .1` in
    /// `(line, grapheme_col)` order. `None` when no mark is set or
    /// mark and cursor coincide. Per-row clipping happens inside
    /// [`render_content`].
    selection: Option<(Cursor, Cursor)>,
    scroll: Scroll,
    area: Rect,
    match_highlight: Option<led_driver_terminal_core::BodyMatch>,
    rebased_tokens: Option<&'a [TokenSpan]>,
    diagnostics: Option<&'a [Diagnostic]>,
    /// Sparse line-level invalidation for diagnostics. When set,
    /// the renderer translates each diagnostic's anchor-row to a
    /// current-row via `current_for_anchor`, dropping any whose
    /// anchor row was touched / deleted since the diagnostic was
    /// stamped. `None` means "no translation needed" (anchor
    /// matched current verbatim, the fast path).
    diag_row_delta: Option<&'a led_state_buffer_edits::RowDelta>,
    git_line_statuses: Option<&'a [led_core::git::LineStatus]>,
    /// Same as `diag_row_delta` but anchored against the buffer's
    /// disk-content hash at git-scan time.
    git_row_delta: Option<&'a led_state_buffer_edits::RowDelta>,
}

fn render_content(args: RenderContentArgs<'_>) -> BodyModel {
    use led_driver_terminal_core::BodyLine;
    use led_core::{SubLine, line_layout};

    let RenderContentArgs {
        rope,
        cursor,
        selection,
        scroll,
        area,
        match_highlight,
        rebased_tokens,
        diagnostics,
        diag_row_delta,
        git_line_statuses,
        git_row_delta,
    } = args;

    let body_rows = area.rows as usize;
    let line_count = rope.len_lines();
    let cols = area.cols as usize;
    let content_cols = cols
        .saturating_sub(GUTTER_WIDTH)
        .saturating_sub(TRAILING_RESERVED_COLS);

    let mut lines: Vec<BodyLine> = Vec::with_capacity(body_rows);
    let mut ln = scroll.top;
    let mut sub = scroll.top_sub_line;

    // Per-logical-line layout cached so we walk graphemes once per
    // line, not once per sub-line query. Refresh whenever `ln`
    // advances past the cached line.
    let mut layout_for: Option<usize> = None;
    let mut layout: Vec<led_core::SubLineRange> = Vec::new();
    let mut full_line: String = String::new();
    // Selection bounds projected onto the current logical line. `None`
    // when this line falls outside the selection. Cached alongside
    // `layout` so each sub-line lookup is O(1) instead of repeating
    // the grapheme walk inside `col_to_sub_line`.
    let mut line_sel: Option<LineSelectionBounds> = None;

    for _ in 0..body_rows {
        if ln >= line_count {
            lines.push(BodyLine {
                text: "~ ".to_string(),
                spans: Vec::new(),
                gutter_diagnostic: None,
                gutter_category: None,
                diagnostics: Vec::new(),
                selection: None,
            });
            continue;
        }
        if layout_for != Some(ln) {
            let line_slice = rope.line(ln);
            layout = line_layout(line_slice, content_cols);
            full_line.clear();
            full_line.extend(line_slice.chars());
            strip_trailing_newline(&mut full_line);
            line_sel = selection.and_then(|(start, end)| {
                project_selection_to_line(start, end, ln, &layout, line_slice, content_cols)
            });
            layout_for = Some(ln);
        }
        let max_sub = layout.len();
        // Clamp `sub` to a valid range; a previous width change
        // could have left `scroll.top_sub_line` past the end of
        // the current line. Render the first sub-line instead
        // of producing an empty row.
        if sub.0 >= max_sub {
            sub = SubLine(0);
        }
        let range = layout[sub.0];
        let col_start = range.char_start;
        let col_end = range.char_end;
        let line_char_start = rope.line_to_char(ln);
        let slice: String = full_line.chars().skip(col_start).take(col_end - col_start).collect();
        let sub_char_start = line_char_start + col_start;
        let is_continued = sub.0 + 1 < max_sub;
        let mut s = String::with_capacity(cols);
        s.push_str("  ");
        // Expand tabs to 4 spaces so the painter doesn't ship a
        // raw `\t` byte to vt100 (which would jump the cursor to
        // the next 8-col tab stop, leaving a one-cell gap and
        // shifting everything right). Matches legacy
        // `core/src/wrap.rs::expand_tabs` (also 4-space).
        for ch in slice.chars() {
            if ch == '\t' {
                s.push_str("    ");
            } else {
                s.push(ch);
            }
        }
        if is_continued {
            // Non-last sub-line: emit `<content><\>`. Wrap
            // geometry reserves exactly one trailing col for the
            // glyph (wrap_width = content_cols - 1), so `\` lands
            // at the editor area's last column, flush against the
            // terminal's right edge — no interior blank, no
            // trailing blank. Matches emacs's display.
            s.push('\\');
        }
        let spans = rebased_tokens
            .map(|tokens| {
                tokens_to_line_spans(
                    tokens,
                    sub_char_start,
                    col_end - col_start,
                    content_cols,
                )
            })
            .unwrap_or_default();
        let (gutter_diag, row_diagnostics) = diagnostics
            .map(|diags| {
                diagnostics_for_sub_line(
                    diags,
                    diag_row_delta,
                    ln,
                    col_start,
                    col_end,
                    content_cols,
                )
            })
            .unwrap_or_default();
        // Merged gutter category (M19 D7): the highest-precedence
        // `IssueCategory` for the gutter bar (git / PR only). Only
        // paints on the first sub-line of a wrapped row — matches
        // legacy's "col 1 marker on chunk 0".
        let is_first_sub = sub == SubLine(0);
        let gutter_category = if is_first_sub {
            merged_gutter_category(git_line_statuses, git_row_delta, ln)
        } else {
            None
        };
        let row_selection = line_sel.and_then(|bounds| {
            clip_selection_to_sub(&bounds, sub, &layout, area.cols as usize)
        });
        lines.push(BodyLine {
            text: s,
            spans,
            gutter_diagnostic: gutter_diag,
            gutter_category,
            diagnostics: row_diagnostics,
            selection: row_selection,
        });
        // Advance to the next visible sub-line; cross into the
        // next logical line when we run past the current one's
        // sub-line count.
        sub = SubLine(sub.0 + 1);
        if sub.0 >= max_sub {
            ln += 1;
            sub = SubLine(0);
        }
    }

    BodyModel::Content {
        lines: Arc::new(lines),
        cursor: visible_cursor(cursor, scroll, area, rope, content_cols),
        match_highlight,
    }
}

/// Project the buffer-wide diagnostic list onto one rendered
/// sub-line: pick the highest-severity diagnostic whose range
/// intersects the logical line for the gutter mark (so every
/// sub-line of a wrapped line carries the dot), and emit an
/// underline clipped to the sub-line's `[sub_col_start, sub_col_end)`
/// range — diagnostics that fall outside the sub-line simply
/// don't appear on that row.
///
/// Severity ordering for the gutter: Error > Warning > Info > Hint.
fn diagnostics_for_sub_line(
    diags: &[Diagnostic],
    row_delta: Option<&led_state_buffer_edits::RowDelta>,
    line_num: usize,
    sub_col_start: usize,
    sub_col_end: usize,
    content_cols: usize,
) -> (
    Option<DiagnosticSeverity>,
    Vec<led_driver_terminal_core::BodyDiagnostic>,
) {
    let mut gutter: Option<DiagnosticSeverity> = None;
    let mut out = Vec::new();
    // Translate the current-coordinate `line_num` back to its
    // anchor-coordinate row. Diagnostics' `start_line` / `end_line`
    // are stamped in anchor coordinates; we filter the entire row
    // out if the anchor row was touched / deleted (no marker survives).
    let anchor_line = match row_delta {
        Some(delta) => match delta.anchor_for_current(line_num) {
            Some(r) => r,
            None => return (None, Vec::new()),
        },
        None => line_num,
    };
    for d in diags {
        if anchor_line < d.start_line || anchor_line > d.end_line {
            continue;
        }
        // Legacy filters Info / Hint out of both gutter dots and
        // inline underlines (display.rs:357-365 for gutter,
        // 506-508 for underlines). They're still available for
        // diagnostic counts + cursor popover, but don't paint
        // chrome — too noisy given how many info notes a typical
        // LSP emits.
        if !matches!(d.severity, DiagnosticSeverity::Error | DiagnosticSeverity::Warning) {
            continue;
        }
        // Gutter tracks "any Err/Warn on this logical line" — a
        // wrapped line shows a dot on every sub-line so the user
        // sees it no matter which part of the line they're on.
        gutter = Some(match gutter {
            Some(existing) => higher(existing, d.severity),
            None => d.severity,
        });
        // Diagnostic column range ON THIS LOGICAL LINE.
        // `d.start_line` / `d.end_line` are in anchor coords;
        // compare against the row we translated above.
        let line_col_start = if anchor_line == d.start_line { d.start_col } else { 0 };
        let line_col_end = if anchor_line == d.end_line {
            d.end_col
        } else {
            sub_col_end // clamped to sub-line end; spans run off visually
        };
        // Clip against the sub-line's column range, then make it
        // relative to the sub-line's own col 0.
        let clip_start = line_col_start.max(sub_col_start);
        let clip_end = line_col_end.min(sub_col_end);
        if clip_end <= clip_start {
            continue;
        }
        let rel_start = clip_start - sub_col_start;
        let rel_end = clip_end - sub_col_start;
        let vis_start = rel_start.min(content_cols) + GUTTER_WIDTH;
        let vis_end = rel_end.min(content_cols) + GUTTER_WIDTH;
        if vis_end <= vis_start {
            continue;
        }
        out.push(led_driver_terminal_core::BodyDiagnostic {
            col_start: vis_start as u16,
            col_end: vis_end as u16,
            severity: d.severity,
        });
    }
    (gutter, out)
}

/// Pre-resolved selection bounds for one logical line: where the
/// selection enters and leaves expressed as `(sub_line, cells)`
/// pairs, plus a flag that's true when the selection's logical end
/// is on a later line. Computed once per logical line in the body
/// loop and reused across every sub-line query for that line — the
/// expensive grapheme walk in `col_to_sub_line` happens at most
/// twice per logical line (one endpoint each), then sub-line
/// clipping is constant time.
#[derive(Clone, Copy)]
struct LineSelectionBounds {
    sub_at_start: led_core::SubLine,
    cells_at_start: usize,
    sub_at_end: led_core::SubLine,
    cells_at_end: usize,
    extends_past_line: bool,
}

/// Project the buffer-wide mark→cursor selection onto one logical
/// line. Returns `None` when the line falls outside the selection;
/// otherwise the bounds describe where the selection enters and
/// leaves this line in `(sub_line, cells)` coordinates.
fn project_selection_to_line(
    sel_start: Cursor,
    sel_end: Cursor,
    ln: usize,
    layout: &[led_core::SubLineRange],
    line_slice: ropey::RopeSlice<'_>,
    content_cols: usize,
) -> Option<LineSelectionBounds> {
    use led_core::{SubLine, col_to_sub_line};
    if ln < sel_start.line || ln > sel_end.line {
        return None;
    }
    let last_sub_idx = layout.len().saturating_sub(1);
    let last_sub_cells = layout.get(last_sub_idx).map(|r| r.cells).unwrap_or(0);
    let (sub_at_start, cells_at_start) = if ln == sel_start.line {
        col_to_sub_line(sel_start.col, line_slice, content_cols)
    } else {
        (SubLine(0), 0)
    };
    let (sub_at_end, cells_at_end) = if ln == sel_end.line {
        col_to_sub_line(sel_end.col, line_slice, content_cols)
    } else {
        (SubLine(last_sub_idx), last_sub_cells)
    };
    Some(LineSelectionBounds {
        sub_at_start,
        cells_at_start,
        sub_at_end,
        cells_at_end,
        extends_past_line: ln < sel_end.line,
    })
}

/// Clip pre-resolved [`LineSelectionBounds`] to one rendered
/// sub-line. Returns the gutter-included display-cell range plus
/// the `extends` flag the painter uses to pad multi-line
/// selections out to the editor's right edge. Cells are clamped
/// to the body area's right edge so a wide-grapheme selection
/// past the viewport stops at the edge instead of overflowing.
fn clip_selection_to_sub(
    bounds: &LineSelectionBounds,
    sub: led_core::SubLine,
    layout: &[led_core::SubLineRange],
    area_cols: usize,
) -> Option<led_driver_terminal_core::BodySelection> {
    let our_sub = sub.0;
    if bounds.sub_at_start.0 > our_sub || bounds.sub_at_end.0 < our_sub {
        return None;
    }
    let left_cells = if bounds.sub_at_start.0 == our_sub {
        bounds.cells_at_start
    } else {
        0
    };
    let our_cells = layout.get(our_sub).map(|r| r.cells).unwrap_or(0);
    let right_cells = if bounds.sub_at_end.0 == our_sub {
        bounds.cells_at_end
    } else {
        our_cells
    };
    let col_start = (GUTTER_WIDTH + left_cells).min(area_cols) as u16;
    let col_end = (GUTTER_WIDTH + right_cells.max(left_cells)).min(area_cols) as u16;
    // `extends` only matters on the last sub-line of a logical
    // line: non-last subs already carry the `\` continuation
    // glyph in the trailing cells, so there's no gap to pad.
    let last_sub_idx = layout.len().saturating_sub(1);
    let is_last_sub_of_line = our_sub >= last_sub_idx;
    let selection_continues = bounds.extends_past_line || bounds.sub_at_end.0 > our_sub;
    let extends = is_last_sub_of_line && selection_continues;
    // Suppress only when there's truly nothing to paint — no inline
    // run AND no trailing pad. An empty line mid-selection has
    // `col_start == col_end` but `extends == true`, so the painter
    // still fills its row with `theme.selection`, matching legacy's
    // "pad to text_width on selected-through lines" behaviour.
    if col_end <= col_start && !extends {
        return None;
    }
    Some(led_driver_terminal_core::BodySelection {
        col_start,
        col_end,
        extends,
    })
}

/// Pick the precedence-winning `IssueCategory` for the gutter
/// bar (col 0) at `row` from git / PR line statuses. LSP severity
/// is intentionally excluded — diagnostics get their own glyph in
/// gutter col 1 (the `●`), so painting the bar from LSP too would
/// double up the indicator. Mirrors legacy `display.rs:328` which
/// queries only `buffer_line_annotations` (git + PR diff/comment,
/// no LSP). The precedence ladder in `IssueCategory::precedence`
/// still includes LSP because other consumers (browser
/// `resolve_display`) tie-break across all categories.
pub(crate) fn merged_gutter_category(
    line_statuses: Option<&[led_core::git::LineStatus]>,
    row_delta: Option<&led_state_buffer_edits::RowDelta>,
    row: usize,
) -> Option<led_core::IssueCategory> {
    let statuses = line_statuses?;
    // Translate the current-coordinate `row` back to its
    // anchor-coordinate row before looking up. `None` means the
    // row was touched / deleted since the marker batch was
    // stamped — suppress the gutter glyph. When no delta is
    // present (fast path), current row IS anchor row.
    let anchor_row = match row_delta {
        Some(delta) => delta.anchor_for_current(row)?,
        None => row,
    };
    led_core::git::best_category_at(statuses, anchor_row)
}

fn higher(a: DiagnosticSeverity, b: DiagnosticSeverity) -> DiagnosticSeverity {
    use DiagnosticSeverity::*;
    fn rank(s: DiagnosticSeverity) -> u8 {
        match s {
            Error => 3,
            Warning => 2,
            Info => 1,
            Hint => 0,
        }
    }
    if rank(a) >= rank(b) { a } else { b }
}

/// Slice the buffer-wide token list into the subset that falls on a
/// single rendered row, translating char offsets into row-relative
/// column positions (gutter included, right-edge-clamped).
///
/// A span that crosses the row boundary is clipped to the row; a
/// span that ends past the truncation point is clipped to
/// `content_cols`. Tokens whose kind is `Default` are dropped
/// because emitting a span that styles nothing would force the
/// painter to reset unnecessarily.
pub(crate) fn tokens_to_line_spans(
    tokens: &[TokenSpan],
    line_char_start: usize,
    line_char_len: usize,
    content_cols: usize,
) -> Vec<led_driver_terminal_core::LineSpan> {
    let line_end = line_char_start + line_char_len;
    let mut out = Vec::new();
    // Binary-search the first span whose end > line_char_start to
    // skip the prefix that lives on earlier lines. Tokens are sorted
    // by (start, end) in the worker, so this stays O(log n + k).
    let start_ix = tokens.partition_point(|t| t.char_end <= line_char_start);
    for t in &tokens[start_ix..] {
        if t.char_start >= line_end {
            break;
        }
        if matches!(t.kind, TokenKind::Default) {
            continue;
        }
        let rel_start = t.char_start.saturating_sub(line_char_start);
        let rel_end = t.char_end.saturating_sub(line_char_start).min(line_char_len);
        let col_start = (rel_start.min(content_cols) + GUTTER_WIDTH) as u16;
        let col_end = (rel_end.min(content_cols) + GUTTER_WIDTH) as u16;
        if col_end <= col_start {
            continue;
        }
        out.push(led_driver_terminal_core::LineSpan {
            col_start,
            col_end,
            kind: t.kind,
        });
    }
    out
}

/// Count how many visible body rows sit between the scroll
/// anchor and the cursor's sub-line. Returns `None` when the
/// cursor is above the scroll anchor or past the body bottom.
///
/// Walks logical lines one at a time — on soft-wrap buffers each
/// logical line may contribute multiple visible rows. Cheap in
/// practice because `body_rows` is tiny (20-50) and the walk
/// short-circuits as soon as we pass the cursor's line.
fn visible_cursor(
    c: Cursor,
    s: Scroll,
    area: Rect,
    rope: &Rope,
    content_cols: usize,
) -> Option<(u16, u16)> {
    use led_core::{col_to_sub_line, line_layout};
    let body_rows = area.rows as usize;
    if body_rows == 0 || c.line < s.top {
        return None;
    }
    let line_count = rope.len_lines();
    if c.line >= line_count {
        return None;
    }
    // Cursor's own sub-line + display-cell column within that sub.
    let cur_slice = rope.line(c.line);
    let (cur_sub, cells_within) = col_to_sub_line(c.col, cur_slice, content_cols);
    // Count visible rows from (s.top, s.top_sub_line) to (c.line, cur_sub).
    // One layout walk per intervening logical line — `line_layout` is
    // the same primitive `render_content` uses, so on cache-hit ticks
    // both share the cost and we don't double-walk.
    let mut row: usize = 0;
    let mut ln = s.top;
    let mut sub_start = s.top_sub_line.0;
    while ln < c.line {
        if ln >= line_count {
            return None;
        }
        let subs = line_layout(rope.line(ln), content_cols).len();
        let remaining = subs.saturating_sub(sub_start);
        row = row.saturating_add(remaining);
        ln += 1;
        sub_start = 0;
    }
    if cur_sub.0 < sub_start {
        return None;
    }
    row = row.saturating_add(cur_sub.0 - sub_start);
    if row >= body_rows {
        return None;
    }
    let max_col = (area.cols as usize).saturating_sub(1);
    let col = (cells_within + GUTTER_WIDTH).min(max_col) as u16;
    Some((row as u16, col))
}


fn strip_trailing_newline(s: &mut String) {
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
}
