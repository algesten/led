//! Desktop-native worker for the file-watch driver (M26).
//!
//! One `std::thread` named `led-file-watch` owns a single
//! `notify::RecommendedWatcher` and consumes [`FileWatchCmd`]s off
//! an mpsc inbox. Watch intents (workspace root recursive, the
//! `<config>/notify/` non-recursive watch, per-buffer parent dirs)
//! all share the same watcher; fan-out to the originating
//! [`WatcherId`] is done by walking the runtime-supplied
//! `registrations` map on each notify event.
//!
//! # Debounce
//!
//! Per the legacy spec the `<config>/notify/` watch coalesces
//! 100 ms (`docs/spec/persistence.md` § "Cross-instance sync"
//! and `docs/drivers/workspace.md` § "Notify debouncing is
//! polling-based"). The worker honours the per-registration
//! `debounce_ms`: 0 emits immediately; `> 0` collects events into
//! a `pending` map and drains them on each `recv_timeout` tick.
//!
//! # Inert fallback
//!
//! Containerised CI environments without inotify/FSEvents return
//! an error from `notify::recommended_watcher`. In that case the
//! worker still spins, but every `Watch` is a silent no-op and no
//! events ever fire — code paths gated on watcher events are
//! tolerant of "no events ever" by construction (see
//! `docs/drivers/fs.md` § "Watcher inert on unsupported
//! platforms").

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use led_core::{CanonPath, Notifier};
use led_driver_file_watch_core::{
    ChangeKinds, FileWatchCmd, FileWatchDriver, FileWatchEvent, Trace, WatcherId,
};
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Lifetime marker; the worker self-exits on the inbound
/// `Sender<FileWatchCmd>` hangup. Per `EXAMPLE-ARCH.md` G12 we
/// do not `join()` in `Drop` — that would deadlock against
/// reverse-order field drops in `Drivers`.
pub struct FileWatchNative {
    _marker: (),
}

pub fn spawn(trace: Arc<dyn Trace>, notify: Notifier) -> (FileWatchDriver, FileWatchNative) {
    let (tx_cmd, rx_cmd) = mpsc::channel::<FileWatchCmd>();
    let (tx_ev, rx_ev) = mpsc::channel::<FileWatchEvent>();
    let native = spawn_worker(rx_cmd, tx_ev, notify);
    let driver = FileWatchDriver::new(tx_cmd, rx_ev, trace);
    (driver, native)
}

pub fn spawn_worker(
    rx_cmd: Receiver<FileWatchCmd>,
    tx_ev: Sender<FileWatchEvent>,
    notify: Notifier,
) -> FileWatchNative {
    thread::Builder::new()
        .name("led-file-watch".into())
        .spawn(move || worker_loop(rx_cmd, tx_ev, notify))
        .expect("spawning file-watch worker should succeed");
    FileWatchNative { _marker: () }
}

/// Shape of the merged channel the worker recvs on. The notify
/// callback runs on its own thread (notify spawns one
/// internally); we route its events through the same channel as
/// runtime-issued commands so the worker has a single recv
/// point.
enum Internal {
    Cmd(FileWatchCmd),
    Raw(notify::Event),
    BackendError(String),
}

/// Per-registration record. Mirrors
/// `led_driver_file_watch_core::Registration` but adds whether
/// we've actually told `notify::Watcher` about this path
/// (`watching == true` after a successful native `watch()`).
struct RegInfo {
    path: CanonPath,
    recursive: bool,
    debounce_ms: u32,
    watching: bool,
}

