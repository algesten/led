//! Dispatch sub-phase of Ingest: drain terminal events, run them
//! through the dispatcher, then sweep orphaned per-buffer state for
//! tabs that have closed.
//!
//! Returns a `DispatchOut` so the orchestrator can react to Quit.
//! Suspend is handled inline because it needs `stdout` for the
//! alt-screen round-trip.

use std::io::Write;

use led_driver_terminal_core::TermEvent;
use led_state_lifecycle::Phase;

use crate::dispatch::{Dispatcher, DispatchOutcome};
use crate::keymap::ChordState;
use crate::phases::TickEnv;
use crate::{Sources, Event};

/// What the dispatch loop wants the orchestrator to do next.
pub(crate) enum DispatchOut {
    /// Continue with subsequent phases this tick.
    Continue,
    /// `DispatchOutcome::Quit` was observed; the dispatch loop has
    /// already set `lifecycle.phase = Phase::Exiting`. The
    /// orchestrator should fall through the rest of this tick (so
    /// the execute pass dispatches `SessionCmd::Save`); the next
    /// tick's gate breaks the outer loop once `session.saved` flips.
    Quit,
}

/// Drain terminal events, dispatch each, sweep orphaned per-tab
/// state, and apply the M21 quit gate (caller breaks the outer
/// loop once it returns true).
pub(crate) fn dispatch_input<W: Write>(
    sources: &mut Sources,
    env: &TickEnv<'_>,
    stdout: &mut W,
    chord: &mut ChordState,
    last_frame: &mut Option<led_driver_terminal_core::Frame>,
) -> DispatchOut {
    let Sources {
        tabs,
        edits,
        kill_ring,
        clip,
        clipboard_driver,
        alerts,
        jumps,
        kbd_macro,
        browser,
        fs,
        store,
        terminal,
        find_file,
        isearch,
        file_search,
        syntax,
        diagnostics,
        path_chains,
        lsp_status,
        completions,
        completions_pending,
        lsp_extras,
        lsp_pending,
        git,
        chat_sessions,
        chat_prefs,
        lifecycle,
        clock,
        ..
    } = sources;

    env.drivers.input.process(terminal);

    while let Some(term_ev) = terminal.pending.pop_front() {
        let ev = match term_ev {
            TermEvent::Key(k) => Event::Key(k),
            TermEvent::Resize(d) => Event::Resize(d),
        };
        let mut dispatcher = Dispatcher {
            tabs,
            edits,
            kill_ring,
            clip,
            clipboard_driver,
            alerts,
            jumps,
            browser,
            fs,
            store,
            terminal,
            find_file,
            isearch,
            file_search,
            completions,
            completions_pending,
            lsp_extras,
            lsp_pending,
            diagnostics,
            lsp_status,
            git,
            path_chains,
            keymap: env.keymap,
            chord,
            kbd_macro,
            chat_sessions,
            chat_prefs,
            syntax,
            clock,
        };
        match dispatcher.dispatch(ev) {
            DispatchOutcome::Continue => {}
            DispatchOutcome::Quit => {
                lifecycle.phase = Phase::Exiting;
                return DispatchOut::Quit;
            }
            DispatchOutcome::Suspend => {
                lifecycle.phase = Phase::Suspended;
                if let Err(e) =
                    led_driver_terminal_native::suspend_and_resume(stdout)
                {
                    alerts.set_warn(
                        "suspend".to_string(),
                        format!("suspend: {e}"),
                    );
                }
                lifecycle.phase = Phase::Running;
                lifecycle.bump_redraw();
                env.drivers.output.invalidate();
                *last_frame = None;
            }
        }
    }

    DispatchOut::Continue
}

/// Sweep driver-owned per-buffer state for paths that are no
/// longer in `tabs.open`. Cheap on idle (the four `retain` walks
/// only do work when a tab actually closed).
pub(crate) fn cleanup_orphans(sources: &mut Sources) {
    let Sources {
        tabs,
        store,
        syntax,
        diagnostics,
        git,
        lsp_notified,
        path_chains,
        lsp_pending,
        chat_sessions,
        ..
    } = sources;

    let open_paths: std::collections::HashSet<&led_core::CanonPath> =
        tabs.open.iter().map(|t| &t.path).collect();
    store.loaded.retain(|p, _| open_paths.contains(p));
    syntax.by_path.retain(|p, _| open_paths.contains(p));
    diagnostics.by_path.retain(|p, _| open_paths.contains(p));
    git.line_statuses.retain(|p, _| open_paths.contains(p));
    lsp_notified.retain(|p, _| open_paths.contains(p));
    path_chains.retain(|p, _| open_paths.contains(p));
    lsp_pending
        .inlay_hints_requested
        .retain(|p, _| open_paths.contains(p));
    lsp_pending
        .inlay_hints_by_path
        .retain(|p, _| open_paths.contains(p));
    chat_sessions.by_path.retain(|p, _| open_paths.contains(p));
}

/// M21 quit gate. Returns `true` when the outer loop should break.
///
/// Per Theme E, this is a pure predicate — the `SessionCmd::
/// Shutdown` it used to emit alongside the break decision now
/// rides through `QueryOut.shutdown_cmd` and ships from
/// `execute_phase` earlier in the same tick (see
/// `query_phase::desired_shutdown` for the matching predicate).
/// The orchestrator calls this gate after every dispatch phase
/// has run, so the shutdown cmd has already been sent by the
/// time we return `true`.
pub(crate) fn check_quit_gate(sources: &Sources) -> bool {
    matches!(sources.lifecycle.phase, Phase::Exiting)
        && (sources.session.saved || !sources.session.primary)
}
