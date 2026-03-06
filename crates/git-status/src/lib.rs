use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use led_core::file_status::{FileStatus, LineStatus, LineStatusKind};
use led_core::{
    Action, Component, Context, DrawContext, Effect, Event, PanelClaim, Waker,
};

use ratatui::Frame;
use ratatui::layout::Rect;

enum WorkerResult {
    FileStatuses {
        statuses: HashMap<PathBuf, HashSet<FileStatus>>,
        branch: Option<String>,
    },
    LineStatuses {
        path: PathBuf,
        statuses: Vec<LineStatus>,
    },
}

pub struct GitStatus {
    root: PathBuf,
    result_rx: tokio::sync::mpsc::UnboundedReceiver<WorkerResult>,
    result_tx: tokio::sync::mpsc::UnboundedSender<WorkerResult>,
    file_notify: Arc<tokio::sync::Notify>,
    line_notify: Arc<tokio::sync::Notify>,
    line_path: Arc<Mutex<Option<PathBuf>>>,
    waker: Option<Waker>,
    spawned: bool,
}

impl GitStatus {
    pub fn new(root: PathBuf, waker: Option<Waker>) -> Self {
        let (result_tx, result_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            root,
            result_rx,
            result_tx,
            file_notify: Arc::new(tokio::sync::Notify::new()),
            line_notify: Arc::new(tokio::sync::Notify::new()),
            line_path: Arc::new(Mutex::new(None)),
            waker,
            spawned: false,
        }
    }

    fn spawn_workers(&mut self) {
        if self.spawned {
            return;
        }
        self.spawned = true;

        // File status worker
        {
            let root = self.root.clone();
            let notify = self.file_notify.clone();
            let tx = self.result_tx.clone();
            let waker = self.waker.clone();

            tokio::spawn(async move {
                loop {
                    notify.notified().await;
                    // Small delay to coalesce rapid saves
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

                    let result =
                        tokio::task::spawn_blocking({
                            let root = root.clone();
                            move || scan_file_statuses(&root)
                        })
                        .await;

                    if let Ok(Some((statuses, branch))) = result {
                        let _ = tx.send(WorkerResult::FileStatuses { statuses, branch });
                        if let Some(ref w) = waker {
                            w();
                        }
                    }
                }
            });
        }

        // Line status worker
        {
            let root = self.root.clone();
            let notify = self.line_notify.clone();
            let line_path = self.line_path.clone();
            let tx = self.result_tx.clone();
            let waker = self.waker.clone();

            tokio::spawn(async move {
                loop {
                    notify.notified().await;

                    let path = line_path.lock().unwrap().take();
                    let Some(path) = path else { continue };

                    let result = tokio::task::spawn_blocking({
                        let root = root.clone();
                        let path = path.clone();
                        move || scan_line_statuses(&root, &path)
                    })
                    .await;

                    if let Ok(Some(statuses)) = result {
                        let _ = tx.send(WorkerResult::LineStatuses { path, statuses });
                        if let Some(ref w) = waker {
                            w();
                        }
                    }
                }
            });
        }

        // Trigger initial scan
        self.file_notify.notify_one();
    }

    fn trigger_file_scan(&self) {
        self.file_notify.notify_one();
    }

    fn trigger_line_scan(&self, path: PathBuf) {
        *self.line_path.lock().unwrap() = Some(path);
        self.line_notify.notify_one();
    }
}

fn scan_file_statuses(
    root: &Path,
) -> Option<(HashMap<PathBuf, HashSet<FileStatus>>, Option<String>)> {
    let repo = git2::Repository::open(root).ok()?;

    let branch = repo
        .head()
        .ok()
        .and_then(|h| h.shorthand().map(|s| s.to_string()));

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .exclude_submodules(true);

    let git_statuses = repo.statuses(Some(&mut opts)).ok()?;
    let workdir = repo.workdir()?.to_path_buf();

    let mut result: HashMap<PathBuf, HashSet<FileStatus>> = HashMap::new();

    for entry in git_statuses.iter() {
        let Some(rel_path) = entry.path() else {
            continue;
        };
        let abs_path = workdir.join(rel_path);
        let status = entry.status();

        let mut file_statuses = HashSet::new();

        if status.intersects(
            git2::Status::WT_MODIFIED
                | git2::Status::INDEX_MODIFIED
                | git2::Status::WT_RENAMED
                | git2::Status::INDEX_RENAMED,
        ) {
            file_statuses.insert(FileStatus::GitModified);
        }

        if status.intersects(git2::Status::INDEX_NEW) {
            file_statuses.insert(FileStatus::GitAdded);
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

fn scan_line_statuses(root: &Path, file_path: &Path) -> Option<Vec<LineStatus>> {
    let repo = git2::Repository::open(root).ok()?;
    let workdir = repo.workdir()?.to_path_buf();
    let rel_path = file_path.strip_prefix(&workdir).ok()?;
    let rel_str = rel_path.to_str()?;

    // Get HEAD blob
    let head = repo.head().ok()?;
    let tree = head.peel_to_tree().ok()?;
    let entry = tree.get_path(std::path::Path::new(rel_str)).ok()?;
    let old_blob = repo.find_blob(entry.id()).ok()?;
    let old_content = old_blob.content();

    // Read current file from disk
    let new_content = std::fs::read(file_path).ok()?;

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
        let (_hunk, num_lines) = patch.hunk(hunk_idx).ok()?;
        let hunk_has_deletes = _hunk.old_lines() > 0;

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

impl Component for GitStatus {
    fn panel_claims(&self) -> &[PanelClaim] {
        &[]
    }

    fn handle_action(&mut self, action: Action, _ctx: &mut Context) -> Vec<Effect> {
        match action {
            Action::Tick => {
                self.spawn_workers();
                let mut effects = Vec::new();
                while let Ok(result) = self.result_rx.try_recv() {
                    match result {
                        WorkerResult::FileStatuses { statuses, branch } => {
                            effects.push(Effect::SetFileStatuses { statuses, branch });
                        }
                        WorkerResult::LineStatuses { path, statuses } => {
                            effects.push(Effect::SetLineStatuses { path, statuses });
                        }
                    }
                }
                effects
            }
            _ => vec![],
        }
    }

    fn handle_event(&mut self, event: &Event, _ctx: &mut Context) -> Vec<Effect> {
        match event {
            Event::FileSaved(path) => {
                self.trigger_file_scan();
                self.trigger_line_scan(path.clone());
            }
            Event::TabActivated { path: Some(path) } => {
                self.trigger_line_scan(path.clone());
            }
            Event::Resume => {
                self.trigger_file_scan();
            }
            _ => {}
        }
        vec![]
    }

    fn draw(&mut self, _frame: &mut Frame, _area: Rect, _ctx: &mut DrawContext) {}
}
