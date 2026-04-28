//! Side-panel slice of the render frame.

use led_driver_terminal_core::{SidePanelModel, SidePanelRow};
use led_state_browser::{Focus, TreeEntryKind};
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
    pub rows: u16,
}

#[drv::memo(single)]
pub fn side_panel_model<'a>(inputs: SidePanelInputs<'a>) -> SidePanelModel {
    let SidePanelInputs {
        fs,
        browser,
        overlays,
        tabs,
        diagnostics,
        git,
        edits,
        rows,
    } = inputs;
    if let Some(state) = overlays.file_search.as_ref() {
        return file_search_side_panel(state, rows);
    }
    if let Some(state) = overlays.find_file.as_ref()
        && state.show_side
    {
        return completions_side_panel(state, rows);
    }
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
                .and_then(led_core::resolve_display)
                .map(|d| led_driver_terminal_core::RowStatus {
                    category: d.category,
                    letter: d.letter,
                }),
            TreeEntryKind::Directory { .. } => {
                let cats = led_core::directory_categories(&categories, &entry.path);
                led_core::resolve_display(&cats).map(|d| {
                    led_driver_terminal_core::RowStatus {
                        category: d.category,
                        // Directories always bullet — matches legacy
                        // display.rs:1396-1402.
                        letter: '\u{2022}',
                    }
                })
            }
        };
        out.push(SidePanelRow {
            depth: entry.depth as u16,
            chevron,
            name: Arc::<str>::from(entry.name.as_str()),
            selected: start + i == selected,
            match_range: None,
            replaced: false,
            status,
        });
    }
    SidePanelModel {
        rows: Arc::new(out),
        focused,
        mode: led_driver_terminal_core::SidePanelMode::Browser,
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
    rows: u16,
) -> SidePanelModel {
    let rows = rows as usize;
    let end = state.completions.len().min(rows);
    let mut out: Vec<SidePanelRow> = Vec::with_capacity(end);
    for (i, entry) in state.completions[..end].iter().enumerate() {
        out.push(SidePanelRow {
            depth: 0,
            chevron: None,
            name: Arc::<str>::from(entry.name.as_str()),
            selected: state.selected == Some(i),
            match_range: None,
            replaced: false,
            status: None,
        });
    }
    SidePanelModel {
        rows: Arc::new(out),
        focused: false,
        mode: led_driver_terminal_core::SidePanelMode::Completions,
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
    rows: u16,
) -> SidePanelModel {
    let total = rows as usize;
    let mut out: Vec<SidePanelRow> = Vec::new();
    let mode = led_driver_terminal_core::SidePanelMode::FileSearch {
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

    out.push(SidePanelRow {
        depth: 0,
        chevron: None,
        name: Arc::<str>::from(" Aa   .*   =>"),
        selected: false,
        match_range: None,
        replaced: false,
            status: None,
    });

    if total > out.len() {
        out.push(SidePanelRow {
            depth: 0,
            chevron: None,
            name: Arc::<str>::from(state.query.text.as_str()),
            selected: matches!(
                state.selection,
                led_state_file_search::FileSearchSelection::SearchInput
            ),
            match_range: None,
            replaced: false,
            status: None,
        });
    }
    if state.replace_mode && total > out.len() {
        out.push(SidePanelRow {
            depth: 0,
            chevron: None,
            name: Arc::<str>::from(state.replace.text.as_str()),
            selected: matches!(
                state.selection,
                led_state_file_search::FileSearchSelection::ReplaceInput
            ),
            match_range: None,
            replaced: false,
            status: None,
        });
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
            out.push(SidePanelRow {
                depth: 0,
                chevron: None,
                name: Arc::<str>::from(group.relative.as_str()),
                selected: false,
                match_range: None,
                replaced: false,
            status: None,
            });
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
                out.push(SidePanelRow {
                    depth: 0,
                    chevron: None,
                    name: Arc::<str>::from(name.as_str()),
                    selected: selected_hit_idx == Some(hit_idx),
                    // Suppress the match highlight on replaced rows
                    // — the dim replaced style reads better without
                    // the yellow/bold overlay competing.
                    match_range: if is_replaced {
                        None
                    } else {
                        Some((match_start, match_end))
                    },
                    replaced: is_replaced,
                    status: None,
                });
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
