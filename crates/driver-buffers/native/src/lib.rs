//! Desktop-native async side of the buffers driver.
//!
//! Spawns a worker thread that drains [`ReadCmd`] off an mpsc and posts
//! [`ReadDone`] back, using `std::fs` for the actual reads. On iOS /
//! Android the equivalent runs on GCD / coroutines respectively, but
//! both speak the same command/event types from `*-core`.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use led_core::{CanonPath, Notifier};
use led_driver_buffers_core::{
    FileReadDriver, FileWriteDriver, ReadCmd, ReadDone, Trace, WriteCmd, WriteDone,
};
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
pub fn spawn(trace: Arc<dyn Trace>, notify: Notifier) -> (FileReadDriver, FileReadNative) {
    let (tx_cmd, rx_cmd) = mpsc::channel::<ReadCmd>();
    let (tx_done, rx_done) = mpsc::channel::<ReadDone>();

    let native = spawn_worker(rx_cmd, tx_done, notify);
    let driver = FileReadDriver::new(tx_cmd, rx_done, trace);
    (driver, native)
}

/// Lower-level: spawn the worker against pre-existing channels. Useful
/// when the binary wants to own the channels (e.g. for telemetry or
/// complex shutdown).
pub fn spawn_worker(
    rx_cmd: Receiver<ReadCmd>,
    tx_done: Sender<ReadDone>,
    notify: Notifier,
) -> FileReadNative {
    thread::Builder::new()
        .name("led-file-read".into())
        .spawn(move || worker_loop(rx_cmd, tx_done, notify))
        .expect("spawning file-read worker should succeed");
    FileReadNative { _marker: () }
}

fn worker_loop(rx: Receiver<ReadCmd>, tx: Sender<ReadDone>, notify: Notifier) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            ReadCmd::Read(path) => {
                let result = read_one(&path);
                if tx.send(ReadDone { path, result }).is_err() {
                    return;
                }
                notify.notify();
            }
        }
    }
}

fn read_one(path: &CanonPath) -> Result<Arc<Rope>, String> {
    let p: PathBuf = path.as_path().to_path_buf();
    match fs::read_to_string(&p) {
        Ok(s) => Ok(Arc::new(Rope::from_str(&s))),
        // Missing file: present as an empty buffer so the user can
        // edit and save to create it. Matches legacy's docstore
        // `create_if_missing=true` semantics — the dispatched.snap
        // trace already always writes `create_if_missing=true`, so
        // this aligns the driver behaviour with the stated
        // contract. A later milestone can thread an explicit
        // `create_if_missing=false` mode through the ABI if we
        // ever need the strict-open flavour (SaveAs re-read path).
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Ok(Arc::new(Rope::new()))
        }
        Err(e) => Err(e.to_string()),
    }
}

// ── FileWriteDriver native worker ──────────────────────────────────────

/// Lifecycle marker for the write-worker thread. See [`FileReadNative`]
/// for the drop-order rationale; same argument applies.
pub struct FileWriteNative {
    _marker: (),
}

/// Convenience: spawn both halves of the write driver, connected to
/// fresh channels.
pub fn spawn_write(
    trace: Arc<dyn Trace>,
    notify: Notifier,
) -> (FileWriteDriver, FileWriteNative) {
    let (tx_cmd, rx_cmd) = mpsc::channel::<WriteCmd>();
    let (tx_done, rx_done) = mpsc::channel::<WriteDone>();
    let native = spawn_write_worker(rx_cmd, tx_done, notify);
    let driver = FileWriteDriver::new(tx_cmd, rx_done, trace);
    (driver, native)
}

pub fn spawn_write_worker(
    rx_cmd: Receiver<WriteCmd>,
    tx_done: Sender<WriteDone>,
    notify: Notifier,
) -> FileWriteNative {
    thread::Builder::new()
        .name("led-file-write".into())
        .spawn(move || write_worker_loop(rx_cmd, tx_done, notify))
        .expect("spawning file-write worker should succeed");
    FileWriteNative { _marker: () }
}

fn write_worker_loop(rx: Receiver<WriteCmd>, tx: Sender<WriteDone>, notify: Notifier) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            WriteCmd::Write {
                path,
                rope,
                version,
            } => {
                let result = write_atomic(&path, &rope, version);
                let done = WriteDone {
                    path,
                    version,
                    // On success echo the same Arc back — the runtime
                    // installs it as the new disk baseline and the
                    // refcount bump is O(1).
                    result: result.map(|()| rope),
                    from: None,
                };
                if tx.send(done).is_err() {
                    return;
                }
                notify.notify();
            }
            WriteCmd::WriteAs {
                from,
                to,
                rope,
                version,
            } => {
                let result = write_atomic(&to, &rope, version);
                let done = WriteDone {
                    path: to,
                    version,
                    result: result.map(|()| rope),
                    from: Some(from),
                };
                if tx.send(done).is_err() {
                    return;
                }
                notify.notify();
            }
        }
    }
}

