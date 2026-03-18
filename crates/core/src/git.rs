use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileStatus {
    GitModified,
    GitAdded,
    GitUntracked,
}

pub struct StatusDisplay {
    pub letter: char,
    pub theme_key: &'static str,
}

#[derive(Clone, Copy)]
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
///
/// Both lowest and highest are tracked independently so iteration
/// order of the HashSet cannot affect the result.
pub fn resolve_display(statuses: &HashSet<FileStatus>) -> Option<StatusDisplay> {
    if statuses.is_empty() {
        return None;
    }
    let mut lowest: Option<StatusInfo> = None;
    let mut highest: Option<StatusInfo> = None;
    for &s in statuses {
        let info = status_info(s);
        if lowest.map_or(true, |l| info.priority < l.priority) {
            lowest = Some(info);
        }
        if highest.map_or(true, |h| info.priority > h.priority) {
            highest = Some(info);
        }
    }
    let lowest = lowest?;
    let theme_key = match highest {
        Some(h) if h.priority > lowest.priority => h.theme_key,
        _ => lowest.theme_key,
    };
    Some(StatusDisplay {
        letter: lowest.letter,
        theme_key,
    })
}

/// Aggregate file statuses for all files under a directory.
pub fn directory_statuses(
    file_statuses: &HashMap<PathBuf, HashSet<FileStatus>>,
    dir: &Path,
) -> HashSet<FileStatus> {
    let mut result = HashSet::new();
    for (path, statuses) in file_statuses {
        if path.starts_with(dir) && path != dir {
            result.extend(statuses);
        }
    }
    result
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

/// Binary search for the line status covering `row`.
pub fn line_status_at(statuses: &[LineStatus], row: usize) -> Option<LineStatusKind> {
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
