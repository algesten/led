//! Status-bar slice of the render frame.

use led_driver_terminal_core::StatusBarModel;
use led_state_tabs::Tab;
use std::sync::Arc;

use crate::query::inputs::*;

/// Status-bar slice of the render frame.
///
/// Priority chain (highest wins):
/// 1. **Confirm-kill prompt** — blocks other content; no position
///    indicator. Matches legacy dismiss-on-first-keystroke UX.
/// 2. **Info alert** — transient; shown alongside the position.
/// 3. **Warn alert** — persistent; shown alongside the position with
///    the `is_warn` flag set (painter renders white-on-red-bold).
/// 4. **Default** — left is `"  ●"` when the active buffer is dirty
///    else empty; right is `"L<row>:C<col> "` (1-indexed human
///    coords, trailing space).
///
/// All strings are `Arc<str>` so cache-hit clones of
/// [`StatusBarModel`] are a pointer copy.
///
/// Bundled input — drv 0.4 nested-inputs shape. Reduces the
/// memo signature from 8 positional args to 1.
#[derive(Copy, Clone, drv::Input)]
pub struct StatusBarInputs<'a> {
    pub alerts: AlertsInput<'a>,
    pub tabs: TabsActiveInput<'a>,
    pub edits: EditedBuffersInput<'a>,
    pub overlays: OverlaysInput<'a>,
    pub diagnostics: DiagnosticsStatesInput<'a>,
    pub lsp: LspStatusesInput<'a>,
    pub lsp_extras: LspExtrasOverlayInput<'a>,
    pub git: GitStateInput<'a>,
    /// M22 — `kbd_macro.recording` for the macro-recording
    /// indicator that replaces the default-left while
    /// recording. Narrow projection so recorded-keystroke
    /// pushes don't invalidate this memo.
    pub kbd_macro: KbdMacroRecordingInput<'a>,
    /// Session flock outcome — used to prefix the right half
    /// with `(secondary) ` when this process didn't acquire the
    /// primary flock for the workspace. The indicator stays
    /// hidden until `init_done` so we don't flash it during the
    /// brief startup window before `Restored` arrives.
    pub session: SessionPrimaryInput<'a>,
    pub render_tick: u64,
}

