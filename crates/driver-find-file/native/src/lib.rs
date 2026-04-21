//! Desktop worker for the find-file completion driver.
//!
//! One worker thread blocks on `Receiver<FindFileCmd>`, runs
//! `fs::read_dir` + case-insensitive prefix filter + dir-first sort,
//! and posts a `FindFileListed` back. A failed read returns an empty
//! `entries` vec — the overlay doesn't distinguish "no completions"
//! from "directory missing".

use std::fs;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use led_core::{Notifier, UserPath};
use led_driver_find_file_core::{
    FindFileCmd, FindFileDriver, FindFileEntry, FindFileListed, Trace,
};

#[cfg(test)]
use led_core::CanonPath;

/// Lifecycle marker. Drops when the driver does; the worker self-exits
/// when its command `Sender` hangs up.
pub struct FindFileNative {
    _marker: (),
}

pub fn spawn(trace: Arc<dyn Trace>, notify: Notifier) -> (FindFileDriver, FindFileNative) {
    let (tx_cmd, rx_cmd) = mpsc::channel::<FindFileCmd>();
    let (tx_done, rx_done) = mpsc::channel::<FindFileListed>();
    let native = spawn_worker(rx_cmd, tx_done, notify);
    let driver = FindFileDriver::new(tx_cmd, rx_done, trace);
    (driver, native)
}

pub fn spawn_worker(
    rx_cmd: Receiver<FindFileCmd>,
    tx_done: Sender<FindFileListed>,
    notify: Notifier,
) -> FindFileNative {
    thread::Builder::new()
        .name("led-find-file".into())
        .spawn(move || worker_loop(rx_cmd, tx_done, notify))
        .expect("spawning find-file worker should succeed");
    FindFileNative { _marker: () }
}

fn worker_loop(
    rx: Receiver<FindFileCmd>,
    tx: Sender<FindFileListed>,
    notify: Notifier,
) {
    while let Ok(cmd) = rx.recv() {
        let entries = read_and_filter(&cmd);
        if tx
            .send(FindFileListed {
                dir: cmd.dir,
                prefix: cmd.prefix,
                entries,
            })
            .is_err()
        {
            return;
        }
        notify.notify();
    }
}

fn read_and_filter(cmd: &FindFileCmd) -> Vec<FindFileEntry> {
    let iter = match fs::read_dir(cmd.dir.as_path()) {
        Ok(it) => it,
        Err(_) => return Vec::new(),
    };
    let prefix_lower = cmd.prefix.to_lowercase();
    let mut out: Vec<FindFileEntry> = Vec::new();
    for entry in iter.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !cmd.show_hidden && name.starts_with('.') {
            continue;
        }
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let is_dir = if ft.is_dir() {
            true
        } else if ft.is_file() {
            false
        } else {
            // Symlinks / other: follow to classify.
            match fs::metadata(entry.path()) {
                Ok(m) => m.is_dir(),
                Err(_) => continue,
            }
        };
        let display = if is_dir { format!("{name}/") } else { name };
        let full = UserPath::new(entry.path()).canonicalize();
        out.push(FindFileEntry {
            name: display,
            full,
            is_dir,
        });
    }
    // Dirs-first, then case-insensitive alphabetical.
    out.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self as stdfs};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    static TMP_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let n = TMP_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = base.join(format!("led-find-file-test.{pid}.{n}"));
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

    struct NoopTraceImpl;
    impl Trace for NoopTraceImpl {
        fn find_file_start(&self, _: &FindFileCmd) {}
        fn find_file_done(&self, _: &CanonPath, _: &str, _: bool) {}
    }

    #[test]
    fn lists_with_prefix_filter_and_dir_first_sort() {
        let dir = tempdir();
        stdfs::write(dir.join("apple.rs"), b"").unwrap();
        stdfs::write(dir.join("banana.rs"), b"").unwrap();
        stdfs::create_dir(dir.join("apricot")).unwrap();
        stdfs::write(dir.join(".hidden"), b"").unwrap();

        let (drv, _native) = spawn(Arc::new(NoopTraceImpl), Notifier::noop());
        drv.execute(std::iter::once(&FindFileCmd {
            dir: canon(&dir),
            prefix: "a".into(),
            show_hidden: false,
        }));

        let deadline = Duration::from_secs(2);
        let mut collected: Vec<FindFileListed> = Vec::new();
        assert!(
            wait_for(
                || {
                    collected.extend(drv.process());
                    !collected.is_empty()
                },
                deadline
            ),
            "expected a completion within {deadline:?}"
        );
        let listed = &collected[0];
        let names: Vec<&str> = listed.entries.iter().map(|e| e.name.as_str()).collect();
        // "apricot/" (dir) before "apple.rs" (file), and "banana.rs"
        // filtered out. ".hidden" excluded because show_hidden=false.
        assert_eq!(names, vec!["apricot/", "apple.rs"]);
    }

    #[test]
    fn show_hidden_includes_dotfiles_matching_prefix() {
        let dir = tempdir();
        stdfs::write(dir.join(".config"), b"").unwrap();
        stdfs::write(dir.join(".cache"), b"").unwrap();

        let (drv, _native) = spawn(Arc::new(NoopTraceImpl), Notifier::noop());
        drv.execute(std::iter::once(&FindFileCmd {
            dir: canon(&dir),
            prefix: ".c".into(),
            show_hidden: true,
        }));

        let mut collected: Vec<FindFileListed> = Vec::new();
        wait_for(
            || {
                collected.extend(drv.process());
                !collected.is_empty()
            },
            Duration::from_secs(2),
        );
        let names: Vec<&str> = collected[0].entries.iter().map(|e| e.name.as_str()).collect();
        // Both dotfiles included, alphabetically.
        assert_eq!(names, vec![".cache", ".config"]);
    }

    #[test]
    fn unreadable_dir_returns_empty_entries() {
        let missing = PathBuf::from("/nonexistent-led-find-file-dir");
        let (drv, _native) = spawn(Arc::new(NoopTraceImpl), Notifier::noop());
        drv.execute(std::iter::once(&FindFileCmd {
            dir: canon(&missing),
            prefix: "".into(),
            show_hidden: false,
        }));

        let mut collected: Vec<FindFileListed> = Vec::new();
        wait_for(
            || {
                collected.extend(drv.process());
                !collected.is_empty()
            },
            Duration::from_secs(2),
        );
        assert!(collected[0].entries.is_empty());
    }
}
