use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory { expanded: bool },
}

#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub depth: usize,
    pub kind: EntryKind,
}

pub struct FileBrowser {
    pub root: PathBuf,
    pub entries: Vec<TreeEntry>,
    pub selected: usize,
    expanded_dirs: HashSet<PathBuf>,
}

impl FileBrowser {
    pub fn new(root: PathBuf) -> Self {
        let mut browser = Self {
            root,
            entries: Vec::new(),
            selected: 0,
            expanded_dirs: HashSet::new(),
        };
        browser.rebuild();
        browser
    }

    pub fn rebuild(&mut self) {
        self.entries.clear();
        self.walk_dir(&self.root.clone(), 0);
        if self.selected >= self.entries.len() {
            self.selected = self.entries.len().saturating_sub(1);
        }
    }

    fn walk_dir(&mut self, dir: &PathBuf, depth: usize) {
        let Ok(read_dir) = fs::read_dir(dir) else {
            return;
        };

        let mut dirs = Vec::new();
        let mut files = Vec::new();

        for entry in read_dir.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Filter hidden files
            if name_str.starts_with('.') {
                continue;
            }

            if path.is_dir() {
                dirs.push(path);
            } else {
                files.push(path);
            }
        }

        dirs.sort();
        files.sort();

        // Directories first, then files
        for path in dirs {
            let expanded = self.expanded_dirs.contains(&path);
            self.entries.push(TreeEntry {
                path: path.clone(),
                depth,
                kind: EntryKind::Directory { expanded },
            });
            if expanded {
                self.walk_dir(&path, depth + 1);
            }
        }

        for path in files {
            self.entries.push(TreeEntry {
                path,
                depth,
                kind: EntryKind::File,
            });
        }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    /// If file, returns path for the caller to open.
    /// If directory, toggles expansion.
    pub fn open_selected(&mut self) -> Option<PathBuf> {
        let entry = self.entries.get(self.selected)?.clone();
        match entry.kind {
            EntryKind::File => Some(entry.path),
            EntryKind::Directory { expanded } => {
                if expanded {
                    self.expanded_dirs.remove(&entry.path);
                } else {
                    self.expanded_dirs.insert(entry.path);
                }
                self.rebuild();
                None
            }
        }
    }

    /// If directory and collapsed, expand it.
    pub fn expand_selected(&mut self) {
        let Some(entry) = self.entries.get(self.selected) else { return };
        if matches!(entry.kind, EntryKind::Directory { expanded: false }) {
            let path = entry.path.clone();
            self.expanded_dirs.insert(path);
            self.rebuild();
        }
    }

    /// Collapse current dir, or walk up to parent dir and collapse it.
    pub fn collapse_selected(&mut self) {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return;
        };

        match &entry.kind {
            EntryKind::Directory { expanded: true } => {
                self.expanded_dirs.remove(&entry.path);
                self.rebuild();
            }
            _ => {
                // Find parent directory entry and collapse it
                if let Some(parent) = entry.path.parent() {
                    let parent_path = parent.to_path_buf();
                    if parent_path != self.root {
                        self.expanded_dirs.remove(&parent_path);
                        // Move selection to the parent
                        self.rebuild();
                        for (i, e) in self.entries.iter().enumerate() {
                            if e.path == parent_path {
                                self.selected = i;
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn expanded_dirs(&self) -> &HashSet<PathBuf> {
        &self.expanded_dirs
    }

    pub fn set_expanded_dirs(&mut self, dirs: HashSet<PathBuf>) {
        self.expanded_dirs = dirs;
        self.rebuild();
    }

    pub fn display_name(entry: &TreeEntry) -> String {
        let name = entry
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let indent = "  ".repeat(entry.depth);

        match &entry.kind {
            EntryKind::Directory { expanded: true } => format!("{indent}\u{25bd} {name}"),
            EntryKind::Directory { expanded: false } => format!("{indent}\u{25b7} {name}"),
            EntryKind::File => format!("{indent}  {name}"),
        }
    }
}
