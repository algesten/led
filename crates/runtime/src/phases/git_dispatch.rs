//! Git driver dispatch + tail-end file-watch event drain.
//!
//! Startup-one-shot + per-save scan dispatch gated on the workspace
//! root being a real git repo (`.git/` exists). Standalone /
//! no-workspace mode discards the per-save flag.
//!
//! Trailing `file_watch.clear_events()` belongs here because the
//! event queue must be drained AFTER all consumers (the ingest
//! reread / sync-check fan-out plus this dispatch's git scan) have
//! observed it.

use led_driver_git_core::GitCmd;

use crate::phases::TickEnv;
use crate::Atoms;

pub(crate) fn run(atoms: &mut Atoms, env: &TickEnv<'_>) {
    let Atoms {
        tabs,
        edits,
        fs,
        git_scan_dispatched,
        git_scan_pending,
        file_watch,
        ..
    } = atoms;

    if let Some(root) = fs.root.as_ref()
        && !env.no_workspace
    {
        let save_pending = std::mem::take(git_scan_pending);
        let any_pending_load = tabs
            .open
            .iter()
            .any(|t| !edits.buffers.contains_key(&t.path));
        let initial_scan_ready = *git_scan_dispatched || !any_pending_load;
        let want_scan = !*git_scan_dispatched || save_pending;
        if want_scan && initial_scan_ready && root.as_path().join(".git").exists() {
            env.drivers.git.execute(std::iter::once(&GitCmd::ScanFiles {
                root: root.clone(),
            }));
            *git_scan_dispatched = true;
        } else if want_scan && initial_scan_ready {
            *git_scan_dispatched = true;
        } else if save_pending {
            *git_scan_pending = true;
        }
    } else {
        *git_scan_pending = false;
    }

    file_watch.clear_events();
}
