use crossterm::event::Event;
use led_core::keys::KeyCombo;
use led_core::rx::Stream;
use std::io;

/// Terminal input events
#[derive(Debug, Clone)]
pub enum TerminalInput {
    Key(KeyCombo),
    Resize(u16, u16),
    FocusGained,
    FocusLost,
}

/// Start the input driver. Returns a stream of terminal input events.
/// Internally spawns a dedicated OS thread for blocking crossterm reads
/// and a local task to bridge results into the reactive tree.
pub fn driver() -> Stream<TerminalInput> {
    let stream: Stream<TerminalInput> = Stream::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);

    // OS thread: blocking crossterm reads → channel
    std::thread::spawn(move || {
        // Emit initial terminal size
        if let Ok((w, h)) = crossterm::terminal::size() {
            let _ = tx.blocking_send(TerminalInput::Resize(w, h));
        }

        loop {
            match crossterm::event::read() {
                Ok(event) => {
                    let input = match event {
                        Event::Key(key) => TerminalInput::Key(KeyCombo::from_key_event(key)),
                        Event::Resize(w, h) => TerminalInput::Resize(w, h),
                        Event::FocusGained => TerminalInput::FocusGained,
                        Event::FocusLost => TerminalInput::FocusLost,
                        _ => continue,
                    };
                    if tx.blocking_send(input).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    log::error!("input error: {}", e);
                    break;
                }
            }
        }
    });

    // Local task: channel → rx::Stream
    let s = stream.clone();
    tokio::task::spawn_local(async move {
        while let Some(v) = rx.recv().await {
            s.push(v);
        }
    });

    stream
}

pub struct InputGuard;

impl Drop for InputGuard {
    fn drop(&mut self) {
        let _ = restore_terminal();
    }
}

pub fn setup_terminal() -> InputGuard {
    do_setup_terminal().expect("set up terminal");
    setup_panic_hook();
    InputGuard
}

fn do_setup_terminal() -> io::Result<()> {
    use crossterm::event::EnableBracketedPaste;
    use crossterm::execute;
    use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    Ok(())
}

fn setup_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = restore_terminal();
        default_hook(panic_info);
    }));
}

fn restore_terminal() -> io::Result<()> {
    use crossterm::event::DisableBracketedPaste;
    use crossterm::execute;
    use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};

    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableBracketedPaste)?;
    Ok(())
}
