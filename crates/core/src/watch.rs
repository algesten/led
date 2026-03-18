use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use notify::Watcher;
use tokio::sync::mpsc;

static REG_SEQ: AtomicU64 = AtomicU64::new(0);

// ── Public types ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchMode {
    Recursive,
    NonRecursive,
}

impl From<WatchMode> for notify::RecursiveMode {
    fn from(m: WatchMode) -> Self {
        match m {
            WatchMode::Recursive => notify::RecursiveMode::Recursive,
            WatchMode::NonRecursive => notify::RecursiveMode::NonRecursive,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WatchEvent {
    pub paths: Vec<PathBuf>,
    pub kind: WatchEventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEventKind {
    Create,
    Modify,
    Remove,
}

// ── FileWatcher ──

struct RegEntry {
    id: u64,
    dir: PathBuf,
    tx: mpsc::Sender<WatchEvent>,
}

/// A shared file-watcher service backed by a single `notify::RecommendedWatcher`.
///
/// Multiple consumers register directories via [`FileWatcher::register`].
/// Each registration gets its own channel; events whose paths fall under a
/// registered directory are dispatched to that channel.  Dropping the
/// [`Registration`] guard removes the sender and, when no registrations
/// remain for a directory, unwatches it at the OS level.
pub struct FileWatcher {
    regs: Arc<Mutex<Vec<RegEntry>>>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// Tracks which canonical dirs have an active `watcher.watch()` call.
    watched: Mutex<HashSet<PathBuf>>,
}

impl FileWatcher {
    /// Create a new shared watcher.  Returns an `Arc` because `register`
    /// takes `&Arc<Self>` (the `Registration` drop guard needs a handle).
    pub fn new() -> Arc<Self> {
        let regs: Arc<Mutex<Vec<RegEntry>>> = Arc::new(Mutex::new(Vec::new()));
        let regs_cb = Arc::clone(&regs);

        let watcher =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                let Ok(ev) = res else { return };
                let kind = match ev.kind {
                    notify::EventKind::Create(_) => WatchEventKind::Create,
                    notify::EventKind::Modify(_) => WatchEventKind::Modify,
                    notify::EventKind::Remove(_) => WatchEventKind::Remove,
                    _ => return,
                };
                let watch_event = WatchEvent {
                    paths: ev.paths,
                    kind,
                };
                let regs = regs_cb.lock().unwrap();
                for reg in regs.iter() {
                    if watch_event.paths.iter().any(|p| p.starts_with(&reg.dir)) {
                        reg.tx.try_send(watch_event.clone()).ok();
                    }
                }
            })
            .expect("create notify watcher");

        Arc::new(FileWatcher {
            regs,
            watcher: Mutex::new(Some(watcher)),
            watched: Mutex::new(HashSet::new()),
        })
    }

    /// Create an inert watcher that accepts `register` calls but never
    /// delivers events.  Used in tests that don't need file-system watching.
    pub fn inert() -> Arc<Self> {
        Arc::new(FileWatcher {
            regs: Arc::new(Mutex::new(Vec::new())),
            watcher: Mutex::new(None),
            watched: Mutex::new(HashSet::new()),
        })
    }

    /// Register a directory for watching.
    ///
    /// `dir` is canonicalized internally so that lookups match what the OS
    /// reports (e.g. `/var` → `/private/var` on macOS).
    ///
    /// Returns a [`Registration`] guard — dropping it removes this
    /// registration and, if no other registrations remain for the same
    /// canonical directory, unwatches it.
    ///
    /// On an inert watcher the registration is recorded but no OS-level
    /// watch is created — the returned channel will never receive events.
    pub fn register(
        self: &Arc<Self>,
        dir: &Path,
        mode: WatchMode,
        tx: mpsc::Sender<WatchEvent>,
    ) -> Registration {
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let id = REG_SEQ.fetch_add(1, Ordering::Relaxed);
        log::trace!(
            "[FileWatcher] register id={} dir={} mode={:?}",
            id,
            dir.display(),
            mode,
        );

        self.regs.lock().unwrap().push(RegEntry {
            id,
            dir: dir.clone(),
            tx,
        });

        let mut watched = self.watched.lock().unwrap();
        if watched.insert(dir.clone()) {
            if let Some(ref mut w) = *self.watcher.lock().unwrap() {
                log::debug!(
                    "[FileWatcher] watching dir={} mode={:?}",
                    dir.display(),
                    mode,
                );
                if let Err(e) = w.watch(&dir, mode.into()) {
                    log::warn!("[FileWatcher] failed to watch {}: {e}", dir.display());
                }
            }
        }

        Registration {
            fw: Arc::clone(self),
            id,
            dir,
        }
    }
}

// ── Registration guard ──

/// Drop guard returned by [`FileWatcher::register`].  Dropping it removes
/// the registration; when no registrations remain for the directory the
/// OS-level watch is removed.
pub struct Registration {
    fw: Arc<FileWatcher>,
    id: u64,
    dir: PathBuf,
}

impl Drop for Registration {
    fn drop(&mut self) {
        log::trace!(
            "[FileWatcher] drop registration id={} dir={}",
            self.id,
            self.dir.display(),
        );
        let mut regs = self.fw.regs.lock().unwrap();
        regs.retain(|r| r.id != self.id);
        let still_watched = regs.iter().any(|r| r.dir == self.dir);
        drop(regs);

        if !still_watched {
            self.fw.watched.lock().unwrap().remove(&self.dir);
            if let Some(ref mut w) = *self.fw.watcher.lock().unwrap() {
                w.unwatch(&self.dir).ok();
            }
        }
    }
}
