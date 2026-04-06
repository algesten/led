use std::io;
use std::rc::Rc;

use led_core::rx::Stream;
use led_state::{AppState, Phase};

use super::Mut;

/// Derive suspend/resume side effects from state.
pub fn process_of(state: &Stream<Rc<AppState>>) -> Stream<Mut> {
    // Suspend: perform terminal restore/re-init, then emit Resumed
    let suspend_s = state
        .filter(|s| s.phase == Phase::Suspended)
        .inspect(|_| suspend())
        .map(|_| Mut::Resumed)
        .stream();

    // Force redraw after resuming from suspend (Suspended→Running transition)
    let redraw_s = state
        .map(|s| (s.phase == Phase::Suspended, s.force_redraw))
        .fold(
            (false, false, 0u64),
            |(_, prev_suspend, _), (suspend, redraw)| (prev_suspend, suspend, redraw),
        )
        .filter(|(prev, curr, _)| *prev && !*curr)
        .map(|(_, _, redraw)| Mut::ForceRedraw(redraw + 1))
        .stream();

    // Activate last arg-file tab after session restore completes.
    // If the arg file was already opened by session restore, activate it
    // without re-opening through the docstore.
    let activate_arg_s = state
        .dedupe_by(|s| s.phase == Phase::Running)
        .filter(|s| s.phase == Phase::Running)
        .filter_map(|s| {
            let last_arg = s.startup.arg_paths.last()?;
            let buf = s
                .buffers
                .values()
                .find(|b| b.is_materialized() && b.path() == Some(last_arg))?;
            Some(Mut::ActivateBuffer(buf.path()?.clone()))
        })
        .stream();

    // Bump last_used for arg files already open from session restore,
    // making them resistant to auto-close.
    let touch_args_s = state
        .dedupe_by(|s| s.phase == Phase::Running)
        .filter(|s| s.phase == Phase::Running)
        .filter(|s| !s.startup.arg_paths.is_empty())
        .filter_map(|s| {
            let entries: Vec<_> = s
                .startup
                .arg_paths
                .iter()
                .filter_map(|p| {
                    let buf = s.buffers.values().find(|b| b.path() == Some(p))?;
                    Some(buf.path()?.clone())
                })
                .collect();
            if entries.is_empty() {
                None
            } else {
                Some(Mut::TouchArgFiles { entries })
            }
        })
        .stream();

    let merged = Stream::new();
    suspend_s.forward(&merged);
    redraw_s.forward(&merged);
    activate_arg_s.forward(&merged);
    touch_args_s.forward(&merged);
    merged
}

fn suspend() {
    use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };

    disable_raw_mode().ok();
    crossterm::execute!(
        io::stdout(),
        crossterm::cursor::Show,
        LeaveAlternateScreen,
        DisableBracketedPaste
    )
    .ok();

    // SAFETY: raise(SIGTSTP) is a well-defined POSIX signal operation.
    unsafe { libc::raise(libc::SIGTSTP) };

    enable_raw_mode().ok();
    crossterm::execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste).ok();
}
