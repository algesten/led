use crossterm::event::{Event, EventStream, MouseEvent};
use led_core::AStream;
use led_core::FanoutStreamExt;
use led_core::keys::KeyCombo;
use std::io;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

/// Terminal input events
#[derive(Debug, Clone)]
pub enum TerminalInput {
    Key(KeyCombo),
    Resize(u16, u16),
    FocusGained,
    FocusLost,
    Mouse(MouseEvent),
}

/// Creates a driver that produces a stream of terminal input events.
///
/// This driver:
/// - Spawns a background task that reads crossterm EventStream
/// - Sets up raw mode, alternate screen, mouse capture, and bracketed paste
/// - Converts crossterm events to TerminalInput types
/// - Restores terminal state on drop
/// - Returns a broadcast stream that multiple subscribers can listen to
pub fn driver() -> impl AStream<TerminalInput> {
    let (tx, _rx) = broadcast::channel(256);

    let tx_clone = tx.clone();
    tokio::spawn(async move {
        if let Err(e) = run_input_loop(tx_clone).await {
            log::error!("input driver error: {}", e);
        }
    });

    setup_terminal().expect("set up terminal");
    setup_panic_hook();

    tx.one_by_one()
}

async fn run_input_loop(tx: broadcast::Sender<TerminalInput>) -> io::Result<()> {
    let mut event_stream = EventStream::new();

    loop {
        match event_stream.next().await {
            Some(Ok(event)) => {
                let input = match event {
                    Event::Key(key) => TerminalInput::Key(KeyCombo::from_key_event(key)),
                    Event::Resize(width, height) => TerminalInput::Resize(width, height),
                    Event::FocusGained => TerminalInput::FocusGained,
                    Event::FocusLost => TerminalInput::FocusLost,
                    Event::Mouse(mouse) => TerminalInput::Mouse(mouse),
                    _ => continue,
                };

                if tx.send(input).is_err() {
                    // No receivers, continue anyway
                    continue;
                }
            }
            Some(Err(e)) => return Err(e),
            None => break,
        }
    }

    Ok(())
}

fn setup_terminal() -> io::Result<()> {
    use crossterm::event::{EnableBracketedPaste, EnableMouseCapture};
    use crossterm::execute;
    use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};

    enable_raw_mode()?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
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
    use crossterm::event::{DisableBracketedPaste, DisableMouseCapture};
    use crossterm::execute;
    use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};

    disable_raw_mode()?;
    execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    Ok(())
}

impl Drop for InputDriver {
    fn drop(&mut self) {
        let _ = restore_terminal();
    }
}

/// Placeholder for the driver guard. In actual use, this would be held
/// by main to ensure cleanup happens at process exit.
pub struct InputDriver;
