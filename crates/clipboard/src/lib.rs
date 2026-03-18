use std::sync::Mutex;

use led_core::rx::Stream;

#[derive(Clone, Debug)]
pub enum ClipboardOut {
    Write(String),
    Read,
}

#[derive(Clone, Debug)]
pub enum ClipboardIn {
    Text(String),
}

/// System clipboard driver using arboard.
pub fn driver(out: Stream<ClipboardOut>) -> Stream<ClipboardIn> {
    let result: Stream<ClipboardIn> = Stream::new();
    let clipboard = Mutex::new(arboard::Clipboard::new().ok());
    let r = result.clone();

    out.on(move |opt: Option<&ClipboardOut>| {
        let Some(cmd) = opt else { return };
        if let Ok(mut guard) = clipboard.lock() {
            if let Some(cb) = guard.as_mut() {
                match cmd {
                    ClipboardOut::Write(text) => {
                        let _ = cb.set_text(text);
                    }
                    ClipboardOut::Read => {
                        let text = cb.get_text().unwrap_or_default();
                        r.push(ClipboardIn::Text(text));
                    }
                }
            }
        }
    });

    result
}

/// In-memory clipboard driver for headless/test mode.
pub fn driver_headless(out: Stream<ClipboardOut>) -> Stream<ClipboardIn> {
    let result: Stream<ClipboardIn> = Stream::new();
    let buf: Mutex<String> = Mutex::new(String::new());
    let r = result.clone();

    out.on(move |opt: Option<&ClipboardOut>| {
        let Some(cmd) = opt else { return };
        if let Ok(mut guard) = buf.lock() {
            match cmd {
                ClipboardOut::Write(text) => {
                    *guard = text.clone();
                }
                ClipboardOut::Read => {
                    r.push(ClipboardIn::Text(guard.clone()));
                }
            }
        }
    });

    result
}
