use std::io;
use std::sync::Arc;

use led_core::{AStream, FanoutStreamExt, StreamOpsExt};
use led_state::AppState;
use tokio_stream::StreamExt;

use super::Mut;

/// Watches AppState for one-shot flags, performs the side effect,
/// and emits Muts to clear the flag.
pub fn process_of(state: impl AStream<Arc<AppState>>) -> impl AStream<Mut> {
    let state_tx = state.broadcast();

    let redraw_on_resume_suspend = state_tx
        .one_by_one()
        .map(|a| a.suspend)
        .reduce((false, false), |p, c| (p.1, c))
        .dedupe()
        .filter(|(p, c)| *p && !c)
        .sample_combine(state_tx.latest())
        .map(|(_, s)| Mut::ForceRedraw(s.force_redraw + 1));

    let unsuspend = state_tx.one_by_one().filter_map(|s| {
        if s.suspend {
            suspend();
            Some(Mut::Suspend(false))
        } else {
            None
        }
    });

    redraw_on_resume_suspend.or(unsuspend)
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
