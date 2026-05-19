//! Side-panel slice of the render frame.
//!
//! Decomposed into three sibling memos — one per side-panel
//! mode — plus the top-level [`side_panel_model`] composition.
//! The composition is a plain `fn` (deliberately not annotated
//! `#[drv::memo]`) so caching at this level can't re-introduce
//! the mega-memo it exists to avoid. The sub-memos cache
//! independently:
//!
//! - [`side_panel_file_search`] — file-search overlay mode.
//!   Reads only `OverlaysInput` + `rows`, so a keystroke into
//!   the browser-mode side panel (or any unrelated source) does
//!   not invalidate this cache.
//! - [`side_panel_completions`] — find-file overlay completions
//!   list. Same narrow input shape.
//! - [`side_panel_browser`] — the workspace file-tree mode. Takes
//!   the full browser-derived bundle (fs / ui / tabs / edits) plus
//!   diagnostics + git for status decoration.
//!
//! Why decompose? The original single memo took every input the
//! three modes could read, so typing into the file-search box
//! mutated `overlays.file_search` and invalidated the browser
//! branch's cache (and vice versa) even though only one branch
//! ever runs per frame. With siblings, each mode only re-fires
//! when *its* narrow input changes.

use led_driver_fs_list_core::TreeEntryKind;
use led_driver_terminal_core::{
    RowStatus, SidePanelMode, SidePanelModel, SidePanelRow, SidePanelStatusCell, Style, Theme,
};
use led_state_browser::Focus;
use std::sync::Arc;

use super::{chars_between, count_chars_of_usize};
use crate::query::browser::*;
use crate::query::inputs::*;

/// Side-panel slice of the render frame. Walks the visible window
/// of `browser.entries` and produces one `SidePanelRow` per row.
/// Empty when the browser has no entries.
///
/// Overlay priority (highest first):
/// - File-search active → render its header (toggle row + query
///   input + optional replace input + results tree).
/// - Find-file overlay active with `show_side=true` → render the
///   completions list.
/// - Otherwise → render the file-browser tree.
///
/// Bundled input — drv 0.4 nested-inputs shape.
#[derive(Copy, Clone, drv::Input)]
pub struct SidePanelInputs<'a> {
    pub fs: FsTreeInput<'a>,
    pub browser: BrowserUiInput<'a>,
    pub overlays: OverlaysInput<'a>,
    pub tabs: TabsActiveInput<'a>,
    pub diagnostics: DiagnosticsStatesInput<'a>,
    pub git: GitStateInput<'a>,
    pub edits: EditedBuffersInput<'a>,
    pub theme: ThemeInput<'a>,
    pub rows: u16,
}

/// Plain composition of the three per-mode memos.
///
/// Not annotated `#[drv::memo]` on purpose — caching at this
/// level would defeat the decomposition. The sub-memos cache;
/// this function is the cheap glue that picks the winning mode.
pub fn side_panel_model<'a>(inputs: SidePanelInputs<'a>) -> SidePanelModel {
    let SidePanelInputs {
        fs,
        browser,
        overlays,
        tabs,
        diagnostics,
        git,
        edits,
        theme,
        rows,
    } = inputs;
    if let Some(model) = side_panel_file_search(overlays, theme, rows) {
        return model;
    }
    if let Some(model) = side_panel_completions(overlays, theme, rows) {
        return model;
    }
    side_panel_browser(SidePanelBrowserInputs {
        fs,
        browser,
        tabs,
        edits,
        diagnostics,
        git,
        theme,
        rows,
    })
}

/// File-search overlay mode. Returns `Some` when
/// `overlays.file_search` is active.
///
/// Reads only `OverlaysInput` + `rows`, mirroring the status-bar
/// slot pattern — taking the existing `OverlaysInput` projection
/// (rather than minting a one-field input per overlay) keeps
/// the projection surface small for marginal cache-hit gain.
#[drv::memo(single)]
pub fn side_panel_file_search<'a, 'b>(
    overlays: OverlaysInput<'a>,
    theme: ThemeInput<'b>,
    rows: u16,
) -> Option<SidePanelModel> {
    let state = overlays.file_search.as_ref()?;
    Some(file_search_side_panel(state, theme.theme, rows))
}