fn worker_loop(rx_cmd: Receiver<FileWatchCmd>, tx_ev: Sender<FileWatchEvent>, notify: Notifier) {
    // Merged-channel pattern: forward both runtime commands and
    // notify-callback events into one queue so the worker has a
    // single blocking recv point.
    let (tx_in, rx_in) = mpsc::channel::<Internal>();

    // Cmd-forwarder thread.
    let tx_cmd_fwd = tx_in.clone();
    thread::Builder::new()
        .name("led-file-watch-cmd".into())
        .spawn(move || {
            while let Ok(cmd) = rx_cmd.recv() {
                if tx_cmd_fwd.send(Internal::Cmd(cmd)).is_err() {
                    return;
                }
            }
        })
        .expect("spawning file-watch cmd forwarder should succeed");

    // notify::Watcher constructed lazily on first Watch cmd
    // arrival. Idle led processes (e.g. `--no-workspace` runs
    // in the goldens harness) never receive a Watch and skip
    // the FSEvents/inotify init entirely.
    let tx_raw = tx_in.clone();
    let mut watcher: Option<RecommendedWatcher> = None;

    let mut regs: HashMap<WatcherId, RegInfo> = HashMap::new();
    // Pending coalescer: keyed by (id, canonical-path-of-event).
    // Holds the running union of ChangeKinds bits and the timestamp
    // of the FIRST observation in the current quiet window.
    let mut pending: HashMap<(WatcherId, CanonPath), (ChangeKinds, Instant)> = HashMap::new();

    loop {
        // Adaptive timeout: when something is pending, wake at the
        // earliest deadline so we can drain. Otherwise block
        // indefinitely.
        let timeout = next_drain_deadline(&pending, &regs)
            .map(|d| d.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(60 * 60));

        match rx_in.recv_timeout(timeout) {
            Ok(Internal::Cmd(cmd)) => {
                handle_cmd(cmd, &mut regs, &mut watcher, &tx_raw, &tx_ev, &notify);
            }
            Ok(Internal::Raw(ev)) => {
                handle_raw_event(ev, &regs, &mut pending, &tx_ev, &notify);
            }
            Ok(Internal::BackendError(message)) => {
                // Apply to every registration so the runtime can
                // see one Failed per id rather than guessing.
                for id in regs.keys() {
                    let _ = tx_ev.send(FileWatchEvent::Failed {
                        id: *id,
                        message: message.clone(),
                    });
                }
                notify.notify();
            }
            Err(RecvTimeoutError::Timeout) => {
                drain_pending(&mut pending, &regs, &tx_ev, &notify);
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn handle_cmd(
    cmd: FileWatchCmd,
    regs: &mut HashMap<WatcherId, RegInfo>,
    watcher: &mut Option<RecommendedWatcher>,
    tx_raw: &Sender<Internal>,
    tx_ev: &Sender<FileWatchEvent>,
    notify: &Notifier,
) {
    match cmd {
        FileWatchCmd::Watch {
            id,
            path,
            recursive,
            debounce_ms,
        } => {
            // Idempotent: if already registered with the same
            // shape, no-op.
            if let Some(existing) = regs.get(&id)
                && existing.path == path
                && existing.recursive == recursive
                && existing.debounce_ms == debounce_ms
            {
                return;
            }
            // Lazy notify::Watcher construction: skip the
            // FSEvents / inotify init until the runtime actually
            // asks for a watch. `--no-workspace` runs never
            // dispatch a Watch and pay zero startup cost.
            if watcher.is_none() {
                let tx_for_callback = tx_raw.clone();
                match notify::recommended_watcher(
                    move |res: notify::Result<notify::Event>| match res {
                        Ok(ev) => {
                            let _ = tx_for_callback.send(Internal::Raw(ev));
                        }
                        Err(e) => {
                            let _ = tx_for_callback
                                .send(Internal::BackendError(e.to_string()));
                        }
                    },
                ) {
                    Ok(mut w) => {
                        let _ = Watcher::configure(&mut w, Config::default());
                        *watcher = Some(w);
                    }
                    Err(e) => {
                        log::warn!(
                            "file-watch backend init failed: {e}; running inert"
                        );
                    }
                }
            }
            let mut info = RegInfo {
                path: path.clone(),
                recursive,
                debounce_ms,
                watching: false,
            };
            if let Some(w) = watcher.as_mut() {
                let mode = if recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                };
                match w.watch(path.as_path(), mode) {
                    Ok(()) => info.watching = true,
                    Err(e) => {
                        log::warn!(
                            "file-watch install failed for {:?}: {}",
                            path.as_path(),
                            e
                        );
                        let _ = tx_ev.send(FileWatchEvent::Failed {
                            id,
                            message: e.to_string(),
                        });
                        notify.notify();
                    }
                }
            }
            regs.insert(id, info);
        }
        FileWatchCmd::Unwatch { id } => {
            if let Some(info) = regs.remove(&id)
                && info.watching
                && let Some(w) = watcher.as_mut()
            {
                let _ = w.unwatch(info.path.as_path());
            }
        }
    }
}

fn handle_raw_event(
    ev: notify::Event,
    regs: &HashMap<WatcherId, RegInfo>,
    pending: &mut HashMap<(WatcherId, CanonPath), (ChangeKinds, Instant)>,
    tx_ev: &Sender<FileWatchEvent>,
    notify: &Notifier,
) {
    let kinds = classify_event_kind(ev.kind);
    if kinds.bits() == 0 {
        return; // Access events and friends — drop.
    }
    let now = Instant::now();
    for raw_path in &ev.paths {
        let canon = canon_for_event(raw_path);
        for (id, info) in regs.iter() {
            if !path_matches_registration(&canon, info) {
                continue;
            }
            if info.debounce_ms == 0 {
                // Emit immediately. FSEvents/inotify already
                // coalesce at their native cadence.
                let _ = tx_ev.send(FileWatchEvent::Changed {
                    id: *id,
                    path: canon.clone(),
                    kinds,
                });
            } else {
                let entry = pending.entry((*id, canon.clone())).or_insert((kinds, now));
                entry.0.insert(kinds.bits());
            }
        }
    }
    notify.notify();
}

fn drain_pending(
    pending: &mut HashMap<(WatcherId, CanonPath), (ChangeKinds, Instant)>,
    regs: &HashMap<WatcherId, RegInfo>,
    tx_ev: &Sender<FileWatchEvent>,
    notify: &Notifier,
) {
    if pending.is_empty() {
        return;
    }
    let now = Instant::now();
    pending.retain(|(id, path), (kinds, started)| {
        let Some(reg) = regs.get(id) else {
            return false; // registration dropped; discard.
        };
        let window = Duration::from_millis(reg.debounce_ms as u64);
        if now.duration_since(*started) >= window {
            let _ = tx_ev.send(FileWatchEvent::Changed {
                id: *id,
                path: path.clone(),
                kinds: *kinds,
            });
            false
        } else {
            true
        }
    });
    notify.notify();
}

/// Compute the earliest wakeup we need to drain a pending entry.
/// `None` means there's nothing waiting and the worker can block
/// indefinitely.
fn next_drain_deadline(
    pending: &HashMap<(WatcherId, CanonPath), (ChangeKinds, Instant)>,
    regs: &HashMap<WatcherId, RegInfo>,
) -> Option<Instant> {
    pending
        .iter()
        .filter_map(|((id, _), (_, started))| {
            let reg = regs.get(id)?;
            Some(*started + Duration::from_millis(reg.debounce_ms as u64))
        })
        .min()
}

fn classify_event_kind(kind: EventKind) -> ChangeKinds {
    match kind {
        EventKind::Create(_) => ChangeKinds::from_bits(ChangeKinds::CREATED),
        EventKind::Modify(_) => ChangeKinds::from_bits(ChangeKinds::MODIFIED),
        EventKind::Remove(_) => ChangeKinds::from_bits(ChangeKinds::REMOVED),
        // `Access`, `Other`, and `Any` aren't user-meaningful; drop.
        _ => ChangeKinds::empty(),
    }
}

fn canon_for_event(path: &Path) -> CanonPath {
    use led_core::UserPath;
    UserPath::new(path.to_path_buf()).canonicalize()
}

fn path_matches_registration(event_path: &CanonPath, reg: &RegInfo) -> bool {
    if event_path == &reg.path {
        return true;
    }
    // Non-recursive watch on a directory still receives events
    // for its *immediate* children. Recursive accepts every
    // descendant.
    if reg.recursive {
        is_descendant(event_path.as_path(), reg.path.as_path())
    } else {
        event_path.as_path().parent() == Some(reg.path.as_path())
    }
}

fn is_descendant(child: &Path, parent: &Path) -> bool {
    let mut c = child;
    while let Some(p) = c.parent() {
        if p == parent {
            return true;
        }
        c = p;
    }
    false
}

// Suppress the unused-PathBuf import warning when no test path is
// constructed at compile-time.
#[allow(dead_code)]
fn _unused_pathbuf_witness(_p: PathBuf) {}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;
    use led_driver_file_watch_core::ChangeKinds;
    use std::sync::mpsc;
    use std::time::Duration;
    use tempfile::tempdir;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    #[test]
    fn classify_create_modify_remove() {
        use notify::event::{CreateKind, ModifyKind, RemoveKind};
        assert_eq!(
            classify_event_kind(EventKind::Create(CreateKind::File)).bits(),
            ChangeKinds::CREATED
        );
        assert_eq!(
            classify_event_kind(EventKind::Modify(ModifyKind::Any)).bits(),
            ChangeKinds::MODIFIED
        );
        assert_eq!(
            classify_event_kind(EventKind::Remove(RemoveKind::File)).bits(),
            ChangeKinds::REMOVED
        );
    }

    #[test]
    fn path_match_exact_recursive_and_immediate_children() {
        let parent = canon("/x");
        let recursive = RegInfo {
            path: parent.clone(),
            recursive: true,
            debounce_ms: 0,
            watching: true,
        };
        let non_recursive = RegInfo {
            path: parent.clone(),
            recursive: false,
            debounce_ms: 0,
            watching: true,
        };
        assert!(path_matches_registration(&parent, &recursive));
        assert!(path_matches_registration(&parent, &non_recursive));
        // Recursive: immediate AND nested descendants both match.
        assert!(path_matches_registration(&canon("/x/child"), &recursive));
        assert!(path_matches_registration(&canon("/x/a/b"), &recursive));
        // Non-recursive: only immediate children match (a sibling
        // dir watch fires for files that land there directly).
        assert!(path_matches_registration(&canon("/x/child"), &non_recursive));
        assert!(!path_matches_registration(&canon("/x/a/b"), &non_recursive));
        assert!(!path_matches_registration(&canon("/y"), &recursive));
    }

    #[test]
    fn descendant_check() {
        assert!(is_descendant(Path::new("/x/y"), Path::new("/x")));
        assert!(is_descendant(Path::new("/x/y/z"), Path::new("/x")));
        assert!(!is_descendant(Path::new("/x"), Path::new("/x")));
        assert!(!is_descendant(Path::new("/y"), Path::new("/x")));
    }

    #[test]
    fn end_to_end_modify_fires_changed_event() {
        let dir = tempdir().expect("tempdir");
        let file_path = dir.path().join("notes.txt");
        std::fs::write(&file_path, "v1\n").unwrap();

        let (tx_cmd, rx_cmd) = mpsc::channel::<FileWatchCmd>();
        let (tx_ev, rx_ev) = mpsc::channel::<FileWatchEvent>();
        let _native = spawn_worker(rx_cmd, tx_ev, Notifier::noop());

        let id = WatcherId(1);
        let parent = canon(dir.path().to_str().unwrap());
        tx_cmd
            .send(FileWatchCmd::Watch {
                id,
                path: parent.clone(),
                recursive: true,
                debounce_ms: 0,
            })
            .unwrap();
        // Give FSEvents a moment to install the watch. macOS
        // FSEvents has a several-hundred-ms startup latency
        // before the first event is delivered.
        std::thread::sleep(Duration::from_millis(600));

        // Repeat-write across the whole wait window so we don't
        // race FSEvents' coalescing (the *first* write inside a
        // freshly-installed FSEvents stream is sometimes
        // dropped on macOS).
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut got_changed = false;
        let mut wrote_n = 0u32;
        while Instant::now() < deadline {
            std::fs::write(&file_path, format!("v{wrote_n}\n")).unwrap();
            wrote_n += 1;
            match rx_ev.recv_timeout(Duration::from_millis(300)) {
                Ok(FileWatchEvent::Changed { id: ev_id, .. }) if ev_id == id => {
                    got_changed = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
        assert!(
            got_changed,
            "expected Changed event from notify::Watcher (parent={parent:?}, file={file_path:?})"
        );
    }
}
