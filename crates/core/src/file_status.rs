use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileStatus {
    GitModified,
    GitAdded,
    GitUntracked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineStatusKind {
    GitAdded,
    GitModified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineStatus {
    pub kind: LineStatusKind,
    pub rows: Range<usize>,
}

#[derive(Debug, Default, Clone)]
pub struct FileStatusStore {
    files: HashMap<PathBuf, HashSet<FileStatus>>,
    lines: HashMap<PathBuf, Vec<LineStatus>>,
    pub branch: Option<String>,
}

impl FileStatusStore {
    pub fn set_file_statuses(&mut self, statuses: HashMap<PathBuf, HashSet<FileStatus>>) {
        self.files = statuses;
    }

    pub fn file_statuses(&self, path: &Path) -> Option<&HashSet<FileStatus>> {
        self.files.get(path)
    }

    pub fn directory_statuses(&self, dir: &Path) -> HashSet<FileStatus> {
        let mut result = HashSet::new();
        for (path, statuses) in &self.files {
            if path.starts_with(dir) && path != dir {
                result.extend(statuses);
            }
        }
        result
    }

    pub fn set_line_statuses(&mut self, path: PathBuf, statuses: Vec<LineStatus>) {
        self.lines.insert(path, statuses);
    }

    pub fn line_status_at(&self, path: &Path, row: usize) -> Option<LineStatusKind> {
        let statuses = self.lines.get(path)?;
        let idx = statuses
            .binary_search_by(|s| {
                if row < s.rows.start {
                    std::cmp::Ordering::Greater
                } else if row >= s.rows.end {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()?;
        Some(statuses[idx].kind)
    }

}

pub struct StatusDisplay {
    pub letter: char,
    pub theme_key: &'static str,
}

struct StatusInfo {
    letter: char,
    theme_key: &'static str,
    priority: u8,
}

fn status_info(s: FileStatus) -> StatusInfo {
    match s {
        FileStatus::GitModified => StatusInfo {
            letter: 'M',
            theme_key: "git.modified",
            priority: 1,
        },
        FileStatus::GitAdded => StatusInfo {
            letter: 'A',
            theme_key: "git.added",
            priority: 2,
        },
        FileStatus::GitUntracked => StatusInfo {
            letter: 'U',
            theme_key: "git.untracked",
            priority: 3,
        },
    }
}

/// Compose a set of file statuses into a display.
/// Letter from lowest priority, color from highest.
pub fn resolve_display(statuses: &HashSet<FileStatus>) -> Option<StatusDisplay> {
    if statuses.is_empty() {
        return None;
    }
    let mut lowest: Option<StatusInfo> = None;
    let mut highest_key: Option<(&'static str, u8)> = None;
    for &s in statuses {
        let info = status_info(s);
        if lowest.as_ref().map_or(true, |l| info.priority < l.priority) {
            lowest = Some(info);
        } else {
            // Still check for highest priority color
            let info2 = status_info(s);
            if highest_key.map_or(true, |(_, p)| info2.priority > p) {
                highest_key = Some((info2.theme_key, info2.priority));
            }
        }
    }
    let lowest = lowest?;
    let theme_key = match highest_key {
        Some((key, p)) if p > lowest.priority => key,
        _ => lowest.theme_key,
    };
    Some(StatusDisplay {
        letter: lowest.letter,
        theme_key,
    })
}

pub fn line_status_theme(kind: LineStatusKind) -> &'static str {
    match kind {
        LineStatusKind::GitAdded => "git.gutter_added",
        LineStatusKind::GitModified => "git.gutter_modified",
    }
}
