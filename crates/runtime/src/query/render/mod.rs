//! Render-time memos: per-region models (tab-bar, body, status-bar,
//! side-panel, popover, popups) plus the top-level `render_frame`
//! that composes them.

pub mod body;
pub mod popover;
pub mod popups;
pub mod side_panel;
pub mod status_bar;
pub mod tab_bar;

use led_driver_terminal_core::{BodyModel, Frame, Layout};
use led_state_browser::Focus;

use super::inputs::*;

pub use body::{body_model, rebased_line_spans, BodyInputs};
pub use popover::popover_model;
pub use popups::{
    code_action_popup_model, completion_popup_model, rename_popup_model,
};
pub use side_panel::{side_panel_model, SidePanelInputs};
pub use status_bar::{status_bar_model, StatusBarInputs};
pub use tab_bar::tab_bar_model;

// Gutter width reserved on the left of every body row. M9 renders
// two blank cols; future milestones fill col 0 with git marks and
// col 1 with diagnostic severity.
pub(crate) const GUTTER_WIDTH: usize = 2;

/// Trailing column never written to on the right edge of the
/// editor area. Held at `0` now that the painter is
/// cell-grid-diff-based (see `driver-terminal/native/src/{buffer,render}.rs`):
/// it never emits `Clear(UntilNewLine)`, so writing the last
/// column is safe and the soft-wrap `\` lives in the true last
/// col of the terminal. The constant survives as a nameable
/// knob in case we ever need to reserve a gap again (e.g. for a
/// scroll indicator column); keeping it at `0` matches legacy
/// emacs/led behaviour.
pub(crate) const TRAILING_RESERVED_COLS: usize = 0;

/// Number of characters (not bytes) in `s[byte_start..byte_end]`.
/// Clamps a bad byte range to an empty slice rather than panicking —
/// the driver sets sensible offsets, but a defensive cast keeps
/// malformed hits from crashing the painter.
pub(super) fn chars_between(s: &str, byte_start: usize, byte_end: usize) -> usize {
    if byte_end <= byte_start
        || byte_end > s.len()
        || !s.is_char_boundary(byte_start)
        || !s.is_char_boundary(byte_end)
    {
        return 0;
    }
    s[byte_start..byte_end].chars().count()
}

/// Char count of an unsigned integer rendered via `Display` — used
/// to compute the width of the `"{line}"` segment in a hit row.
pub(super) fn count_chars_of_usize(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut n = n;
    let mut c = 0;
    while n > 0 {
        n /= 10;
        c += 1;
    }
    c
}

/// Top-level render model. Composes the per-region memos — each
/// independently cached in its own per-memo thread-local cache.
///
/// Bundle of every projection the top-level render memo reads.
/// Composed of 13 narrower projection inputs (plus the
/// `render_tick` scalar), each of which is itself a
/// `#[derive(drv::Input)]` — the drv 0.4 nested-inputs shape.
/// Callers construct one labelled struct literal instead of
/// positionally lining up fourteen arguments; the inner memo
/// takes this whole bundle, and drv's per-field `eq_static`
/// walks into each projection so a single-field change still
/// invalidates correctly.
#[derive(Copy, Clone, drv::Input)]
pub struct RenderInputs<'a> {
    pub term: TerminalDimsInput<'a>,
    pub edits: EditedBuffersInput<'a>,
    pub store: StoreLoadedInput<'a>,
    pub tabs: TabsActiveInput<'a>,
    pub alerts: AlertsInput<'a>,
    pub browser: BrowserUiInput<'a>,
    pub fs: FsTreeInput<'a>,
    pub overlays: OverlaysInput<'a>,
    pub syntax: SyntaxStatesInput<'a>,
    pub diagnostics: DiagnosticsStatesInput<'a>,
    pub lsp: LspStatusesInput<'a>,
    pub completions: CompletionsSessionInput<'a>,
    pub lsp_extras: LspExtrasOverlayInput<'a>,
    pub git: GitStateInput<'a>,
    /// M22 — `kbd_macro.recording` for the status-bar
    /// recording indicator. Narrow projection so per-keystroke
    /// pushes into `KbdMacroState.current` don't invalidate
    /// the render memo cache while a recording is in progress.
    pub kbd_macro: KbdMacroRecordingInput<'a>,
    /// Session flock outcome — drives the `(secondary)` prefix
    /// in the status bar when this process is attached as a
    /// non-primary instance.
    pub session: SessionPrimaryInput<'a>,
    /// Current frame in 80ms buckets. Used by the status-bar
    /// spinner formatter; the main loop quantises wall-clock
    /// millis to 80 so the memo only invalidates once per
    /// spinner frame, not on every recompute. Pin to `0` when
    /// no LSP server is busy so the memo stays warm.
    pub render_tick: u64,
}

