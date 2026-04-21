//! Desktop-native worker for the fs-list driver.
//!
//! A single worker thread drains `ListCmd` off an mpsc and posts
//! `ListDone` back. Reads use `std::fs::read_dir`. The worker
//! canonicalizes each child path so later equality checks against
//! `CanonPath` keys work uniformly, filters out hidden entries
//! (leading `.`), and returns unsorted — the state layer owns the
//! sort contract.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use led_core::{CanonPath, Notifier, UserPath};
use led_driver_fs_list_core::{DirEntry, DirEntryKind, FsListDriver, ListCmd, ListDone, Trace};

/// Lifecycle marker. See `FileReadNative` in `driver-buffers/native`
/// for the drop-order rationale — same idea.
pub struct FsListNative {
    _marker: (),
}

pub fn spawn(trace: Arc<dyn Trace>, notify: Notifier) -> (FsListDriver, FsListNative) {
    let (tx_cmd, rx_cmd) = mpsc::channel::<ListCmd>();
    let (tx_done, rx_done) = mpsc::channel::<ListDone>();
    let native = spawn_worker(rx_cmd, tx_done, notify);
    let driver = FsListDriver::new(tx_cmd, rx_done, trace);
    (driver, native)
}

pub fn spawn_worker(
    rx_cmd: Receiver<ListCmd>,
    tx_done: Sender<ListDone>,
    notify: Notifier,
) -> FsListNative {
    thread::Builder::new()
        .name("led-fs-list".into())
        .spawn(move || worker_loop(rx_cmd, tx_done, notify))
        .expect("spawning fs-list worker should succeed");
    FsListNative { _marker: () }
}

fn worker_loop(rx: Receiver<ListCmd>, tx: Sender<ListDone>, notify: Notifier) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            ListCmd::List(path) => {
                let result = read_one(&path);
                if tx.send(ListDone { path, result }).is_err() {
                    return;
                }
                notify.notify();
            }
        }
    }
}

fn read_one(path: &CanonPath) -> Result<Vec<DirEntry>, String> {
    let p: PathBuf = path.as_path().to_path_buf();
    let iter = fs::read_dir(&p).map_err(|e| e.to_string())?;
    let mut out: Vec<DirEntry> = Vec::new();
    for entry in iter.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let kind = if ft.is_dir() {
            DirEntryKind::Directory
        } else if ft.is_file() {
            DirEntryKind::File
        } else {
            // Symlinks / other: follow symlink metadata to classify.
            match fs::metadata(entry.path()) {
                Ok(m) if m.is_dir() => DirEntryKind::Directory,
                Ok(m) if m.is_file() => DirEntryKind::File,
                _ => continue,
            }
        };
        let child_path = UserPath::new(entry.path()).canonicalize();
        out.push(DirEntry {
            name,
            path: child_path,
            kind,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self as stdfs};
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    static TMP_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let n = TMP_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = base.join(format!("led-fs-list-test.{pid}.{n}"));
        stdfs::create_dir_all(&dir).unwrap();
        dir
    }

    fn canon(p: &std::path::Path) -> CanonPath {
        UserPath::new(p).canonicalize()
    }

    fn wait_for<F: FnMut() -> bool>(mut f: F, deadline: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    #[test]
    fn spawn_and_list_a_real_directory() {
        let dir = tempdir();
        // Create a file, a subdirectory, and a hidden file.
        {
            let mut f = stdfs::File::create(dir.join("a.txt")).unwrap();
            writeln!(f, "x").unwrap();
        }
        stdfs::create_dir_all(dir.join("sub")).unwrap();
        {
            let mut f = stdfs::File::create(dir.join(".hidden")).unwrap();
            writeln!(f, "x").unwrap();
        }

        let (drv, _native) = spawn(Arc::new(NoopTraceImpl), Notifier::noop());
        drv.execute([&ListCmd::List(canon(&dir))]);

        let mut got: Vec<ListDone> = Vec::new();
        let ok = wait_for(
            || {
                let mut batch = drv.process();
                if !batch.is_empty() {
                    got.append(&mut batch);
                    return true;
                }
                false
            },
            Duration::from_secs(5),
        );
        assert!(ok, "listing didn't complete");
        let done = got.pop().unwrap();
        let entries = done.result.expect("list failed");
        // Hidden filtered; order not guaranteed.
        let names: std::collections::HashSet<String> =
            entries.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains("a.txt"));
        assert!(names.contains("sub"));
        assert!(!names.contains(".hidden"));
    }

    struct NoopTraceImpl;
    impl Trace for NoopTraceImpl {
        fn list_start(&self, _: &CanonPath) {}
        fn list_done(&self, _: &CanonPath, _: &Result<Vec<DirEntry>, String>) {}
    }

    #[test]
    fn listing_a_nonexistent_path_errors() {
        let (drv, _native) = spawn(Arc::new(NoopTraceImpl), Notifier::noop());
        drv.execute([&ListCmd::List(canon(std::path::Path::new(
            "/no/such/directory/definitely",
        )))]);

        let mut got: Vec<ListDone> = Vec::new();
        let ok = wait_for(
            || {
                let mut batch = drv.process();
                if !batch.is_empty() {
                    got.append(&mut batch);
                    return true;
                }
                false
            },
            Duration::from_secs(5),
        );
        assert!(ok);
        assert!(got.pop().unwrap().result.is_err());
    }
}