/// Find-file overlay completions list. Returns `Some` when
/// `overlays.find_file` is active *and* its `show_side` flag is
/// set — the overlay starts with `show_side = false` (status-bar
/// prompt only) and dispatch flips it to `true` after the first
/// arrow key.
#[drv::memo(single)]
pub fn side_panel_completions<'a, 'b>(
    overlays: OverlaysInput<'a>,
    theme: ThemeInput<'b>,
    rows: u16,
) -> Option<SidePanelModel> {
    let state = overlays.find_file.as_ref()?;
    if !state.show_side {
        return None;
    }
    Some(completions_side_panel(state, theme.theme, rows))
}

/// Workspace browser-tree mode. Always returns a model (possibly
/// empty when the tree has no entries) — this is the fallback
/// slot the composition lands on when neither overlay is active.
///
/// Takes the full browser bundle (fs + ui + tabs + edits) plus
/// diagnostics + git for per-row status decoration. `rows` is the
/// side-panel area height as a Copy scalar so memo input equality
/// is structural.
/// Bundled input for [`side_panel_browser`] — drv 0.4 nested-
/// inputs shape. Reduces the memo signature from 8 positional
/// arguments to one. Built from [`SidePanelInputs`] by
/// [`side_panel_model`].
#[derive(Copy, Clone, drv::Input)]
pub struct SidePanelBrowserInputs<'a> {
    pub fs: FsTreeInput<'a>,
    pub browser: BrowserUiInput<'a>,
    pub tabs: TabsActiveInput<'a>,
    pub edits: EditedBuffersInput<'a>,
    pub diagnostics: DiagnosticsStatesInput<'a>,
    pub git: GitStateInput<'a>,
    pub theme: ThemeInput<'a>,
    pub rows: u16,
}

#[drv::memo(single)]
pub fn side_panel_browser<'a>(inputs: SidePanelBrowserInputs<'a>) -> SidePanelModel {
    let SidePanelBrowserInputs {
        fs,
        browser,
        tabs,
        edits,
        diagnostics,
        git,
        theme,
        rows,
    } = inputs;
    let entries = browser_entries(BrowserDerivedInputs {
        fs,
        ui: browser,
        tabs,
        edits,
    });
    let selected = browser_selected_idx(&entries, browser.selected_path.as_ref());
    let rows = rows as usize;
    let start = *browser.scroll_offset;
    let end = start.saturating_add(rows).min(entries.len());
    let focused = *browser.focus == Focus::Side;
    // Per-file category map — used for both file rows (direct
    // lookup) and directory rows (union over descendants).
    let categories = file_categories_map(diagnostics, git);
    let mut out: Vec<SidePanelRow> = Vec::with_capacity(end.saturating_sub(start));
    for (i, entry) in entries[start..end].iter().enumerate() {
        let chevron = match entry.kind {
            TreeEntryKind::File => None,
            TreeEntryKind::Directory { expanded } => Some(expanded),
        };
        // Resolve category per legacy:
        //  - Files look up their own categories.
        //  - Directories aggregate child categories via
        //    `directory_categories`, then always render as a
        //    bullet (letter forced regardless of resolver).
        let status = match entry.kind {
            TreeEntryKind::File => categories
                .get(&entry.path)
                .and_then(crate::query::resolve_display)
                .map(|d| led_driver_terminal_core::RowStatus {
                    category: d.category,
                    letter: d.letter,
                }),
            TreeEntryKind::Directory { .. } => {
                let cats = crate::query::directory_categories(&categories, &entry.path);
                crate::query::resolve_display(&cats).map(|d| {
                    led_driver_terminal_core::RowStatus {
                        category: d.category,
                        // Directories always bullet — matches legacy
                        // display.rs:1396-1402.
                        letter: '\u{2022}',
                    }
                })
            }
        };
        let is_selected = start + i == selected;
        let (name_style, status_cell) = resolve_row_styles(RowStyleInputs {
            mode: SidePanelMode::Browser,
            focused,
            selected: is_selected,
            replaced: false,
            has_match_range: false,
            status,
            theme: theme.theme,
        });
        out.push(SidePanelRow {
            depth: entry.depth as u16,
            chevron,
            name: Arc::<str>::from(entry.name.as_str()),
            selected: is_selected,
            match_range: None,
            replaced: false,
            status,
            name_style,
            status_cell,
        });
    }
    SidePanelModel {
        rows: Arc::new(out),
        focused,
        mode: SidePanelMode::Browser,
    }
}

