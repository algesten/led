use std::collections::{HashMap, HashSet};

use led_core::git::{FileStatus, LineStatus, LineStatusKind};
use led_core::rx::Stream;
use led_core::{CanonPath, UserPath};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub enum GitOut {
    ScanFiles { root: CanonPath },
}

#[derive(Clone, Debug)]
pub enum GitIn {
    FileStatuses {
        statuses: HashMap<CanonPath, HashSet<FileStatus>>,
        branch: Option<String>,
    },
    LineStatuses {
        path: CanonPath,
        statuses: Vec<LineStatus>,
    },
}

pub fn driver(out: Stream<GitOut>) -> Stream<GitIn> {
    let stream: Stream<GitIn> = Stream::new();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<GitOut>(64);
    let (result_tx, mut result_rx) = mpsc::channel::<GitIn>(64);

    // Bridge: rx::Stream -> channel
    out.on(move |opt: Option<&GitOut>| {
        if let Some(cmd) = opt {
            cmd_tx.try_send(cmd.clone()).ok();
        }
    });

    // Async task: handle git operations via spawn_blocking
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                GitOut::ScanFiles { root } => {
                    let result = tokio::task::spawn_blocking(move || {
                        let (file_statuses, branch) = scan_file_statuses(&root)?;
                        let line_statuses: Vec<_> = file_statuses
                            .keys()
                            .filter_map(|path| {
                                let lines = scan_line_statuses(&root, path)?;
                                Some((path.clone(), lines))
                            })
                            .collect();
                        Some((file_statuses, branch, line_statuses))
                    })
                    .await;
                    if let Ok(Some((file_statuses, branch, line_statuses))) = result {
                        result_tx
                            .send(GitIn::FileStatuses {
                                statuses: file_statuses,
                                branch,
                            })
                            .await
                            .ok();
                        for (path, statuses) in line_statuses {
                            result_tx
                                .send(GitIn::LineStatuses { path, statuses })
                                .await
                                .ok();
                        }
                    }
                }
            }
        }
    });

    // Bridge: channel -> rx::Stream
    let s = stream.clone();
    tokio::task::spawn_local(async move {
        while let Some(v) = result_rx.recv().await {
            s.push(v);
        }
    });

    stream
}

fn scan_file_statuses(
    root: &CanonPath,
) -> Option<(HashMap<CanonPath, HashSet<FileStatus>>, Option<String>)> {
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

    let mut result: HashMap<CanonPath, HashSet<FileStatus>> = HashMap::new();

    for entry in git_statuses.iter() {
        let Some(rel_path) = entry.path() else {
            continue;
        };
        let abs_path = UserPath::new(workdir.as_path().join(rel_path)).canonicalize();
        let status = entry.status();

        let mut file_statuses = HashSet::new();

        if status.intersects(git2::Status::WT_MODIFIED | git2::Status::WT_RENAMED) {
            file_statuses.insert(FileStatus::GitWtModified);
        }

        if status.intersects(git2::Status::INDEX_MODIFIED | git2::Status::INDEX_RENAMED) {
            file_statuses.insert(FileStatus::GitIndexModified);
        }

        if status.intersects(git2::Status::INDEX_NEW) {
            file_statuses.insert(FileStatus::GitIndexNew);
        }

        if status.intersects(git2::Status::WT_NEW) {
            file_statuses.insert(FileStatus::GitUntracked);
        }

        if !file_statuses.is_empty() {
            result.insert(abs_path, file_statuses);
        }
    }

    Some((result, branch))
}

fn scan_line_statuses(root: &CanonPath, file_path: &CanonPath) -> Option<Vec<LineStatus>> {
    let repo = git2::Repository::open(root.as_path()).ok()?;
    let workdir = UserPath::new(repo.workdir()?).canonicalize();
    let rel_path = file_path.strip_prefix(&workdir)?;
    let rel_str = rel_path.to_str()?;

    // Get HEAD blob
    let head = repo.head().ok()?;
    let tree = head.peel_to_tree().ok()?;
    let entry = tree.get_path(std::path::Path::new(rel_str)).ok()?;
    let old_blob = repo.find_blob(entry.id()).ok()?;
    let old_content = old_blob.content();

    // Read current file from disk
    let new_content = std::fs::read(file_path.as_path()).ok()?;

    let patch = git2::Patch::from_buffers(
        old_content,
        Some(std::path::Path::new(rel_str)),
        &new_content,
        Some(std::path::Path::new(rel_str)),
        None,
    )
    .ok()?;

    let mut statuses: Vec<LineStatus> = Vec::new();
    let num_hunks = patch.num_hunks();

    for hunk_idx in 0..num_hunks {
        let (hunk, num_lines) = patch.hunk(hunk_idx).ok()?;
        let hunk_has_deletes = hunk.old_lines() > 0;

        for line_idx in 0..num_lines {
            let line = patch.line_in_hunk(hunk_idx, line_idx).ok()?;
            if line.origin() == '+' {
                let row = line.new_lineno()? as usize - 1;
                let kind = if hunk_has_deletes {
                    LineStatusKind::GitModified
                } else {
                    LineStatusKind::GitAdded
                };
                if let Some(last) = statuses.last_mut() {
                    if last.kind == kind && last.rows.end == row {
                        last.rows.end = row + 1;
                        continue;
                    }
                }
                statuses.push(LineStatus {
                    kind,
                    rows: row..row + 1,
                });
            }
        }
    }

    Some(statuses)
}