/// Atomic write: dump rope to `<dir>/.led.<basename>.v<version>.tmp`,
/// then rename onto the target. Power loss or crash mid-write leaves
/// either the old file or the new one intact, never a torn write.
fn write_atomic(path: &CanonPath, rope: &Rope, version: u64) -> Result<(), String> {
    let target: PathBuf = path.as_path().to_path_buf();
    let dir = target
        .parent()
        .ok_or_else(|| "save target has no parent directory".to_string())?;
    let base = target
        .file_name()
        .ok_or_else(|| "save target has no filename".to_string())?;
    let mut tmp_name = std::ffi::OsString::from(".led.");
    tmp_name.push(base);
    tmp_name.push(format!(".v{version}.tmp"));
    let tmp_path = dir.join(&tmp_name);

    // Scope the File so it's closed (and flushed) before the rename.
    let write_result = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp_path)?;
        // `Rope::write_to` iterates chunks — no full-content allocation.
        rope.write_to(&mut f)?;
        f.flush()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(e.to_string());
    }

    if let Err(e) = fs::rename(&tmp_path, &target) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e.to_string());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    //! Integration-ish tests that actually spawn the native worker.
    //! Anything more abstract belongs in `*-core/tests`.

    use super::*;
    use led_core::UserPath;
    use led_driver_buffers_core::{
        BufferStore, LoadAction, LoadState, NoopTrace, SaveAction, WriteDone,
    };
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

        let (driver, _native) = spawn(Arc::new(NoopTrace), Notifier::noop());
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

    fn drain_write_completions(
        driver: &FileWriteDriver,
        deadline: Duration,
    ) -> Vec<WriteDone> {
        let mut all: Vec<WriteDone> = Vec::new();
        let start = Instant::now();
        while start.elapsed() < deadline {
            let mut batch = driver.process();
            if !batch.is_empty() {
                all.append(&mut batch);
                return all;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        all
    }

    #[test]
    fn spawn_write_persists_rope_atomically() {
        let tmp = tempdir();
        let file_path = tmp.path().join("out.txt");
        let path = canon(&file_path);
        let rope = Arc::new(Rope::from_str("hello, save\n"));

        let (driver, _native) = spawn_write(Arc::new(NoopTrace), Notifier::noop());
        driver.execute([&SaveAction::Save {
            path: path.clone(),
            rope: rope.clone(),
            version: 1,
        }]);

        let completions = drain_write_completions(&driver, Duration::from_secs(5));
        assert_eq!(completions.len(), 1);
        let done = &completions[0];
        assert_eq!(done.path, path);
        assert_eq!(done.version, 1);
        assert!(done.result.is_ok());

        // File contains what we wrote, and no `.tmp` detritus remains.
        let persisted = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(persisted, "hello, save\n");
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".led."))
            .collect();
        assert!(leftovers.is_empty(), "expected no tmp files left over");
    }

    #[test]
    fn spawn_write_reports_error_on_unwritable_path() {
        let tmp = tempdir();
        // Use a path whose parent doesn't exist — rename will fail.
        let bogus = tmp.path().join("no-such-dir").join("out.txt");
        let path = canon(&bogus);

        let (driver, _native) = spawn_write(Arc::new(NoopTrace), Notifier::noop());
        driver.execute([&SaveAction::Save {
            path,
            rope: Arc::new(Rope::from_str("x")),
            version: 1,
        }]);

        let completions = drain_write_completions(&driver, Duration::from_secs(5));
        assert_eq!(completions.len(), 1);
        assert!(completions[0].result.is_err());
    }

    #[test]
    fn spawn_treats_missing_file_as_empty_buffer() {
        // `create_if_missing=true` semantics: opening a path that
        // doesn't exist yields an empty rope so the user can edit
        // and save to create it. Real error states (permission
        // denied etc.) stay as `LoadState::Error`; this test only
        // covers the common "new file" case.
        let tmp = tempdir();
        let path = canon(&tmp.path().join("does-not-exist.rs"));

        let (driver, _native) = spawn(Arc::new(NoopTrace), Notifier::noop());
        let mut store = BufferStore::default();

        let acts = [LoadAction::Load(path.clone())];
        driver.execute(acts.iter(), &mut store);

        let got_ready = wait_until(
            || {
                driver.process(&mut store);
                matches!(store.loaded.get(&path), Some(LoadState::Ready(_)))
            },
            Duration::from_secs(5),
        );
        assert!(got_ready, "expected Ready(empty) within 5s");
        let Some(LoadState::Ready(rope)) = store.loaded.get(&path) else {
            panic!("expected Ready");
        };
        assert_eq!(rope.len_chars(), 0, "missing file should load as empty");
    }

    // ── Minimal tempdir without a dev-dep ──────────────────────────────

    struct TempDir(PathBuf);

    fn tempdir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir();
        let unique = format!(
            "led-driver-buffers-native-{}-{}",
            std::process::id(),
            n,
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
