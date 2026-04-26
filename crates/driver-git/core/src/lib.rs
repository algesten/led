//! Sync core of the git driver.
//!
//! One command (`GitCmd::ScanFiles { root }`) fans out to a burst
//! of events: exactly one [`GitEvent::FileStatuses`] followed by
//! one [`GitEvent::LineStatuses`] per currently-dirty path, then
//! one empty `LineStatuses` per path that was dirty last scan and
//! is now clean (the "clear" signal that erases gutter bars).
//!
//! The actual libgit2 work lives in `driver-git-native`. This crate
//! only defines the ABI so the runtime compiles against a narrow
//! surface and a mock driver (for tests) can substitute a plain
//! channel pair.
//!
//! # Scan discipline
//!
//! Each `ScanFiles` is independent: the worker re-opens the repo
//! via `git2::Repository::open`, runs the status + diff pass, and
//! drops the handle. Cheap because libgit2 memory-maps `.git/`;
//! matches legacy's stateless-about-repo-identity design. The
//! only state the driver retains between scans is `tracked`, the
//! set of paths that produced a non-empty `LineStatuses` last
//! time — needed to synthesise clear-events.
//!
//! # Ordering
//!
//! `FileStatuses` is always emitted *first* so the runtime's
//! reducer can install the new file-level map before per-path line
//! statuses arrive. Reversing the order would briefly show gutter
//! bars for paths not yet catalogued in the sidebar map.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use led_core::git::LineStatus;
use led_core::{CanonPath, IssueCategory};

// ── ABI ─────────────────────────────────────────────────────────

/// Runtime → driver commands.
#[derive(Debug, Clone)]
pub enum GitCmd {
    /// Scan the workspace rooted at `root`. The worker reopens
    /// the repo, produces file + line statuses, and emits a
    /// burst of [`GitEvent`]s before settling.
    ScanFiles { root: CanonPath },
}

/// Driver → runtime events.
#[derive(Debug, Clone)]
pub enum GitEvent {
    /// Repo-wide file status + branch. First message of every
    /// scan.
    FileStatuses {
        statuses: HashMap<CanonPath, HashSet<IssueCategory>>,
        branch: Option<String>,
    },
    /// One path's per-line status list. Empty `statuses` is the
    /// "clear" signal emitted for paths that were dirty on the
    /// previous scan and are no longer dirty now. The runtime
    /// treats empty-list as remove, not no-op.
    LineStatuses {
        path: CanonPath,
        statuses: Vec<LineStatus>,
    },
}

// ── Trace ──────────────────────────────────────────────────────

/// `--golden-trace` hook. `git_scan_start` fires once per
/// dispatched command (becomes the `GitScan\troot=<p>` line in
/// `dispatched.snap`); `git_scan_done` is internal-only — the
/// FileStatuses delivery already carries the outcome, so the
/// dispatched-intent log doesn't need the done-line.
pub trait Trace: Send + Sync {
    fn git_scan_start(&self, root: &CanonPath);
    fn git_scan_done(&self, ok: bool, n_files: usize);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn git_scan_start(&self, _: &CanonPath) {}
    fn git_scan_done(&self, _: bool, _: usize) {}
}

// ── Driver handle ──────────────────────────────────────────────

/// Main-loop-facing half. Owns the `Sender` for commands and the
/// `Receiver` for events. Constructed by the native `spawn`
/// alongside the lifetime marker.
pub struct GitDriver {
    tx: Sender<GitCmd>,
    rx: Receiver<GitEvent>,
    trace: Arc<dyn Trace>,
}

impl GitDriver {
    pub fn new(tx: Sender<GitCmd>, rx: Receiver<GitEvent>, trace: Arc<dyn Trace>) -> Self {
        Self { tx, rx, trace }
    }

    /// Ship a batch of commands. The worker is strictly serial —
    /// scans queue up and process in order.
    pub fn execute<'a>(&self, cmds: impl IntoIterator<Item = &'a GitCmd>) {
        for cmd in cmds {
            match cmd {
                GitCmd::ScanFiles { root } => {
                    self.trace.git_scan_start(root);
                }
            }
            if self.tx.send(cmd.clone()).is_err() {
                return;
            }
        }
    }

    /// Drain completions. Caller folds `FileStatuses` then each
    /// `LineStatuses` into the atom in arrival order.
    pub fn process(&self) -> Vec<GitEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            if let GitEvent::FileStatuses { statuses, .. } = &ev {
                self.trace.git_scan_done(true, statuses.len());
            }
            out.push(ev);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;
    use std::sync::mpsc;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    #[test]
    fn process_returns_empty_when_channel_quiet() {
        let (_tx_cmd, _rx_cmd) = mpsc::channel::<GitCmd>();
        let (_tx_ev, rx_ev) = mpsc::channel::<GitEvent>();
        let drv = GitDriver::new(_tx_cmd, rx_ev, Arc::new(NoopTrace));
        assert!(drv.process().is_empty());
    }

    #[test]
    fn process_drains_a_file_statuses_event() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<GitCmd>();
        let (tx_ev, rx_ev) = mpsc::channel::<GitEvent>();
        let drv = GitDriver::new(tx_cmd, rx_ev, Arc::new(NoopTrace));
        let mut map = HashMap::new();
        let mut cats = HashSet::new();
        cats.insert(IssueCategory::Unstaged);
        map.insert(canon("/x/y.rs"), cats);
        tx_ev
            .send(GitEvent::FileStatuses {
                statuses: map,
                branch: Some("main".to_string()),
            })
            .unwrap();
        let batch = drv.process();
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn execute_forwards_a_scan_command() {
        let (tx_cmd, rx_cmd) = mpsc::channel::<GitCmd>();
        let (_tx_ev, rx_ev) = mpsc::channel::<GitEvent>();
        let drv = GitDriver::new(tx_cmd, rx_ev, Arc::new(NoopTrace));
        drv.execute([&GitCmd::ScanFiles {
            root: canon("/root"),
        }]);
        let cmd = rx_cmd.try_recv().expect("cmd sent");
        matches!(cmd, GitCmd::ScanFiles { .. });
    }
}
