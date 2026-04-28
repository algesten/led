//! Completion / code-action / rename popups.

use led_driver_terminal_core::{BodyModel, Rect};
use std::sync::Arc;

use super::GUTTER_WIDTH;
use crate::query::inputs::*;

/// Maximum rows the completion popup displays at once. Matches
/// legacy's fixed window — users scroll the list with
/// Up/Down beyond this.
const COMPLETION_MAX_ROWS: usize = 10;

/// "What should the completion popup look like right now?"
///
/// Builds the visible window (`scroll..scroll + COMPLETION_MAX_ROWS`)
/// from the active session's filtered items, computes the label /
/// detail column widths the painter needs, and anchors the popup
/// at the cursor's terminal position.
///
/// Returns `None` when no session is active, the session's tab
/// isn't the current active tab (user navigated away), or the
/// filtered list is empty (the dispatch-side dismiss should have
/// caught this, but guard anyway so a stale frame doesn't paint
/// an empty box).
#[drv::memo(single)]
pub fn completion_popup_model<'c, 't>(
    completions: CompletionsSessionInput<'c>,
    tabs: TabsActiveInput<'t>,
    editor_area: Rect,
) -> Option<led_driver_terminal_core::CompletionPopupModel> {
    use led_driver_terminal_core::{CompletionPopupModel, CompletionRow};
    let session = completions.session.as_ref()?;
    let active = (*tabs.active)?;
    if session.tab != active {
        return None;
    }
    if session.filtered.is_empty() {
        return None;
    }
    // Visible window — scroll..scroll + MAX, clamped to the
    // filtered length. `selected` has already been scroll-
    // adjusted by the overlay dispatch (ensure_visible); the
    // memo just paints what it sees.
    let total = session.filtered.len();
    let scroll = session.scroll.min(total.saturating_sub(1));
    let end = (scroll + COMPLETION_MAX_ROWS).min(total);
    let mut rows: Vec<CompletionRow> = Vec::with_capacity(end - scroll);
    let mut label_width: usize = 0;
    let mut detail_width: usize = 0;
    for &item_ix in &session.filtered[scroll..end] {
        let item = &session.items[item_ix];
        let label_cols = item.label.chars().count();
        label_width = label_width.max(label_cols);
        if let Some(d) = item.detail.as_ref() {
            detail_width = detail_width.max(d.chars().count());
        }
        rows.push(CompletionRow {
            label: item.label.clone(),
            detail: item.detail.clone(),
        });
    }
    // Anchor at cursor's terminal position. Painter flips above
    // or below based on remaining rows below the cursor.
    let tab = tabs.open.iter().find(|t| t.id == session.tab)?;
    let cursor_col = tab.cursor.col as u16;
    let cursor_row = tab.cursor.line as u16;
    let anchor = (
        editor_area.x.saturating_add(GUTTER_WIDTH as u16).saturating_add(cursor_col),
        editor_area.y.saturating_add(cursor_row),
    );
    let selected_in_window = session.selected.saturating_sub(scroll);
    Some(CompletionPopupModel {
        rows: Arc::new(rows),
        selected: selected_in_window,
        anchor,
        label_width: label_width.min(u16::MAX as usize) as u16,
        detail_width: detail_width.min(u16::MAX as usize) as u16,
    })
}

