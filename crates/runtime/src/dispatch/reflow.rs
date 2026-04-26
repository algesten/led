//! `Ctrl-q` paragraph reflow (M23).
//!
//! Glue between [`led_text_reflow`] and the dispatch surface.
//! Resolves the active tab + buffer + file extension, calls
//! `reflow_at`, and either applies the returned plan or surfaces
//! the "Nothing to reflow" alert.

use std::sync::Arc;

use led_core::{CanonPath, PathChain};
use led_state_alerts::AlertState;
use led_state_buffer_edits::BufferEdits;
use led_state_tabs::Tabs;
use std::collections::HashMap;

use super::shared::{bump, with_active};

const ALERT_TTL_SECS: u64 = 2;

/// Run reflow on the active buffer at the cursor row.
///
/// - Empty buffer / no active tab / preview tab → no-op.
/// - Reflow returns `None` (cursor not on a reflowable region) →
///   info alert "Nothing to reflow", buffer untouched.
/// - Reflow returns `Some(plan)` → apply, record undo, no alert
///   on success (the visible reflow IS the feedback).
pub(super) fn reflow_paragraph(
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    alerts: &mut AlertState,
    path_chains: &HashMap<CanonPath, PathChain>,
) {
    let mut nothing = false;
    with_active(tabs, edits, |tab, eb| {
        if tab.preview {
            return;
        }
        let extension = file_extension(&tab.path, path_chains);
        let plan = led_text_reflow::reflow_at(&eb.rope, tab.cursor.line, extension.as_deref());

        let Some(plan) = plan else {
            nothing = true;
            return;
        };

        let before = tab.cursor;

        // Apply the replacement: remove the old slice, insert
        // the replacement at the same start_char.
        let mut rope = (*eb.rope).clone();
        let removed: String = rope.slice(plan.start_char..plan.end_char).chars().collect();
        rope.remove(plan.start_char..plan.end_char);
        rope.insert(plan.start_char, &plan.replacement);
        bump(eb, rope);

        // Cursor: clamp to the new buffer's bounds. If the
        // cursor row would be past the new line count, drop to
        // the last line; if the column would be past EOL, clamp.
        let new_line_count = eb.rope.len_lines().max(1);
        let new_row = tab.cursor.line.min(new_line_count - 1);
        let new_line_len = line_char_len(&eb.rope, new_row);
        let new_col = tab.cursor.col.min(new_line_len);
        tab.cursor.line = new_row;
        tab.cursor.col = new_col;
        tab.cursor.preferred_col = new_col;
        let after = tab.cursor;

        eb.history.finalise();
        eb.history.record_replace(
            plan.start_char,
            Arc::<str>::from(removed.as_str()),
            Arc::<str>::from(plan.replacement.as_str()),
            before,
            after,
            None,
        );
    });

    if nothing {
        alerts.set_info(
            "Nothing to reflow".to_string(),
            std::time::Instant::now(),
            std::time::Duration::from_secs(ALERT_TTL_SECS),
        );
    }
}

fn file_extension(
    path: &CanonPath,
    path_chains: &HashMap<CanonPath, PathChain>,
) -> Option<String> {
    // Prefer the user-typed name's extension (mirrors language
    // detection). Fall back to the canonical path's extension.
    if let Some(chain) = path_chains.get(path) {
        for p in chain.iter_paths() {
            if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                return Some(ext.to_string());
            }
        }
    }
    path.as_path()
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_string())
}

fn line_char_len(rope: &ropey::Rope, line: usize) -> usize {
    if line >= rope.len_lines() {
        return 0;
    }
    let slice = rope.line(line);
    let mut end = slice.len_chars();
    if end == 0 {
        return 0;
    }
    if slice.char(end - 1) == '\n' {
        end -= 1;
        if end > 0 && slice.char(end - 1) == '\r' {
            end -= 1;
        }
    }
    end
}