#[drv::memo(single)]
pub fn render_frame<'a>(inputs: RenderInputs<'a>) -> Option<Frame> {
    let RenderInputs {
        term,
        edits,
        store,
        tabs,
        alerts,
        browser,
        fs,
        overlays,
        syntax,
        diagnostics,
        lsp,
        completions,
        lsp_extras,
        git,
        kbd_macro,
        session,
        render_tick,
    } = inputs;
    let dims = (*term.dims)?;
    let layout = Layout::compute(dims, *browser.visible);
    let tab_bar = tab_bar_model(tabs, edits);
    let body = body_model(BodyInputs {
        edits,
        store,
        tabs,
        overlays,
        syntax,
        diagnostics,
        git,
        area: layout.editor_area,
    });
    let status_bar = status_bar_model(StatusBarInputs {
        alerts,
        tabs,
        edits,
        overlays,
        diagnostics,
        lsp,
        lsp_extras,
        git,
        kbd_macro,
        session,
        render_tick,
    });
    let side_panel = layout
        .side_area
        .map(|area| {
            side_panel_model(SidePanelInputs {
                fs,
                browser,
                overlays,
                tabs,
                diagnostics,
                git,
                edits,
                rows: area.rows,
            })
        });
    let popover =
        popover_model(edits, tabs, overlays, browser, diagnostics, layout.editor_area);
    // Code-action picker wins when live — the rename overlay
    // and code-action picker are mutually exclusive with
    // completions (dispatch guards that in `run_command`), so
    // whichever is populated paints into the shared
    // `completion` slot of the frame.
    let completion = code_action_popup_model(lsp_extras, tabs, layout.editor_area)
        .or_else(|| completion_popup_model(completions, tabs, layout.editor_area));
    let rename_popup = rename_popup_model(lsp_extras, &body, layout.editor_area);
    // Cursor placement, in priority order:
    //
    // 1. Find-file overlay active → status-bar row, column = prompt
    //    length + overlay input cursor. Byte offsets are ASCII-safe
    //    for the English prompts; if `input` ever carries non-ASCII
    //    the overlay's own cursor field is already a char-boundary
    //    byte index, and we convert to display columns here.
    // 2. Side-panel focus → no cursor (M11 cursor-hide rule).
    // 3. Otherwise, map the body cursor from editor-area-relative
    //    coords to absolute terminal coords.
    let cursor = if let Some(popup) = rename_popup.as_ref() {
        // Inside the rename popup, after " Rename: " (9 cols) plus
        // however many input chars precede the input cursor.
        let prefix_cols: u16 = 9;
        Some((
            popup.anchor.0
                .saturating_add(prefix_cols)
                .saturating_add(popup.input_cursor),
            popup.anchor.1,
        ))
    } else if let Some(state) = overlays.find_file.as_ref() {
        let prefix_cols: u16 = match state.mode {
            led_state_find_file::FindFileMode::Open => 12, // " Find file: "
            led_state_find_file::FindFileMode::SaveAs => 10, // " Save as: "
        };
        let input_col = state.input.text[..state.input.cursor].chars().count() as u16;
        Some((
            prefix_cols.saturating_add(input_col),
            layout.status_bar.y,
        ))
    } else if *browser.focus == Focus::Side {
        None
    } else {
        match &body {
            BodyModel::Content {
                cursor: Some((row, col)),
                ..
            } => Some((
                layout.editor_area.x.saturating_add(*col),
                layout.editor_area.y.saturating_add(*row),
            )),
            _ => None,
        }
    };
    Some(Frame {
        tab_bar,
        body,
        status_bar,
        side_panel,
        popover,
        completion,
        rename_popup,
        layout,
        cursor,
        dims,
    })
}

