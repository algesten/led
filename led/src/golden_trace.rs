//! Black-box test trace. When `--golden-trace <FILE>` is set, led writes
//! one normalized line per externally-observable dispatch to FILE. The
//! goldens runner snapshots the file and compares against `dispatched.snap`.
//!
//! Format: `t=<ms>\t<category>\t<fields>\n`. Stable string representations
//! only; no typed protocol. Paths are emitted absolute and stripped to a
//! placeholder by the runner (which knows the tempdir).
//!
//! The actual emission is dispatched through `led_core::golden_trace`'s
//! global sink so that crates deep in the stack (e.g. the LSP transport
//! layer) can emit without depending on the `led` crate.

use std::cell::RefCell;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use led_core::golden_trace::TraceSink;

pub struct GoldenTraceSink {
    file: Mutex<RefCell<File>>,
    start: Instant,
}

impl GoldenTraceSink {
    pub fn install(path: &Path) -> std::io::Result<Arc<Self>> {
        let file = File::create(path)?;
        let sink = Arc::new(Self {
            file: Mutex::new(RefCell::new(file)),
            start: Instant::now(),
        });
        led_core::golden_trace::set_sink(sink.clone());
        Ok(sink)
    }
}

impl TraceSink for GoldenTraceSink {
    fn emit(&self, category: &str, fields: &str) {
        let elapsed_ms = self.start.elapsed().as_millis();
        let guard = match self.file.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let mut f = guard.borrow_mut();
        let _ = writeln!(f, "t={elapsed_ms}ms\t{category}\t{fields}");
        let _ = f.flush();
    }
}
