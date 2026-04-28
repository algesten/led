//! Popover (diagnostic hover) slice of the render frame.

use led_driver_terminal_core::{PopoverLine, PopoverModel, PopoverSeverity, Rect};
use led_state_browser::Focus;
use led_state_diagnostics::{Diagnostic, DiagnosticSeverity};
use std::sync::Arc;

use super::{GUTTER_WIDTH, TRAILING_RESERVED_COLS};
use crate::query::inputs::*;

// ── Popover (diagnostic hover) ───────────────────────────────────────

/// Max content width inside the popover box (excluding the 1-col
/// padding on each side). Matches legacy's ceiling so the wrap
/// looks identical in golden traces.
const POPOVER_MAX_CONTENT: usize = 58;

/// Build the cursor-line diagnostic popover.
///
/// Returns `None` (no popover) when any of:
/// - An overlay has input focus (find-file / file-search / isearch).
/// - The browser is focused.
/// - No active tab, or the active buffer isn't loaded yet.
/// - `DiagnosticsStates` has nothing for the active path.
/// - The stamped content hash doesn't match the buffer's current
///   content (no-smear: hide rather than show stale).
/// - No Error/Warning diagnostic covers the cursor row
///   (Info/Hint are silent, matching legacy).
pub fn popover_model(
    edits: EditedBuffersInput<'_>,
    tabs: TabsActiveInput<'_>,
    overlays: OverlaysInput<'_>,
    browser: BrowserUiInput<'_>,
    diagnostics: DiagnosticsStatesInput<'_>,
    editor_area: Rect,
) -> Option<PopoverModel> {
    if overlays.find_file.is_some()
        || overlays.file_search.is_some()
        || overlays.isearch.is_some()
    {
        return None;
    }
    if *browser.focus == Focus::Side {
        return None;
    }
    let id = (*tabs.active)?;
    let tab = tabs.open.iter().find(|t| t.id == id)?;
    let eb = edits.buffers.get(&tab.path)?;
    let bd = diagnostics.by_path.get(&tab.path)?;
    let current_hash = led_core::EphemeralContentHash::of_rope(&eb.rope).persist();
    if bd.hash != current_hash {
        return None;
    }
    let cursor_row = tab.cursor.line;
    let mut hits: Vec<&Diagnostic> = bd
        .diagnostics
        .iter()
        .filter(|d| {
            cursor_row >= d.start_line
                && cursor_row <= d.end_line
                && matches!(
                    d.severity,
                    DiagnosticSeverity::Error | DiagnosticSeverity::Warning
                )
        })
        .collect();
    if hits.is_empty() {
        return None;
    }
    // Stable order: error before warning, then by start position.
    hits.sort_by_key(|d| (severity_rank(d.severity), d.start_line, d.start_col));
    // Dedupe by `(severity, message)`. Parsers (especially
    // rust-analyzer) cascade identical "expected X" errors across
    // several positions on the same line — showing the same
    // sentence three times in a vertical stack is noise.
    hits.dedup_by(|a, b| a.severity == b.severity && a.message == b.message);

    let max_content = POPOVER_MAX_CONTENT.min(editor_area.rows.saturating_sub(0) as usize);
    // Cap width by editor area so the box never exceeds the edit
    // region minus a 2-col margin (padding inside the box).
    let max_content = max_content.min(
        editor_area
            .cols
            .saturating_sub(4)
            .max(1) as usize,
    );

    let mut lines: Vec<PopoverLine> = Vec::new();
    for (i, d) in hits.iter().enumerate() {
        if i > 0 {
            lines.push(PopoverLine {
                text: Arc::<str>::from(""),
                severity: None,
            });
        }
        let severity = Some(match d.severity {
            DiagnosticSeverity::Error => PopoverSeverity::Error,
            DiagnosticSeverity::Warning => PopoverSeverity::Warning,
            DiagnosticSeverity::Info => PopoverSeverity::Info,
            DiagnosticSeverity::Hint => PopoverSeverity::Hint,
        });
        for wrapped in word_wrap(&d.message, max_content) {
            lines.push(PopoverLine {
                text: Arc::<str>::from(wrapped.as_str()),
                severity,
            });
        }
    }

    // Anchor in absolute terminal coords: cursor position inside
    // the editor area. Mirrors `visible_cursor` so the popover
    // sits exactly over the cursor cell, gutter offset included
    // and sub-line column for soft-wrapped lines.
    let scroll_row = tab.scroll.top;
    if cursor_row < scroll_row {
        return None;
    }
    let row_in_area = (cursor_row - scroll_row) as u16;
    if row_in_area >= editor_area.rows {
        return None;
    }
    use led_core::col_to_sub_line;
    let content_cols = (editor_area.cols as usize)
        .saturating_sub(GUTTER_WIDTH)
        .saturating_sub(TRAILING_RESERVED_COLS);
    // Cursor on a row past the rope's last line — anchor at col 0
    // (no content to translate). Same-row diagnostics still pop.
    let col_within_cells = if cursor_row >= eb.rope.len_lines() {
        0
    } else {
        let (_, cells) = col_to_sub_line(tab.cursor.col, eb.rope.line(cursor_row), content_cols);
        cells
    };
    let anchor_x = editor_area
        .x
        .saturating_add(GUTTER_WIDTH as u16)
        .saturating_add(col_within_cells as u16);
    let anchor_y = editor_area.y.saturating_add(row_in_area);

    Some(PopoverModel {
        lines: Arc::new(lines),
        anchor: (anchor_x, anchor_y),
    })
}

fn severity_rank(s: DiagnosticSeverity) -> u8 {
    match s {
        DiagnosticSeverity::Error => 0,
        DiagnosticSeverity::Warning => 1,
        DiagnosticSeverity::Info => 2,
        DiagnosticSeverity::Hint => 3,
    }
}

/// Ratatui-compatible greedy word wrap. Breaks at ASCII spaces;
/// long tokens with no whitespace are split at the width. Output
/// lines have no trailing whitespace.
fn word_wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        if word_len > width {
            // Split oversized word across lines.
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            let chars: Vec<char> = word.chars().collect();
            for chunk in chars.chunks(width) {
                out.push(chunk.iter().collect());
            }
            continue;
        }
        let sep_len = if line.is_empty() { 0 } else { 1 };
        if line.chars().count() + sep_len + word_len > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}
