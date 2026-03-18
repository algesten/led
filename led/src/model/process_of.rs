use std::io;
use std::sync::Arc;

use led_core::rx::Stream;
use led_state::{AppState, SessionRestorePhase};

use super::Mut;

/// Derive suspend/resume side effects from state.
pub fn process_of(state: &Stream<Arc<AppState>>) -> Stream<Mut> {
    // Suspend: perform terminal restore/re-init, then clear the flag
    let suspend_s = state
        .filter(|s| s.suspend)
        .inspect(|_| suspend())
        .map(|_| Mut::Suspend(false))
        .stream();

    // Force redraw after resuming from suspend (true→false transition)
    let redraw_s = state
        .map(|s| (s.suspend, s.force_redraw))
        .fold(
            (false, false, 0u64),
            |(_, prev_suspend, _), (suspend, redraw)| (prev_suspend, suspend, redraw),
        )
        .filter(|(prev, curr, _)| *prev && !*curr)
        .map(|(_, _, redraw)| Mut::ForceRedraw(redraw + 1))
        .stream();

    // Activate arg-file tab after session restore completes.
    // If the arg file was already opened by session restore, activate it
    // without re-opening through the docstore.
    let activate_arg_s = state
        .dedupe_by(|s| s.session_restore_phase == SessionRestorePhase::Done)
        .filter(|s| s.session_restore_phase == SessionRestorePhase::Done)
        .filter_map(|s| {
            let first_arg = s.startup.arg_paths.first()?;
            let buf = s
                .buffers
                .values()
                .find(|b| b.path.as_ref() == Some(first_arg))?;
            Some(Mut::ActivateBuffer(buf.id))
        })
        .stream();

    let merged = Stream::new();
    suspend_s.forward(&merged);
    redraw_s.forward(&merged);
    activate_arg_s.forward(&merged);
    merged
}

fn suspend() {
    use crossterm::event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    };
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };

    disable_raw_mode().ok();
    crossterm::execute!(
        io::stdout(),
        crossterm::cursor::Show,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )
    .ok();

    unsafe { libc::raise(libc::SIGTSTP) };

    enable_raw_mode().ok();
    crossterm::execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
    .ok();
}