/// Build a side-panel model from the find-file completions list.
/// Selection highlights the arrow-selected row; `focused` is always
/// `false` because the side panel never "has focus" in overlay mode
/// — keystrokes go through the overlay's own handler, and the
/// painter uses the flag to distinguish focused vs unfocused
/// selection styling (M14b chrome theming).
fn completions_side_panel(
    state: &led_state_find_file::FindFileState,
    theme: &Theme,
    rows: u16,
) -> SidePanelModel {
    let rows = rows as usize;
    let end = state.completions.len().min(rows);
    let mut out: Vec<SidePanelRow> = Vec::with_capacity(end);
    for (i, entry) in state.completions[..end].iter().enumerate() {
        let is_selected = state.selected == Some(i);
        let (name_style, status_cell) = resolve_row_styles(RowStyleInputs {
            mode: SidePanelMode::Completions,
            // Completions overlay never holds focus on the side
            // panel — keystrokes go through the find-file overlay
            // handler. The painter resolves the selection style
            // accordingly (matches legacy "unfocused selection bar").
            focused: false,
            selected: is_selected,
            replaced: false,
            has_match_range: false,
            status: None,
            theme,
        });
        out.push(SidePanelRow {
            depth: 0,
            chevron: None,
            name: Arc::<str>::from(entry.name.as_str()),
            selected: is_selected,
            match_range: None,
            replaced: false,
            status: None,
            name_style,
            status_cell,
        });
    }
    SidePanelModel {
        rows: Arc::new(out),
        focused: false,
        mode: SidePanelMode::Completions,
    }
}

/// Build a side-panel model from the file-search overlay.
///
/// Layout:
/// - Row 0: toggle header " Aa   .*   =>" — the three toggles for
///   case-sensitive, regex, replace-mode. Later stages will style
///   active toggles distinctly (reverse video); for now the
///   characters appear regardless.
/// - Row 1: query input row.
/// - Row 2: replace input row — only when `replace_mode`.
/// - Rows 3+: results tree — one row per file group header, then
///   one row per hit formatted `"   <line>: <preview>"` (3-space
///   indent matching legacy). The tree scrolls to follow the
///   selection when the user arrows past the bottom edge; inputs
///   stay pinned on the first 1–2 rows.
///
/// `focused=false` because M14b chrome theming hasn't picked a
/// focused side-panel style for this overlay yet.
pub(crate) fn file_search_side_panel(
    state: &led_state_file_search::FileSearchState,
    theme: &Theme,
    rows: u16,
) -> SidePanelModel {
    let total = rows as usize;
    let mut out: Vec<SidePanelRow> = Vec::new();
    let mode = SidePanelMode::FileSearch {
        case_sensitive: state.case_sensitive,
        use_regex: state.use_regex,
        replace_mode: state.replace_mode,
    };

    if total == 0 {
        return SidePanelModel {
            rows: Arc::new(out),
            focused: false,
            mode,
        };
    }

    // Pre-resolve the two style outcomes used by every row in
    // FileSearch mode: an unselected row (default styling), and a
    // selected row (unfocused selection bar — the overlay never
    // gives the side panel focus, so the painter shows the dim
    // selection variant on the active input / hit row).
    let push_row = |out: &mut Vec<SidePanelRow>,
                    name: Arc<str>,
                    selected: bool,
                    match_range: Option<(u16, u16)>,
                    replaced: bool| {
        let (name_style, status_cell) = resolve_row_styles(RowStyleInputs {
            mode,
            focused: false,
            selected,
            replaced,
            has_match_range: match_range.is_some(),
            status: None,
            theme,
        });
        out.push(SidePanelRow {
            depth: 0,
            chevron: None,
            name,
            selected,
            match_range,
            replaced,
            status: None,
            name_style,
            status_cell,
        });
    };

    push_row(
        &mut out,
        Arc::<str>::from(" Aa   .*   =>"),
        false,
        None,
        false,
    );

    if total > out.len() {
        push_row(
            &mut out,
            Arc::<str>::from(state.query.text.as_str()),
            matches!(
                state.selection,
                led_state_file_search::FileSearchSelection::SearchInput
            ),
            None,
            false,
        );
    }
    if state.replace_mode && total > out.len() {
        push_row(
            &mut out,
            Arc::<str>::from(state.replace.text.as_str()),
            matches!(
                state.selection,
                led_state_file_search::FileSearchSelection::ReplaceInput
            ),
            None,
            false,
        );
    }

    // Selected flat-hit index (if the cursor is on a result row).
    let selected_hit_idx = match state.selection {
        led_state_file_search::FileSearchSelection::Result(i) => Some(i),
        _ => None,
    };

    // Rows remaining for the results tree after the pinned inputs.
    let tree_rows_avail = total.saturating_sub(out.len());
    if tree_rows_avail == 0 {
        return SidePanelModel {
            rows: Arc::new(out),
            focused: false,
            mode,
        };
    }

    // `scroll_offset` is maintained by dispatch's move_selection —
    // it already points at the correct top-of-tree row for the
    // current selection, so the renderer doesn't re-derive.
    let effective_scroll = state.scroll_offset;

    // Flatten results: one row per group header + one row per hit.
    let mut skipped = 0usize;
    let mut hit_idx: usize = 0;
    'outer: for group in state.results.iter() {
        // Group header row.
        if skipped < effective_scroll {
            skipped += 1;
        } else {
            if total <= out.len() {
                break 'outer;
            }
            push_row(
                &mut out,
                Arc::<str>::from(group.relative.as_str()),
                false,
                None,
                false,
            );
        }
        for hit in &group.hits {
            if skipped < effective_scroll {
                skipped += 1;
            } else {
                if total <= out.len() {
                    break 'outer;
                }
                let is_replaced = state
                    .hit_replacements
                    .get(hit_idx)
                    .and_then(|e| e.as_ref())
                    .is_some();
                let prefix_chars = 3 + count_chars_of_usize(hit.line) + 2;
                // Side panel content area is 24 cols (see Layout in
                // driver-terminal/core); the prefix eats `prefix_chars`,
                // the rest is what the preview can fill before the
                // border. Trim only when the raw preview wouldn't fit.
                let preview_budget = 24usize.saturating_sub(prefix_chars);
                let (preview, match_preview_idx) = trimmed_preview(hit, preview_budget);
                let match_len = chars_between(&hit.preview, hit.match_start, hit.match_end);
                let match_start = (prefix_chars + match_preview_idx) as u16;
                let match_end = match_start.saturating_add(match_len as u16);
                let name = format!("   {}: {}", hit.line, preview);
                // Suppress the match highlight on replaced rows —
                // the dim replaced style reads better without the
                // yellow/bold overlay competing.
                let match_range = if is_replaced {
                    None
                } else {
                    Some((match_start, match_end))
                };
                push_row(
                    &mut out,
                    Arc::<str>::from(name.as_str()),
                    selected_hit_idx == Some(hit_idx),
                    match_range,
                    is_replaced,
                );
            }
            hit_idx += 1;
        }
    }

    SidePanelModel {
        rows: Arc::new(out),
        focused: false,
        mode,
    }
}