#[drv::memo(single)]
pub fn status_bar_model<'a>(inputs: StatusBarInputs<'a>) -> StatusBarModel {
    let StatusBarInputs {
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
    } = inputs;
    // The rename overlay used to take the status-bar prompt slot
    // here; legacy renders it as an in-buffer popup anchored on
    // the row below the cursor instead. See `rename_popup_model`.
    let _ = lsp_extras;
    // Priority 0b — in-buffer isearch prompt. Matches legacy
    // `display.rs` "Failing search:" / "Search:" wording so the
    // failed-state and live-state share a single prefix slot.
    if let Some(state) = overlays.isearch.as_ref() {
        let hint_len = state.query.hint.as_ref().map(|h| h.len() + 1).unwrap_or(0);
        let mut left = String::with_capacity(state.query.text.len() + 18 + hint_len);
        if state.failed {
            left.push_str(" Failing search: ");
        } else {
            left.push_str(" Search: ");
        }
        left.push_str(&state.query.text);
        if let Some(hint) = state.query.hint.as_ref() {
            left.push(' ');
            left.push_str(hint);
        }
        return StatusBarModel {
            left: Arc::from(left),
            right: Arc::from(""),
            is_warn: false,
        };
    }

    // Priority 0 — find-file overlay prompt. Replaces the whole
    // status bar content: left is `Find file: <input>` /
    // `Save as: <input>`, right is empty (no position indicator
    // while the overlay has focus). Matches legacy goldens.
    //
    // An active `hint` (e.g. "[No match]") appends after one space
    // of padding — Emacs-style transient feedback.
    if let Some(state) = overlays.find_file.as_ref() {
        let label = match state.mode {
            led_state_find_file::FindFileMode::Open => "Find file",
            led_state_find_file::FindFileMode::SaveAs => "Save as",
        };
        let hint_len = state.input.hint.as_ref().map(|h| h.len() + 1).unwrap_or(0);
        let mut left = String::with_capacity(state.input.text.len() + label.len() + 3 + hint_len);
        left.push(' ');
        left.push_str(label);
        left.push_str(": ");
        left.push_str(&state.input.text);
        if let Some(hint) = state.input.hint.as_ref() {
            left.push(' ');
            left.push_str(hint);
        }
        return StatusBarModel {
            left: Arc::from(left),
            right: Arc::from(""),
            is_warn: false,
        };
    }

    // Priority 1 — confirm-kill prompt.
    if let Some(kill_id) = *alerts.confirm_kill {
        let name = tabs
            .open
            .iter()
            .find(|t| t.id == kill_id)
            .and_then(|t| t.path.file_name().map(|os| os.to_string_lossy().into_owned()))
            .unwrap_or_default();
        return StatusBarModel {
            left: Arc::from(format!(" Kill buffer '{name}'? (y/N) ")),
            right: Arc::from(""),
            is_warn: false,
        };
    }

    let right = position_string(tabs, edits, diagnostics, session);

    // Priority 2 — info alert.
    if let Some(msg) = alerts.info.as_deref() {
        let mut left = String::with_capacity(msg.len() + 1);
        left.push(' ');
        left.push_str(msg);
        return StatusBarModel {
            left: Arc::from(left),
            right,
            is_warn: false,
        };
    }

    // Priority 3 — warn alert (first-arrived).
    if let Some((_, msg)) = alerts.warns.first() {
        let mut left = String::with_capacity(msg.len() + 1);
        left.push(' ');
        left.push_str(msg);
        return StatusBarModel {
            left: Arc::from(left),
            right,
            is_warn: true,
        };
    }

    // M22 — Macro-recording indicator. Per `ui-chrome.md`
    // § "Status bar" (table row "macro-recording indicator"),
    // while `kbd_macro.recording` is true the default-left is
    // *replaced* by a fixed " Defining kbd macro..." string.
    // Lower priority than alerts and overlays so the transient
    // start / end alerts still show through their TTL; higher
    // priority than the branch / dirty / lsp composition below.
    if *kbd_macro.recording {
        return StatusBarModel {
            left: Arc::from(" Defining kbd macro..."),
            right,
            is_warn: false,
        };
    }

    // Priority 4 — default left half: ` {branch}{modified}{lsp}`.
    // Legacy composes this as ` {branch}{modified}{pr}{lsp}`; PR
    // lands at M27. `lsp_progress_message` always returns `Some`
    // once a server is registered, so "rust-analyzer" stays
    // visible both during indexing and idle. The branch segment
    // is empty when `git.branch` is `None` (detached HEAD or
    // non-repo workspace) so the bar collapses back to the
    // pre-M19 shape automatically.
    let dirty = active_is_dirty(tabs, edits);
    let modified = if dirty { " \u{25cf}" } else { "" };
    let branch = git.branch.as_deref().unwrap_or("");
    let branch_segment = if branch.is_empty() {
        String::new()
    } else {
        format!(" {branch}")
    };
    let lsp_str = lsp_progress_message(lsp, render_tick).unwrap_or_default();
    let left: Arc<str> = Arc::from(format!(" {branch_segment}{modified}{lsp_str}"));
    StatusBarModel {
        left,
        right,
        is_warn: false,
    }
}

fn active_tab<'t>(tabs: TabsActiveInput<'t>) -> Option<&'t Tab> {
    let id = (*tabs.active)?;
    tabs.open.iter().find(|t| t.id == id)
}

