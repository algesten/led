//! Wait phase: block on the wake channel until either a driver
//! signals progress or the nearest static deadline elapses.
//!
//! Returns `Some(())` to continue the loop; `None` when the wake
//! channel is disconnected (treated as a clean shutdown).

use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use crate::phases::TickEnv;
use crate::query::{
    self, AlertExpiryInput, FindFileInput, UndoFlushDebounceInput,
};
use crate::Sources;

pub(crate) fn run(sources: &Sources, env: &TickEnv<'_>) -> Option<()> {
    let Sources {
        alerts,
        find_file,
        undo_flush_debounce,
        lsp_status,
        clock,
        ..
    } = sources;

    let static_dl = query::static_deadline(
        AlertExpiryInput::new(alerts),
        FindFileInput::new(find_file),
        UndoFlushDebounceInput::new(undo_flush_debounce),
    );
    let deadline = if lsp_status.any_busy() {
        let lsp_dl = clock.now + Duration::from_millis(80);
        Some(static_dl.map(|d| d.min(lsp_dl)).unwrap_or(lsp_dl))
    } else {
        static_dl
    };
    let timeout = deadline
        .and_then(|d| d.checked_duration_since(clock.now))
        .unwrap_or(Duration::from_secs(60));
    match env.wake.rx.recv_timeout(timeout) {
        Ok(()) | Err(RecvTimeoutError::Timeout) => {}
        Err(RecvTimeoutError::Disconnected) => return None,
    }
    while env.wake.rx.try_recv().is_ok() {}
    Some(())
}
