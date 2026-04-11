use std::collections::{HashMap, HashSet};

use led_core::git::LineStatus;
use led_core::rx::Stream;
use led_core::{CanonPath, IssueCategory, UserPath};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub enum GitOut {
    ScanFiles { root: CanonPath },
}

#[derive(Clone, Debug)]
pub enum GitIn {
    FileStatuses {
        statuses: HashMap<CanonPath, HashSet<IssueCategory>>,
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
        // Paths that had non-empty line statuses in the previous scan, so
        // the next scan can emit empty LineStatuses to clear gutter markers
        // for files that are no longer dirty.
        let mut tracked: HashSet<CanonPath> = HashSet::new();
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
                        let mut next_tracked: HashSet<CanonPath> = HashSet::new();
                        for (path, statuses) in line_statuses {
                            next_tracked.insert(path.clone());
                            result_tx
                                .send(GitIn::LineStatuses { path, statuses })
                                .await
                                .ok();
                        }
                        for path in tracked.difference(&next_tracked) {
                            result_tx
                                .send(GitIn::LineStatuses {
                                    path: path.clone(),
                                    statuses: Vec::new(),
                                })
                                .await
                                .ok();
                        }
                        tracked = next_tracked;
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
) -> Option<(HashMap<CanonPath, HashSet<IssueCategory>>, Option<String>)> {
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

    let mut result: HashMap<CanonPath, HashSet<IssueCategory>> = HashMap::new();

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

/// Compute line-level statuses for a file by computing two diffs:
/// - HEAD blob ↔ INDEX blob → `StagedModified` line ranges
/// - INDEX blob ↔ WORKTREE bytes → `Unstaged` line ranges
///
/// When both diffs cover the same line, the unstaged ranges win
/// (resolved at display time via `IssueCategory::precedence`).
fn scan_line_statuses(root: &CanonPath, file_path: &CanonPath) -> Option<Vec<LineStatus>> {
    let repo = git2::Repository::open(root.as_path()).ok()?;
    let workdir = UserPath::new(repo.workdir()?).canonicalize();
    let rel_path = file_path.strip_prefix(&workdir)?;
    let rel_str = rel_path.to_str()?;

    // HEAD blob (may be absent for new files).
    let head_content: Vec<u8> = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_tree().ok())
        .and_then(|tree| tree.get_path(std::path::Path::new(rel_str)).ok())
        .and_then(|entry| repo.find_blob(entry.id()).ok())
        .map(|blob| blob.content().to_vec())
        .unwrap_or_default();

    // INDEX blob (may be absent for unstaged files).
    let index_content: Vec<u8> = repo
        .index()
        .ok()
        .and_then(|idx| {
            idx.get_path(std::path::Path::new(rel_str), 0)
                .and_then(|entry| repo.find_blob(entry.id).ok().map(|b| b.content().to_vec()))
        })
        .unwrap_or_default();

    // WORKTREE bytes (current file content on disk).
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

    // Sort by start row so binary search in `line_category_at` works.
    // Unstaged ranges should win on overlap; sort with unstaged first within
    // a tie so the binary search hits unstaged.
    statuses.sort_by(|a, b| {
        a.rows
            .start
            .cmp(&b.rows.start)
            .then_with(|| a.category.precedence().cmp(&b.category.precedence()))
    });

    Some(statuses)
}

/// Diff two byte buffers and append every `+` line as a `LineStatus` of
/// the given category. Coalesces adjacent rows.
fn collect_added_lines(
    old: &[u8],
    new: &[u8],
    rel_str: &str,
    category: IssueCategory,
    out: &mut Vec<LineStatus>,
) {
    let Ok(patch) = git2::Patch::from_buffers(
        old,
        Some(std::path::Path::new(rel_str)),
        new,
        Some(std::path::Path::new(rel_str)),
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
            if let Some(last) = out.last_mut() {
                if last.category == category && last.rows.end == row {
                    last.rows.end = row + 1;
                    continue;
                }
            }
            out.push(LineStatus {
                category,
                rows: row..row + 1,
            });
        }
    }
}
