use std::io;
use std::rc::Rc;

use led_core::rx::Stream;
use led_state::{AppState, SessionRestorePhase};

use super::Mut;

/// Derive suspend/resume side effects from state.
pub fn process_of(state: &Stream<Rc<AppState>>) -> Stream<Mut> {
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

    // Activate last arg-file tab after session restore completes.
    // If the arg file was already opened by session restore, activate it
    // without re-opening through the docstore.
    let activate_arg_s = state
        .dedupe_by(|s| s.session.restore_phase == SessionRestorePhase::Done)
        .filter(|s| s.session.restore_phase == SessionRestorePhase::Done)
        .filter_map(|s| {
            let last_arg = s.startup.arg_paths.last()?;
            let buf = s
                .buffers
                .values()
                .find(|b| b.is_loaded() && b.path_buf() == Some(last_arg))?;
            Some(Mut::ActivateBuffer(buf.path_buf()?.clone()))
        })
        .stream();

    // Bump last_used for arg files already open from session restore,
    // making them resistant to auto-close. For multi-file args, also
    // reorder their tab_order to the end of the tab bar in arg order.
    let touch_args_s = state
        .dedupe_by(|s| s.session.restore_phase == SessionRestorePhase::Done)
        .filter(|s| s.session.restore_phase == SessionRestorePhase::Done)
        .filter(|s| !s.startup.arg_paths.is_empty())
        .filter_map(|s| {
            let reorder = s.startup.arg_paths.len() > 1;
            let base = if reorder {
                s.buffers
                    .values()
                    .filter(|b| {
                        !s.startup
                            .arg_paths
                            .iter()
                            .any(|ap| b.path_buf() == Some(ap))
                    })
                    .map(|b| b.tab_order().0)
                    .max()
                    .map_or(0, |m| m + 1)
            } else {
                0 // unused when not reordering
            };
            let entries: Vec<_> = s
                .startup
                .arg_paths
                .iter()
                .enumerate()
                .filter_map(|(i, p)| {
                    let buf = s.buffers.values().find(|b| b.path_buf() == Some(p))?;
                    let tab_order = if reorder { base + i } else { buf.tab_order().0 };
                    Some((buf.path_buf()?.clone(), tab_order))
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
