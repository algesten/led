//! Desktop-native worker for the git driver.
//!
//! One worker thread consumes `GitCmd::ScanFiles` off an mpsc
//! inbox. Each scan opens the repo via `git2::Repository::open`,
//! runs `repo.statuses(...)` for file-level categories, diffs
//! HEAD↔INDEX and INDEX↔WORKTREE for every dirty path, and emits:
//!
//! 1. One [`GitEvent::FileStatuses`] (always, even if empty), with
//!    the current branch shorthand (`None` for detached HEAD /
//!    missing HEAD).
//! 2. One [`GitEvent::LineStatuses`] per path that has at least
//!    one changed line.
//! 3. One empty `LineStatuses` per path that emitted non-empty
//!    ranges on the previous scan and is now clean — this is the
//!    gutter-clear signal.
//!
//! A failed `Repository::open` (not a repo, permissions, etc.)
//! returns *no* events — silent no-op per `docs/spec/git.md`
//! "Error paths". The runtime's atom keeps its previous values.
//!
//! The port of `scan_file_statuses` and `scan_line_statuses` is
//! verbatim from legacy `led/crates/git/src/lib.rs`, translated
//! off tokio onto std::thread.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use led_core::git::LineStatus;
use led_core::{CanonPath, IssueCategory, Notifier, UserPath};
use led_driver_git_core::{GitCmd, GitDriver, GitEvent, Trace};

/// Lifetime marker; the thread self-exits on `Sender` hangup.
pub struct GitNative {
    _marker: (),
}

pub fn spawn(trace: Arc<dyn Trace>, notify: Notifier) -> (GitDriver, GitNative) {
    let (tx_cmd, rx_cmd) = mpsc::channel::<GitCmd>();
    let (tx_ev, rx_ev) = mpsc::channel::<GitEvent>();
    let native = spawn_worker(rx_cmd, tx_ev, notify);
    let driver = GitDriver::new(tx_cmd, rx_ev, trace);
    (driver, native)
}

pub fn spawn_worker(
    rx_cmd: Receiver<GitCmd>,
    tx_ev: Sender<GitEvent>,
    notify: Notifier,
) -> GitNative {
    thread::Builder::new()
        .name("led-git".into())
        .spawn(move || worker_loop(rx_cmd, tx_ev, notify))
        .expect("spawning git worker should succeed");
    GitNative { _marker: () }
}