/// Inputs to [`resolve_row_styles`]. Bundled as a struct so call
/// sites read top-to-bottom and the order is checked by the
/// compiler when a new field is added — the function used to take
/// seven positional bool / Option arguments which was easy to
/// transpose.
struct RowStyleInputs<'a> {
    mode: SidePanelMode,
    /// `true` when keyboard focus is on the side panel itself
    /// (Browser mode + `browser.focus == Side`). Drives the
    /// "loud" selection style on the focused row; unfocused
    /// selection is dimmer so users can tell which pane owns
    /// their input.
    focused: bool,
    selected: bool,
    /// `true` for file-search hit rows the user has already
    /// applied a per-hit replace to. Stomps the category style
    /// with `theme.search_hit_replaced` (dim grey) — same
    /// precedence as legacy.
    replaced: bool,
    /// `true` when the row carries a `match_range` overlay
    /// (file-search hit rows). The painter draws the matched
    /// substring with `theme.search_match` and the surrounding
    /// run with the base `name_style`; legacy forces the base to
    /// `Style::default()` so the highlight reads consistently
    /// regardless of any category styling that would otherwise
    /// land on this row.
    has_match_range: bool,
    status: Option<RowStatus>,
    theme: &'a Theme,
}

/// Resolve `(name_style, status_cell)` for one side-panel row.
///
/// Mirrors the pre-Theme-J painter cascade exactly:
///
/// - **Selected, focused** → `theme.browser_selected_focused` (loud).
/// - **Selected, unfocused, with status** → bg from
///   `theme.browser_selected_unfocused` patched with the marker's
///   `fg` so the row still reads as "this errored file is
///   selected".
/// - **Selected, unfocused, without status** →
///   `theme.browser_selected_unfocused`.
/// - **Replaced (file-search)** → `theme.search_hit_replaced`.
/// - **Match-range overlay** → `Style::default()` so the
///   `theme.search_match` highlight reads cleanly against the
///   surrounding run.
/// - **Has status** → `theme.category_style(status.category)`.
/// - **Otherwise** → `Style::default()`.
///
/// In Browser mode the right-most two cells (gap + status letter)
/// also pick up the selection style — when selected the bar reads
/// continuous across the whole row; otherwise the gap is plain
/// and the letter picks up the category style. Non-Browser modes
/// return `None` for `status_cell` (no column reserved).
fn resolve_row_styles(args: RowStyleInputs<'_>) -> (Style, Option<SidePanelStatusCell>) {
    let RowStyleInputs {
        mode,
        focused,
        selected,
        replaced,
        has_match_range,
        status,
        theme,
    } = args;
    // Pre-compute the selection style once; reused for both the
    // name region and the status-column cells.
    let sel_style_base = if focused {
        theme.browser_selected_focused
    } else {
        theme.browser_selected_unfocused
    };
    // For the unfocused-with-status case the marker fg shines
    // through the selection bg (legacy display.rs:1381-1389).
    let sel_style = if !focused && selected
        && let Some(s) = status
    {
        let marker = theme.category_style(s.category);
        Style {
            fg: marker.fg.or(sel_style_base.fg),
            bg: sel_style_base.bg,
            attrs: sel_style_base.attrs,
        }
    } else {
        sel_style_base
    };
    let name_style = if selected {
        sel_style
    } else if replaced {
        theme.search_hit_replaced
    } else if has_match_range {
        // Match-range case forces a default base — the matched
        // substring picks up `theme.search_match`, surrounding
        // text stays unstyled. Category styling on a match-range
        // row would clash with the highlight.
        Style::default()
    } else if let Some(s) = status {
        theme.category_style(s.category)
    } else {
        Style::default()
    };
    let status_cell = match mode {
        SidePanelMode::Browser => {
            // In Browser mode every row reserves the rightmost two
            // cells. Even when the row has no category, those cells
            // still take on the selection bg when selected, so the
            // highlight bar doesn't stop one col short of the edge.
            let gap_style = if selected {
                sel_style
            } else {
                Style::default()
            };
            let letter_style = if selected {
                sel_style
            } else if let Some(s) = status {
                theme.category_style(s.category)
            } else {
                Style::default()
            };
            Some(SidePanelStatusCell {
                gap_style,
                letter_style,
            })
        }
        SidePanelMode::Completions | SidePanelMode::FileSearch { .. } => None,
    };
    (name_style, status_cell)
}

