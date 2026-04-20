//! Desktop-native async half of the clipboard driver.
//!
//! Single worker thread owns an `arboard::Clipboard` handle and
//! drains [`ClipboardCmd`]s from an mpsc. Each command is a blocking
//! system call; that's why it runs off the main loop.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use led_driver_clipboard_core::{
    ClipboardCmd, ClipboardDone, ClipboardDriver, ClipboardResult, Trace,
};

/// Lifecycle marker for the worker thread. See `FileReadNative` for
/// the drop-order rationale — same idea applies here.
pub struct ClipboardNative {
    _marker: (),
}

pub fn spawn(trace: Arc<dyn Trace>) -> (ClipboardDriver, ClipboardNative) {
    let (tx_cmd, rx_cmd) = mpsc::channel::<ClipboardCmd>();
    let (tx_done, rx_done) = mpsc::channel::<ClipboardDone>();
    let native = spawn_worker(rx_cmd, tx_done);
    let driver = ClipboardDriver::new(tx_cmd, rx_done, trace);
    (driver, native)
}

pub fn spawn_worker(
    rx_cmd: Receiver<ClipboardCmd>,
    tx_done: Sender<ClipboardDone>,
) -> ClipboardNative {
    thread::Builder::new()
        .name("led-clipboard".into())
        .spawn(move || worker_loop(rx_cmd, tx_done))
        .expect("spawning clipboard worker should succeed");
    ClipboardNative { _marker: () }
}

fn worker_loop(rx_cmd: Receiver<ClipboardCmd>, tx_done: Sender<ClipboardDone>) {
    // Construct lazily so a clipboard-less environment (headless CI)
    // doesn't crash — `arboard::Clipboard::new()` fails there.
    let mut clip: Option<arboard::Clipboard> = None;

    while let Ok(cmd) = rx_cmd.recv() {
        let result = match cmd {
            ClipboardCmd::Read => read(&mut clip),
            ClipboardCmd::Write(text) => write(&mut clip, &text),
        };
        if tx_done.send(ClipboardDone { result }).is_err() {
            return;
        }
    }
}

fn ensure_clip(clip: &mut Option<arboard::Clipboard>) -> Result<&mut arboard::Clipboard, String> {
    if clip.is_none() {
        match arboard::Clipboard::new() {
            Ok(c) => *clip = Some(c),
            Err(e) => return Err(format!("open clipboard: {e}")),
        }
    }
    Ok(clip.as_mut().expect("set above"))
}

fn read(clip: &mut Option<arboard::Clipboard>) -> Result<ClipboardResult, String> {
    let clip = ensure_clip(clip)?;
    match clip.get_text() {
        Ok(s) if s.is_empty() => Ok(ClipboardResult::Text(None)),
        Ok(s) => Ok(ClipboardResult::Text(Some(Arc::from(s)))),
        // `arboard` returns an Err for "no text content" — fold into
        // `Text(None)` rather than surfacing as a hard failure, so the
        // runtime falls back to the kill ring naturally.
        Err(arboard::Error::ContentNotAvailable) => Ok(ClipboardResult::Text(None)),
        Err(e) => Err(format!("read clipboard: {e}")),
    }
}

fn write(clip: &mut Option<arboard::Clipboard>, text: &str) -> Result<ClipboardResult, String> {
    let clip = ensure_clip(clip)?;
    match clip.set_text(text.to_string()) {
        Ok(()) => Ok(ClipboardResult::Written),
        Err(e) => Err(format!("write clipboard: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_driver_clipboard_core::NoopTrace;

    #[test]
    fn spawn_and_drop_is_clean() {
        // Smoke: spawn the worker, immediately drop. Worker should
        // exit when the Sender drops. Doesn't actually touch the
        // clipboard.
        let (_driver, _native) = spawn(Arc::new(NoopTrace));
    }
}
