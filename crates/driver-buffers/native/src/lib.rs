//! Desktop-native async side of the buffers driver.
//!
//! Spawns a worker thread that drains [`ReadCmd`] off an mpsc and posts
//! [`ReadDone`] back, using `std::fs` for the actual reads. On iOS /
//! Android the equivalent runs on GCD / coroutines respectively, but
//! both speak the same command/event types from `*-core`.

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use led_core::CanonPath;
use led_driver_buffers_core::{FileReadDriver, ReadCmd, ReadDone, Trace};
use ropey::Rope;

/// Lifecycle marker for the native worker thread.
///
/// Deliberately does not hold the `JoinHandle` — joining in `Drop` would
/// deadlock whenever `FileReadNative` drops before `FileReadDriver`
/// (e.g. the `let (driver, _native) = spawn(...)` tuple pattern drops
/// right-to-left). The worker self-exits when `FileReadDriver` drops
/// its `Sender<ReadCmd>` and the receiver returns `Err` on `recv`. Any
/// thread still alive at process exit is reaped by the OS.
pub struct FileReadNative {
    _marker: (),
}

/// Convenience: build both halves of the driver connected to each
/// other, with channels allocated internally. This is the one-call
/// wiring most binaries want.
pub fn spawn(trace: Arc<dyn Trace>) -> (FileReadDriver, FileReadNative) {
    let (tx_cmd, rx_cmd) = mpsc::channel::<ReadCmd>();
    let (tx_done, rx_done) = mpsc::channel::<ReadDone>();

    let native = spawn_worker(rx_cmd, tx_done);
    let driver = FileReadDriver::new(tx_cmd, rx_done, trace);
    (driver, native)
}

/// Lower-level: spawn the worker against pre-existing channels. Useful
/// when the binary wants to own the channels (e.g. for telemetry or
/// complex shutdown).
pub fn spawn_worker(rx_cmd: Receiver<ReadCmd>, tx_done: Sender<ReadDone>) -> FileReadNative {
    thread::Builder::new()
        .name("led-file-read".into())
        .spawn(move || worker_loop(rx_cmd, tx_done))
        .expect("spawning file-read worker should succeed");
    FileReadNative { _marker: () }
}

fn worker_loop(rx: Receiver<ReadCmd>, tx: Sender<ReadDone>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            ReadCmd::Read(path) => {
                let result = read_one(&path);
                if tx.send(ReadDone { path, result }).is_err() {
                    return;
                }
            }
        }
    }
}

fn read_one(path: &CanonPath) -> Result<Arc<Rope>, String> {
    let p: PathBuf = path.as_path().to_path_buf();
    match fs::read_to_string(&p) {
        Ok(s) => Ok(Arc::new(Rope::from_str(&s))),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    //! Integration-ish tests that actually spawn the native worker.
    //! Anything more abstract belongs in `*-core/tests`.

    use super::*;
    use led_core::UserPath;
    use led_driver_buffers_core::{BufferStore, LoadAction, LoadState, NoopTrace};
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn canon(p: &std::path::Path) -> CanonPath {
        UserPath::new(p).canonicalize()
    }

    fn wait_until(mut predicate: impl FnMut() -> bool, deadline: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    #[test]
    fn spawn_reads_an_existing_file() {
        let tmp = tempdir();
        let file_path = tmp.path().join("hello.txt");
        {
            let mut f = std::fs::File::create(&file_path).unwrap();
            writeln!(f, "hello world").unwrap();
        }
        let path = canon(&file_path);

        let (driver, _native) = spawn(Arc::new(NoopTrace));
        let mut store = BufferStore::default();

        let acts = [LoadAction::Load(path.clone())];
        driver.execute(acts.iter(), &mut store);
        assert!(matches!(store.loaded.get(&path), Some(LoadState::Pending)));

        let ready = wait_until(
            || {
                driver.process(&mut store);
                matches!(store.loaded.get(&path), Some(LoadState::Ready(_)))
            },
            Duration::from_secs(5),
        );
        assert!(ready, "expected Ready within 5s");
    }

    #[test]
    fn spawn_reports_error_on_missing_file() {
        let tmp = tempdir();
        let path = canon(&tmp.path().join("does-not-exist.rs"));

        let (driver, _native) = spawn(Arc::new(NoopTrace));
        let mut store = BufferStore::default();

        let acts = [LoadAction::Load(path.clone())];
        driver.execute(acts.iter(), &mut store);

        let got_error = wait_until(
            || {
                driver.process(&mut store);
                matches!(store.loaded.get(&path), Some(LoadState::Error(_)))
            },
            Duration::from_secs(5),
        );
        assert!(got_error, "expected Error within 5s");
    }

    // ── Minimal tempdir without a dev-dep ──────────────────────────────

    struct TempDir(PathBuf);

    fn tempdir() -> TempDir {
        let base = std::env::temp_dir();
        let unique = format!(
            "led-driver-buffers-native-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        );
        let p = base.join(unique);
        std::fs::create_dir_all(&p).expect("tempdir create");
        TempDir(p)
    }

    impl TempDir {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