/// Center-window trim for a hit's preview so the match sits in
/// the middle of the visible column. Returns the trimmed preview
/// and the 0-indexed char offset at which the match starts inside
/// it — the painter uses the second value to draw the match-
/// highlight segment.
///
/// Uses `hit.col` (1-indexed character offset) rather than
/// `match_start` (byte offset), so multi-byte UTF-8 content doesn't
/// miscount. Mirrors legacy `display.rs::file_search_hit_spans`
/// (centers the match within `avail`, clamps the window to the
/// preview length, no ellipsis — narrow column gets a literal
/// substring slice).
fn trimmed_preview(
    hit: &led_state_file_search::FileSearchHit,
    budget: usize,
) -> (String, usize) {
    let match_char_idx = hit.col.saturating_sub(1);
    let preview_chars: Vec<char> = hit.preview.chars().collect();
    let preview_len = preview_chars.len();
    if preview_len <= budget {
        return (hit.preview.clone(), match_char_idx);
    }
    let match_len = chars_between(&hit.preview, hit.match_start, hit.match_end);
    let context_before = budget.saturating_sub(match_len) / 2;
    let mut win_start = match_char_idx.saturating_sub(context_before);
    let win_end = (win_start + budget).min(preview_len);
    if win_end.saturating_sub(budget) < win_start {
        win_start = win_end.saturating_sub(budget);
    }
    let visible: String = preview_chars[win_start..win_end].iter().collect();
    let match_in_window = match_char_idx.saturating_sub(win_start);
    (visible, match_in_window)
}

/// Test helper — accept the budget the caller wants so each
/// test can verify the centering behaviour with a realistic
/// (or deliberately tiny) column budget.
#[cfg(test)]
pub(crate) fn trim_preview_at_budget(
    hit: &led_state_file_search::FileSearchHit,
    budget: usize,
) -> String {
    trimmed_preview(hit, budget).0
}
