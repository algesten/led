//! Find-file / save-as overlay dispatch.
//!
//! Stage 1: activation + deactivation only. Future stages add input
//! editing, tab completion, arrow navigation + preview, and commit
//! paths.
//!
//! Activation rules (matching legacy `compute_activate` /
//! `compute_activate_save_as`):
//!
//! - **Open**: initial input is the active buffer's parent directory
//!   with a trailing `/`, home-abbreviated to `~/...` when the parent
//!   lives under `$HOME`. Falls back to the workspace root (or the
//!   filesystem root `/` if no workspace) when there's no active
//!   buffer.
//! - **SaveAs**: initial input is the active buffer's full path
//!   (home-abbreviated). Caller can tweak the filename in place and
//!   hit Enter. Falls back to the workspace root + `/` when there's
//!   no active buffer.
//!
//! Future stages (M12 phases 2+):
//! - Driver completion requests (`FsFindFile` trace).
//! - `handle_action` for InsertChar / DeleteBack / arrows / Tab / Enter.
//! - Preview-tab side effects during arrow navigation.
//! - Open-mode paths A/B/C; SaveAs commit.

use led_state_browser::FsTree;
use led_state_find_file::{FindFileState, abbreviate_home};
use led_state_tabs::Tabs;

/// Enter Open mode. No-op if an overlay is already active — matches
/// legacy's "cannot nest find-file".
pub(super) fn activate_open(
    find_file: &mut Option<FindFileState>,
    tabs: &Tabs,
    fs: &FsTree,
) {
    if find_file.is_some() {
        return;
    }
    let input = compute_open_input(tabs, fs);
    *find_file = Some(FindFileState::open(input));
}

/// Enter SaveAs mode. No-op when already active.
pub(super) fn activate_save_as(
    find_file: &mut Option<FindFileState>,
    tabs: &Tabs,
    fs: &FsTree,
) {
    if find_file.is_some() {
        return;
    }
    let input = compute_save_as_input(tabs, fs);
    *find_file = Some(FindFileState::save_as(input));
}

/// Close the overlay. Idempotent. Future stages will also close
/// the preview tab (if any) and restore the previously-active tab.
pub(super) fn deactivate(find_file: &mut Option<FindFileState>) {
    *find_file = None;
}

fn compute_open_input(tabs: &Tabs, fs: &FsTree) -> String {
    let parent = active_path(tabs).and_then(|p| {
        p.as_path()
            .parent()
            .map(|pp| pp.to_path_buf())
    });
    let dir = parent.or_else(|| fs.root.as_ref().map(|r| r.as_path().to_path_buf()));
    let mut s = match dir {
        Some(p) => abbreviate_home(&p.to_string_lossy()),
        None => "/".to_string(),
    };
    if !s.ends_with('/') {
        s.push('/');
    }
    s
}

fn compute_save_as_input(tabs: &Tabs, fs: &FsTree) -> String {
    match active_path(tabs) {
        Some(p) => abbreviate_home(&p.as_path().to_string_lossy()),
        None => {
            let mut s = fs
                .root
                .as_ref()
                .map(|r| abbreviate_home(&r.as_path().to_string_lossy()))
                .unwrap_or_else(|| "/".to_string());
            if !s.ends_with('/') {
                s.push('/');
            }
            s
        }
    }
}

fn active_path(tabs: &Tabs) -> Option<&led_core::CanonPath> {
    let id = tabs.active?;
    tabs.open.iter().find(|t| t.id == id).map(|t| &t.path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;
    use led_state_tabs::{Tab, TabId};

    fn canon(s: &str) -> led_core::CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn tabs_with_active(path: &str) -> Tabs {
        let mut t = Tabs::default();
        t.open.push_back(Tab {
            id: TabId(1),
            path: canon(path),
            ..Default::default()
        });
        t.active = Some(TabId(1));
        t
    }

    #[test]
    fn activate_open_uses_parent_dir_of_active_buffer() {
        let tabs = tabs_with_active("/tmp/xyz/a.txt");
        let fs = FsTree::default();
        let mut ff = None;
        activate_open(&mut ff, &tabs, &fs);
        let s = ff.expect("activated");
        assert_eq!(s.input, "/tmp/xyz/");
        // Cursor sits at end-of-input for immediate editing.
        assert_eq!(s.cursor, s.input.len());
    }

    #[test]
    fn activate_save_as_uses_full_active_path() {
        let tabs = tabs_with_active("/tmp/xyz/a.txt");
        let fs = FsTree::default();
        let mut ff = None;
        activate_save_as(&mut ff, &tabs, &fs);
        let s = ff.expect("activated");
        assert_eq!(s.input, "/tmp/xyz/a.txt");
    }

    #[test]
    fn activate_open_no_active_falls_back_to_fs_root() {
        let tabs = Tabs::default();
        let fs = FsTree {
            root: Some(canon("/workspace")),
            ..Default::default()
        };
        let mut ff = None;
        activate_open(&mut ff, &tabs, &fs);
        let s = ff.expect("activated");
        assert_eq!(s.input, "/workspace/");
    }

    #[test]
    fn activate_is_no_op_when_already_active() {
        let tabs = tabs_with_active("/tmp/a.txt");
        let fs = FsTree::default();
        let mut ff = Some(FindFileState::open("preserved/".to_string()));
        activate_open(&mut ff, &tabs, &fs);
        assert_eq!(ff.as_ref().unwrap().input, "preserved/");
    }

    #[test]
    fn deactivate_clears_state() {
        let mut ff = Some(FindFileState::open("x/".into()));
        deactivate(&mut ff);
        assert!(ff.is_none());
    }
}