/// Build the code-action picker popup. Reuses `CompletionPopupModel`
/// because the painter for completion popups is the right
/// visual shape (list of titles) and we don't want two popup
/// paint paths.
///
/// Only the title is surfaced — legacy hides `kind` from the
/// picker (display.rs:972-983), so we follow suit and leave
/// `detail` empty.
pub fn code_action_popup_model<'e, 't>(
    lsp_extras: LspExtrasOverlayInput<'e>,
    tabs: TabsActiveInput<'t>,
    editor_area: Rect,
) -> Option<led_driver_terminal_core::CompletionPopupModel> {
    use led_driver_terminal_core::{CompletionPopupModel, CompletionRow};
    let picker = lsp_extras.code_actions.as_ref()?;
    if picker.items.is_empty() {
        return None;
    }
    let total = picker.items.len();
    let scroll = picker.scroll.min(total.saturating_sub(1));
    let end = (scroll + COMPLETION_MAX_ROWS).min(total);
    let mut rows: Vec<CompletionRow> = Vec::with_capacity(end - scroll);
    let mut label_width: usize = 0;
    let detail_width: usize = 0;
    for item in &picker.items[scroll..end] {
        let label_cols = item.title.chars().count();
        label_width = label_width.max(label_cols);
        rows.push(CompletionRow {
            label: item.title.clone(),
            detail: None,
        });
    }
    // Anchor at the active tab's cursor. The picker is a
    // transient modal — rendering it where completions render
    // is the most natural place.
    let active = (*tabs.active)?;
    let tab = tabs.open.iter().find(|t| t.id == active)?;
    let cursor_col = tab.cursor.col as u16;
    let cursor_row = tab.cursor.line as u16;
    let anchor = (
        editor_area.x.saturating_add(GUTTER_WIDTH as u16).saturating_add(cursor_col),
        editor_area.y.saturating_add(cursor_row),
    );
    let selected_in_window = picker.selected.saturating_sub(scroll);
    Some(CompletionPopupModel {
        rows: Arc::new(rows),
        selected: selected_in_window,
        anchor,
        label_width: label_width.min(u16::MAX as usize) as u16,
        detail_width: detail_width.min(u16::MAX as usize) as u16,
    })
}

/// Build the LSP rename overlay's in-buffer popup. Mirrors
/// legacy `OverlayContent::Rename`: anchor at one row below the
/// cursor (or the cursor row when there is no row below), at
/// the cursor's screen column. Width is sized to fit
/// `" Rename: <input> "` with a 2-col padding tail so the box
/// reads cleanly even with short input.
pub fn rename_popup_model(
    lsp_extras: LspExtrasOverlayInput<'_>,
    body: &BodyModel,
    editor_area: Rect,
) -> Option<led_driver_terminal_core::RenamePopupModel> {
    use led_driver_terminal_core::RenamePopupModel;
    let state = lsp_extras.rename.as_ref()?;
    let (cur_row, cur_col) = match body {
        BodyModel::Content {
            cursor: Some((r, c)),
            ..
        } => (*r, *c),
        _ => return None,
    };
    // Legacy width: " Rename: " (9) + input chars + 2 trailing
    // padding cols. Keeps the box visibly distinct from
    // surrounding buffer content even on empty input.
    let input_chars = state.input.text.chars().count();
    let label_cols: u16 = 9; // " Rename: "
    let width_unclamped = label_cols
        .saturating_add(input_chars as u16)
        .saturating_add(2);
    // Cursor offset within `input.text` measured in chars (not
    // bytes) — `TextInput.cursor` is a byte index but always
    // sits on a char boundary by construction.
    let input_cursor_chars =
        state.input.text[..state.input.cursor].chars().count() as u16;
    let anchor_x = editor_area.x.saturating_add(cur_col);
    let anchor_y_row = (cur_row as usize)
        .saturating_add(1)
        .min(editor_area.rows.saturating_sub(1) as usize) as u16;
    let anchor_y = editor_area.y.saturating_add(anchor_y_row);
    // Clamp width so the popup never spills past the editor's
    // right edge.
    let area_right = editor_area.x.saturating_add(editor_area.cols);
    let max_width = area_right.saturating_sub(anchor_x);
    let width = width_unclamped.min(max_width);
    if width < label_cols {
        return None;
    }
    Some(RenamePopupModel {
        input: Arc::<str>::from(state.input.text.as_str()),
        input_cursor: input_cursor_chars,
        anchor: (anchor_x, anchor_y),
        width,
    })
}