fn worker_loop(rx: Receiver<GitCmd>, tx: Sender<GitEvent>, notify: Notifier) {
    // Paths that produced non-empty LineStatuses on the previous
    // scan. Used to synthesise empty-list clear-events for paths
    // that transitioned from dirty to clean.
    let mut tracked: HashSet<CanonPath> = HashSet::new();
    while let Ok(cmd) = rx.recv() {
        match cmd {
            GitCmd::ScanFiles { root } => {
                // Silent on repo.open failure per spec.
                let Some((file_statuses, branch)) = scan_file_statuses(&root) else {
                    continue;
                };

                // Compute per-file line statuses for every dirty
                // path. Some may legitimately produce zero ranges
                // (e.g. rename-only entries with identical
                // contents); we still emit a non-entry in
                // `line_statuses` so the tracked-set dance below
                // treats them as clean.
                let mut line_statuses: Vec<(CanonPath, Vec<LineStatus>)> = Vec::new();
                for path in file_statuses.keys() {
                    if let Some(lines) = scan_line_statuses(&root, path)
                        && !lines.is_empty()
                    {
                        line_statuses.push((path.clone(), lines));
                    }
                }

                // Order matters: FileStatuses first so the ingest
                // reducer installs the map before per-path line
                // entries arrive.
                if tx
                    .send(GitEvent::FileStatuses {
                        statuses: file_statuses,
                        branch,
                    })
                    .is_err()
                {
                    return;
                }

                // Fresh tracked set for this scan. Any path in the
                // previous set but not here is a now-clean file
                // and gets an explicit clear-event.
                let mut next_tracked: HashSet<CanonPath> = HashSet::new();
                for (path, statuses) in line_statuses {
                    next_tracked.insert(path.clone());
                    if tx.send(GitEvent::LineStatuses { path, statuses }).is_err() {
                        return;
                    }
                }
                for stale in tracked.difference(&next_tracked) {
                    if tx
                        .send(GitEvent::LineStatuses {
                            path: stale.clone(),
                            statuses: Vec::new(),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                tracked = next_tracked;

                notify.notify();
            }
        }
    }
}

/// Per-path category set the scan produces, aliased so the
/// function signature doesn't trip `clippy::type_complexity`.
type FileStatusMap = HashMap<CanonPath, HashSet<IssueCategory>>;

/// Walk `repo.statuses()` and translate each entry onto zero or
/// more `IssueCategory` values. Also returns the branch shorthand
/// (or `None` for detached HEAD / missing HEAD).
fn scan_file_statuses(root: &CanonPath) -> Option<(FileStatusMap, Option<String>)> {
    let repo = git2::Repository::open(root.as_path()).ok()?;

    let branch = repo
        .head()
        .ok()
        .and_then(|h| h.shorthand().map(|s| s.to_string()));

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .exclude_submodules(true);

    let git_statuses = repo.statuses(Some(&mut opts)).ok()?;
    let workdir = UserPath::new(repo.workdir()?).canonicalize();

    let mut result: FileStatusMap = HashMap::new();

    for entry in git_statuses.iter() {
        let Some(rel_path) = entry.path() else {
            continue;
        };
        let abs_path = UserPath::new(workdir.as_path().join(rel_path)).canonicalize();
        let status = entry.status();

        let mut categories = HashSet::new();

        if status.intersects(git2::Status::WT_MODIFIED | git2::Status::WT_RENAMED) {
            categories.insert(IssueCategory::Unstaged);
        }
        if status.intersects(git2::Status::INDEX_MODIFIED | git2::Status::INDEX_RENAMED) {
            categories.insert(IssueCategory::StagedModified);
        }
        if status.intersects(git2::Status::INDEX_NEW) {
            categories.insert(IssueCategory::StagedNew);
        }
        if status.intersects(git2::Status::WT_NEW) {
            categories.insert(IssueCategory::Untracked);
        }

        if !categories.is_empty() {
            result.insert(abs_path, categories);
        }
    }

    Some((result, branch))
}

/// Compute line-level statuses for a file via two diffs:
///
/// - HEAD blob ↔ INDEX blob → [`IssueCategory::StagedModified`] ranges.
/// - INDEX blob ↔ WORKTREE bytes → [`IssueCategory::Unstaged`] ranges.
///
/// When both diffs cover the same row, the unstaged ranges win at
/// display time via [`IssueCategory::precedence`]; the sort order
/// below makes the binary search in `line_category_at` hit the
/// unstaged entry first.
fn scan_line_statuses(root: &CanonPath, file_path: &CanonPath) -> Option<Vec<LineStatus>> {
    let repo = git2::Repository::open(root.as_path()).ok()?;
    let workdir = UserPath::new(repo.workdir()?).canonicalize();
    let rel_path = file_path.as_path().strip_prefix(workdir.as_path()).ok()?;
    let rel_str = rel_path.to_str()?;

    // HEAD blob (absent for new files).
    let head_content: Vec<u8> = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_tree().ok())
        .and_then(|tree| tree.get_path(Path::new(rel_str)).ok())
        .and_then(|entry| repo.find_blob(entry.id()).ok())
        .map(|blob| blob.content().to_vec())
        .unwrap_or_default();

    // INDEX blob (absent for unstaged files).
    let index_content: Vec<u8> = repo
        .index()
        .ok()
        .and_then(|idx| {
            idx.get_path(Path::new(rel_str), 0)
                .and_then(|entry| repo.find_blob(entry.id).ok().map(|b| b.content().to_vec()))
        })
        .unwrap_or_default();

    // WORKTREE bytes (current content on disk).
    let worktree_content = std::fs::read(file_path.as_path()).ok()?;

    let mut statuses: Vec<LineStatus> = Vec::new();

    // Staged: HEAD ↔ INDEX
    collect_added_lines(
        &head_content,
        &index_content,
        rel_str,
        IssueCategory::StagedModified,
        &mut statuses,
    );

    // Unstaged: INDEX ↔ WORKTREE
    collect_added_lines(
        &index_content,
        &worktree_content,
        rel_str,
        IssueCategory::Unstaged,
        &mut statuses,
    );

    // Sort by start row so binary search works in
    // `line_category_at`; on a row tie, unstaged sorts first (it
    // has the lower precedence number) so unstaged wins.
    statuses.sort_by(|a, b| {
        a.rows
            .start
            .cmp(&b.rows.start)
            .then_with(|| a.category.precedence().cmp(&b.category.precedence()))
    });

    Some(statuses)
}

/// Diff two byte buffers and append every `+` line (new side) as
/// a `LineStatus` of the given category. Coalesces adjacent rows.
fn collect_added_lines(
    old: &[u8],
    new: &[u8],
    rel_str: &str,
    category: IssueCategory,
    out: &mut Vec<LineStatus>,
) {
    let Ok(patch) = git2::Patch::from_buffers(
        old,
        Some(Path::new(rel_str)),
        new,
        Some(Path::new(rel_str)),
        None,
    ) else {
        return;
    };

    let num_hunks = patch.num_hunks();
    for hunk_idx in 0..num_hunks {
        let Ok((_, num_lines)) = patch.hunk(hunk_idx) else {
            continue;
        };
        for line_idx in 0..num_lines {
            let Ok(line) = patch.line_in_hunk(hunk_idx, line_idx) else {
                continue;
            };
            if line.origin() != '+' {
                continue;
            }
            let Some(lineno) = line.new_lineno() else {
                continue;
            };
            let row = lineno as usize - 1;
            if let Some(last) = out.last_mut()
                && last.category == category
                && last.rows.end == row
            {
                last.rows.end = row + 1;
                continue;
            }
            out.push(LineStatus {
                category,
                rows: row..row + 1,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{Duration, Instant};

    struct NoopTrace;
    impl Trace for NoopTrace {
        fn git_scan_start(&self, _: &CanonPath) {}
        fn git_scan_done(&self, _: bool, _: usize) {}
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git invoke");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "t@t.com"]);
        git(dir, &["config", "user.name", "T"]);
    }

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    fn canon_of(p: &Path) -> CanonPath {
        UserPath::new(p).canonicalize()
    }

    fn drain_until<F: FnMut() -> bool>(mut f: F, deadline: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn non_repo_produces_no_events() {
        let tmp = tempfile::tempdir().unwrap();
        let (drv, _n) = spawn(Arc::new(NoopTrace), Notifier::noop());
        drv.execute([&GitCmd::ScanFiles {
            root: canon_of(tmp.path()),
        }]);
        // No events should arrive; wait a short window, expect none.
        std::thread::sleep(Duration::from_millis(200));
        assert!(drv.process().is_empty());
    }

    #[test]
    fn tracked_and_untracked_file_statuses() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        // Tracked + committed, then modify.
        write(dir, "tracked.txt", "original\n");
        git(dir, &["add", "tracked.txt"]);
        git(dir, &["commit", "-q", "-m", "init"]);
        write(dir, "tracked.txt", "original\nchanged\n");
        // Untracked file.
        write(dir, "untracked.txt", "new\n");

        let (drv, _n) = spawn(Arc::new(NoopTrace), Notifier::noop());
        drv.execute([&GitCmd::ScanFiles { root: canon_of(dir) }]);

        let mut events: Vec<GitEvent> = Vec::new();
        let ok = drain_until(
            || {
                let mut b = drv.process();
                if !b.is_empty() {
                    events.append(&mut b);
                    true
                } else {
                    false
                }
            },
            Duration::from_secs(5),
        );
        assert!(ok, "no events arrived");
        // Coalesce a second-round drain in case the LineStatuses
        // followed after the first drain exit.
        std::thread::sleep(Duration::from_millis(50));
        events.extend(drv.process());

        // Expect one FileStatuses with both paths, then at least
        // one LineStatuses for tracked.txt.
        let Some(GitEvent::FileStatuses { statuses, branch }) = events
            .iter()
            .find(|e| matches!(e, GitEvent::FileStatuses { .. }))
            .cloned()
        else {
            panic!("missing FileStatuses: {events:?}");
        };
        assert_eq!(branch.as_deref(), Some("main"));
        let tracked = canon_of(&dir.join("tracked.txt"));
        let untracked = canon_of(&dir.join("untracked.txt"));
        assert!(
            statuses
                .get(&tracked)
                .is_some_and(|c| c.contains(&IssueCategory::Unstaged)),
            "tracked.txt should carry Unstaged: {statuses:?}"
        );
        assert!(
            statuses
                .get(&untracked)
                .is_some_and(|c| c.contains(&IssueCategory::Untracked)),
            "untracked.txt should carry Untracked: {statuses:?}"
        );

        // Line statuses should include one Unstaged range for
        // tracked.txt (the added 'changed\n' line).
        let tracked_line = events.iter().find_map(|e| {
            if let GitEvent::LineStatuses { path, statuses } = e
                && path == &tracked
                && !statuses.is_empty()
            {
                return Some(statuses.clone());
            }
            None
        });
        let lines = tracked_line.expect("LineStatuses for tracked.txt");
        assert!(
            lines.iter().any(|l| l.category == IssueCategory::Unstaged),
            "expected Unstaged line: {lines:?}"
        );
    }

    #[test]
    fn clean_transition_emits_empty_line_statuses() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        write(dir, "f.txt", "a\n");
        git(dir, &["add", "f.txt"]);
        git(dir, &["commit", "-q", "-m", "init"]);
        // Dirty it.
        write(dir, "f.txt", "a\nb\n");

        let (drv, _n) = spawn(Arc::new(NoopTrace), Notifier::noop());
        drv.execute([&GitCmd::ScanFiles { root: canon_of(dir) }]);
        std::thread::sleep(Duration::from_millis(200));
        let _first = drv.process();

        // Revert to clean.
        write(dir, "f.txt", "a\n");
        drv.execute([&GitCmd::ScanFiles { root: canon_of(dir) }]);
        std::thread::sleep(Duration::from_millis(200));
        let second = drv.process();

        let target = canon_of(&dir.join("f.txt"));
        let has_clear = second.iter().any(|e| {
            matches!(
                e,
                GitEvent::LineStatuses { path, statuses }
                    if path == &target && statuses.is_empty()
            )
        });
        assert!(has_clear, "second scan should clear f.txt: {second:?}");
    }
}
