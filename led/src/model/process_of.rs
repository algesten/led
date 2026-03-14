use std::io;
use std::sync::Arc;

use led_core::rx::Stream;
use led_state::AppState;

use super::Mut;

/// Derive suspend/resume side effects from state.
pub fn process_of(state: &Stream<Arc<AppState>>) -> Stream<Mut> {
    // Suspend: perform terminal restore/re-init, then clear the flag
    let suspend_s = state
        .filter_map(|s| {
            if s.suspend {
                suspend();
                Some(Mut::Suspend(false))
            } else {
                None
            }
        })
        .stream();

    // Force redraw after resuming from suspend (true→false transition)
    let redraw_s = state
        .map(|s| (s.suspend, s.force_redraw))
        .fold((false, false, 0u64), |(_, prev_suspend, _), (suspend, redraw)| {
            (prev_suspend, suspend, redraw)
        })
        .filter_map(|(prev, curr, redraw)| {
            if prev && !curr {
                Some(Mut::ForceRedraw(redraw + 1))
            } else {
                None
            }
        })
        .stream();

    let merged = Stream::new();
    suspend_s.forward(&merged);
    redraw_s.forward(&merged);
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