fn active_is_dirty(tabs: TabsActiveInput<'_>, edits: EditedBuffersInput<'_>) -> bool {
    let Some(tab) = active_tab(tabs) else {
        return false;
    };
    edits
        .buffers
        .get(&tab.path)
        .map(|eb| eb.dirty())
        .unwrap_or(false)
}

/// Format one LSP server's status line matching legacy's
/// `format_lsp_status` (`/crates/ui/src/display.rs:803` on
/// main). Shape:
///
/// - Busy, no detail:    `  ⠋ rust-analyzer`
/// - Busy, with detail:  `  ⠋ rust-analyzer  ⠹ indexing crates`
/// - Idle, with detail:  `  rust-analyzer  indexing crates`
/// - Idle, no detail:    `  rust-analyzer`
/// - Empty server name:  `""` (no row)
///
/// `render_tick` is the current time in 80ms buckets so the
/// spinner animates across frames: each 80ms bucket advances
/// one frame in a 10-frame braille cycle. Two spinners are used
/// when detail is present, with an offset between them so they
/// animate out of phase (matches legacy's 400-bucket offset).
pub(crate) fn format_lsp_status(server_name: &str, busy: bool, detail: Option<&str>, render_tick: u64) -> String {
    if server_name.is_empty() {
        return String::new();
    }
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let spinner_char = |offset: u64| -> char {
        FRAMES[((render_tick + offset) as usize) % FRAMES.len()]
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
                // Legacy offsets by 400ms-worth (= 5 buckets) so
                // the two spinners animate staggered.
                format!("  {} {d}", spinner_char(5))
            } else {
                format!("  {d}")
            }
        })
        .unwrap_or_default();
    format!("  {spinner}{server_name}{detail_str}")
}

/// Rendered LSP status line for the status bar's left half.
/// Returns `None` only when no server is registered yet;
/// otherwise a server is picked and shown persistently — busy
/// with detail, busy alone, idle with detail, or just the
/// server name when idle with no detail. Legacy does the same:
/// once rust-analyzer is up, its name stays visible in the
/// status bar, spinner and detail come and go around it.
///
/// Selection: prefer a busy server; else a server that has a
/// non-empty detail; else just pick one (iteration order is
/// fine — typically there's only one).
pub(crate) fn lsp_progress_message(lsp: LspStatusesInput<'_>, render_tick: u64) -> Option<String> {
    let (server, status) = lsp
        .by_server
        .iter()
        .find(|(_, s)| s.busy)
        .or_else(|| {
            lsp.by_server
                .iter()
                .find(|(_, s)| s.detail.as_deref().is_some_and(|d| !d.is_empty()))
        })
        .or_else(|| lsp.by_server.iter().next())
        .map(|(name, s)| (name.clone(), s.clone()))?;
    let formatted =
        format_lsp_status(server.as_str(), status.busy, status.detail.as_deref(), render_tick);
    if formatted.is_empty() {
        None
    } else {
        Some(formatted)
    }
}

fn position_string(
    tabs: TabsActiveInput<'_>,
    _edits: EditedBuffersInput<'_>,
    _diagnostics: DiagnosticsStatesInput<'_>,
    session: SessionPrimaryInput<'_>,
) -> Arc<str> {
    // 1-indexed for human display — matches legacy goldens.
    // Falls back to `L1:C1` when no tab is active so post-kill /
    // empty-workspace status bars still anchor a position
    // string (legacy `display.rs` uses `s.cursor_row/col` which
    // default to zero in the same case).
    let (row, col) = active_tab(tabs)
        .map(|t| (t.cursor.line + 1, t.cursor.col + 1))
        .unwrap_or((1, 1));
    // `(secondary) ` prefix only after Init has resolved — the
    // default `primary == false` would otherwise flash for the
    // first few ticks before `Restored` lands and stamps the
    // real flock outcome.
    if *session.init_done && !*session.primary {
        Arc::from(format!("(secondary) L{row}:C{col} "))
    } else {
        Arc::from(format!("L{row}:C{col} "))
    }
}
