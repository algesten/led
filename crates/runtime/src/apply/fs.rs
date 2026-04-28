//! Filesystem / workspace-tree helpers extracted from `lib.rs`.
//!
//! Verbatim moves of: `apply_workspace_tree_delta`,
//! `invalidate_subtree`, `clear_ancestor_failures`, `stat_kind`,
//! `is_git_internal`, `is_git_sentinel`, `reconcile_external_change`,
//! `refresh_after_external_change`, `diff_watch_actions`. (The
//! `synthesize_reread` helper that the legacy version of these
//! comments references lives in `apply::session`.) Visibility
//! bumped to `pub(crate)` so the main loop can call them.

use std::sync::Arc;

use led_core::{CanonPath, SavedVersion, WatchSeq};
use led_driver_buffers_core::RereadCompletion;
use led_state_browser::FsTree;
use led_state_buffer_edits::BufferEdits;

/// Diff the memoized `desired_watches` map against the driver's
/// current registry; emit one `FileWatchCmd` per change.
///
/// The desired-set computation is now a pure memo (`query::
/// desired_watches`); this function only does the id-reconciling
/// diff and `WatchSeq` minting that the memo can't (memos are
/// pure). Idle ticks: desired == registry-by-path → no `cmds`
/// pushed.
pub(crate) fn diff_watch_actions(
    desired: &imbl::HashMap<CanonPath, led_driver_file_watch_core::Registration>,
    file_watch: &led_driver_file_watch_core::FileWatchState,
    watch_id_seq: &mut WatchSeq,
    root: &CanonPath,
    notify_dir: &CanonPath,
) -> Vec<led_driver_file_watch_core::FileWatchCmd> {
    use led_driver_file_watch_core::FileWatchCmd;

    let mut cmds: Vec<FileWatchCmd> = Vec::new();
    for (path, reg) in desired.iter() {
        // Sentinel paths get fixed ids; per-buffer parents
        // reuse whatever id already covers the same path so
        // `Registration` shape comparisons stay stable.
        let id = if path == root {
            crate::WATCHER_ID_ROOT
        } else if path == notify_dir {
            crate::WATCHER_ID_NOTIFY_DIR
        } else {
            file_watch
                .registry
                .iter()
                .find(|(_, r)| &r.path == path)
                .map(|(id, _)| *id)
                .unwrap_or_else(|| {
                    let id = *watch_id_seq;
                    watch_id_seq.0 = watch_id_seq.0.saturating_add(1);
                    id
                })
        };
        match file_watch.registry.get(&id) {
            Some(existing) if existing == reg => {}
            _ => cmds.push(FileWatchCmd::Watch {
                id,
                path: reg.path.clone(),
                recursive: reg.recursive,
                debounce_ms: reg.debounce_ms,
            }),
        }
    }
    for (id, reg) in file_watch.registry.iter() {
        if !desired.contains_key(&reg.path) {
            cmds.push(FileWatchCmd::Unwatch { id: *id });
        }
    }
    cmds
}

/// Walk the root-recursive watcher's recent events and apply
/// per-event deltas to `fs.dir_contents` directly. Returns
/// `true` if any event signalled an external git command
/// (`.git/index|HEAD|refs/*`) and a git rescan should run.
///
/// # Why a delta, not a full clear+relist
///
/// The first cut of this code did `fs.dir_contents.clear()` on
/// every burst, then leaned on the `file_list_action` memo to
/// re-issue `ListDir` for every visible directory. That has
/// two problems for real projects:
///
/// 1. **Flicker.** Every event blanked the sidebar between
///    clear and the round-trip to fs-list.
/// 2. **Scale.** A burst of N events relisted every visible
///    directory, even those untouched by the burst — so a
///    cargo `target/` build with the workspace root expanded
///    re-scanned the entire root + every expanded subdir per
///    debounce window. Doesn't fly for repos with thousands
///    of files.
///
/// The delta apply only touches the cached parent vector for
/// each event — O(1) work per CREATE/REMOVE. Events whose
/// parent dir isn't cached (e.g. anything under `target/` when
/// `target/` is collapsed) cost nothing.
///
/// # Filter rules
///
/// - **`.git/` internal paths** are dropped before any cache
///   work: `.git/index|HEAD|refs/*` ⇒ request a git rescan,
///   nothing else; any other `.git/*` (objects/, locks, pack/)
///   ⇒ ignored entirely. Without this filter FSEvents history
///   replay alone would keep the sidebar churning at startup.
/// - **MODIFIED-only events** never affect listings. The
///   external-reread path consumes them separately.
/// - **CREATED for an already-open buffer's path** is a known
///   FSEvents quirk (Create-on-install for a file that already
///   existed when the watch came up). Skipped — the
///   `compute_external_reread_targets` path handles real
///   content changes.
/// - **Events whose parent dir isn't in `dir_contents`** are
///   dropped: nobody is looking at that listing, so updating
///   it would cost stats with no UI benefit.
pub(crate) fn apply_workspace_tree_delta(
    file_watch: &led_driver_file_watch_core::FileWatchState,
    edits: &BufferEdits,
    fs: &mut FsTree,
) -> bool {
    use led_driver_file_watch_core::{ChangeKinds, FileWatchEvent};
    use led_driver_fs_list_core::DirEntry;
    let Some(queue) = file_watch.recent_events.get(&crate::WATCHER_ID_ROOT) else {
        return false;
    };
    let mut git_scan = false;
    for ev in queue {
        let FileWatchEvent::Changed { path, kinds, .. } = ev else {
            continue;
        };

        // `.git/` filter — see fn-doc above.
        if is_git_internal(path) {
            if is_git_sentinel(path) {
                git_scan = true;
            }
            continue;
        }

        // Listings only move on CREATE / REMOVE. MODIFIED-only
        // belongs to the reread path.
        let created = kinds.contains_any(ChangeKinds::CREATED);
        let removed = kinds.contains_any(ChangeKinds::REMOVED);
        if !created && !removed {
            continue;
        }

        let Some(parent) = path.as_path().parent() else {
            continue;
        };
        let parent = led_core::UserPath::new(parent.to_path_buf()).canonicalize();

        // REMOVED first, so a coalesced create+remove (rare on
        // 0 ms debounce, but FSEvents can do it) settles to the
        // post-create state when both bits are set.
        if removed {
            // Drop the entry from the parent's listing if cached.
            if let Some(children) = fs.dir_contents.get_mut(&parent) {
                children.retain(|e| &e.path != path);
            }
            // The removed path itself may have been an expanded
            // directory whose listing we cached. Drop that key
            // and any cached descendants — every cached entry
            // under it is now stale.
            invalidate_subtree(fs, path);
        }

        if created {
            // Recovery path for `failed_dirs`: a CREATE under
            // `path` (or for `path` itself) proves the dir tree
            // up to `path`'s parent now exists. Walk every
            // ancestor up to the workspace root and drop any
            // matching `failed_dirs` entry so the next tick's
            // `file_list_action` re-emits `ListCmd::List` for
            // the recovered dir. Without this hook, a re-mkdir
            // or git checkout under the recursive root would
            // leave the failure marker in place forever and the
            // sidebar would never re-populate.
            clear_ancestor_failures(fs, path);

            // Already-open buffer + CREATE: legacy quirk filter
            // (`docs/spec/buffers.md` § "External filesystem
            // change"). Skip the listing insert; the reread
            // path handles real content changes.
            if !removed && edits.buffers.contains_key(path) {
                continue;
            }
            // Hidden filter mirrors the fs-list driver native worker.
            let Some(name) = path.as_path().file_name() else {
                continue;
            };
            let name = name.to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            // Parent must be currently cached for the insert to
            // matter. If the user hasn't expanded `parent`, no
            // visible state changes — cheapest possible no-op.
            let Some(children) = fs.dir_contents.get_mut(&parent) else {
                continue;
            };
            // Dedup by path: FSEvents commonly delivers the
            // same Create twice (once for the open, once on
            // close), and our 0 ms debounce passes both.
            if children.iter().any(|e| &e.path == path) {
                continue;
            }
            // Stat to determine file vs directory. A failed
            // stat means the path was created and removed
            // before we got to it — drop the event.
            let Some(kind) = stat_kind(path) else {
                continue;
            };
            children.push_back(DirEntry {
                name,
                path: path.clone(),
                kind,
            });
            // Sort happens at render time
            // (`emit_children_of`), so push order doesn't
            // matter here. Avoiding the sort on the hot path
            // keeps a 10k-file burst at O(1) per event.
        }
    }
    git_scan
}

/// Drop `path` and every cached descendant from `fs.dir_contents`
/// and `fs.failed_dirs`. Called when a path is removed: any
/// listing we had under it is stale, and any cached "this
/// listing failed" verdict is also stale (the dir doesn't exist
/// at all now, no point gating future attempts on a verdict that
/// applied to a different inode). Cheap because cached entries
/// are typed `imbl::HashMap` / `imbl::HashSet` — retain walks
/// the keys but the values are pointer copies.
pub(crate) fn invalidate_subtree(fs: &mut FsTree, root: &CanonPath) {
    let prefix = root.as_path();
    fs.dir_contents
        .retain(|p, _| p != root && !p.as_path().starts_with(prefix));
    fs.failed_dirs
        .retain(|p| p != root && !p.as_path().starts_with(prefix));
}

/// Walk from `path` up through every ancestor (stopping at the
/// workspace root, or the filesystem root if there's no
/// workspace) and remove each one from `fs.failed_dirs`. Called
/// from the watcher's CREATE branch — a fresh entry anywhere
/// proves the dir chain leading to it is readable now. The walk
/// includes `path` itself so a `mkdir crates/timers` event
/// (where the new path equals the failed entry) recovers.
pub(crate) fn clear_ancestor_failures(fs: &mut FsTree, path: &CanonPath) {
    if fs.failed_dirs.is_empty() {
        return;
    }
    let stop = fs.root.as_ref().map(|r| r.as_path());
    let mut cur: Option<&std::path::Path> = Some(path.as_path());
    while let Some(p) = cur {
        let canon = led_core::UserPath::new(p.to_path_buf()).canonicalize();
        fs.failed_dirs.remove(&canon);
        if Some(p) == stop {
            break;
        }
        cur = p.parent();
    }
}

/// Stat `path` and classify it as file or directory. Returns
/// `None` for any I/O error or unsupported file type — caller
/// treats those as "drop this event".
pub(crate) fn stat_kind(path: &CanonPath) -> Option<led_driver_fs_list_core::DirEntryKind> {
    use led_driver_fs_list_core::DirEntryKind;
    let meta = std::fs::metadata(path.as_path()).ok()?;
    if meta.is_dir() {
        Some(DirEntryKind::Directory)
    } else if meta.is_file() {
        Some(DirEntryKind::File)
    } else {
        None
    }
}

/// True if any component of `path` is literally `.git`.
/// Matches every path inside a git metadata dir (the workspace
/// root's `.git/` and any nested submodule's `.git/`).
pub(crate) fn is_git_internal(path: &CanonPath) -> bool {
    use std::path::Component;
    path.as_path().components().any(|c| {
        matches!(c, Component::Normal(name) if name == std::ffi::OsStr::new(".git"))
    })
}

/// True if `path` is one of the git sentinel files whose
/// modification means an external git command has run:
/// `.git/index`, `.git/HEAD`, or `.git/refs/**`. Other paths
/// under `.git/` (objects/, lock files, pack/) are suppressed
/// entirely by the caller — they fire continuously and do not
/// signify a user-visible state change.
pub(crate) fn is_git_sentinel(path: &CanonPath) -> bool {
    use std::path::Component;
    let mut comps = path.as_path().components().peekable();
    while let Some(c) = comps.next() {
        let Component::Normal(name) = c else { continue };
        if name != std::ffi::OsStr::new(".git") {
            continue;
        }
        let Some(Component::Normal(child)) = comps.next() else {
            return false;
        };
        if child == std::ffi::OsStr::new("index") || child == std::ffi::OsStr::new("HEAD") {
            return comps.next().is_none();
        }
        if child == std::ffi::OsStr::new("refs") {
            // Anything under `refs/` (heads/, tags/, remotes/, …)
            // is a sentinel.
            return comps.next().is_some();
        }
        return false;
    }
    false
}

/// M26 — three-branch reconcile of an external-change reread.
///
/// Application logic in the ingest phase per `EXAMPLE-ARCH.md` §
/// "Invariant enforcement": cleans up the user-decision shadow
/// source `EditedBuffer.rope` in response to disk content (an
/// external fact) changing.
///
/// - **Clean buffer + new content** — replace the rope, refresh
///   `disk_content_hash`, push one `EditGroup` so `Ctrl-/` takes
///   the user back to the prior content, bump `version` and let
///   `saved_version` catch up so the buffer stays clean. Also
///   bump `git_scan_pending` and drop the parent dir from
///   `fs.dir_contents` so the sidebar relists — same
///   side-effects an in-editor save fires (the disk-side
///   transition is identical).
/// - **Dirty buffer + new content** — silently drop. Legacy
///   parity (`docs/spec/buffers.md` § "External filesystem
///   change") protects unsaved local edits. A future polish
///   adds an `Alert::Warn` and an explicit `Action::Reload`.
/// - **Hash matches our anchor** — no-op. This is either our own
///   save echoing back through the watcher or a peer wrote
///   identical bytes. If `dirty()` was somehow set despite the
///   hash matching, that's already incoherent — skip silently.
pub(crate) fn reconcile_external_change(
    reread: &RereadCompletion,
    edits: &mut BufferEdits,
    fs: &mut FsTree,
    git_scan_pending: &mut bool,
) {
    let new_rope = match &reread.result {
        Ok(r) => r.clone(),
        Err(_) => return, // Read failed; nothing to reconcile.
    };
    let Some(eb) = edits.buffers.get_mut(&reread.path) else {
        return; // Buffer no longer materialised.
    };
    let new_hash = led_core::EphemeralContentHash::of_rope(&new_rope).persist();
    let dirty = eb.dirty();
    let hash_matches = new_hash == eb.disk_content_hash;
    match (dirty, hash_matches) {
        (false, false) => {
            // Clean reload. Push one group so undo can restore the
            // prior content; replace the rope; advance version and
            // saved_version together so the buffer stays clean.
            let prev_text: Arc<str> = Arc::from(eb.rope.to_string().as_str());
            let new_text: Arc<str> = Arc::from(new_rope.to_string().as_str());
            let cursor_before = led_state_tabs::Cursor::default();
            let cursor_after = led_state_tabs::Cursor::default();
            eb.history.record_replace(
                0,
                prev_text,
                new_text,
                cursor_before,
                cursor_after,
                None,
            );
            eb.rope = new_rope;
            eb.disk_content_hash = new_hash;
            eb.version.0 = eb.version.0.saturating_add(1);
            eb.saved_version = SavedVersion(eb.version.0);
            refresh_after_external_change(reread, fs, git_scan_pending);
        }
        (true, false) => {
            // Dirty + content diverges. Legacy parity: silent
            // drop — the user's local edits stay. But the disk
            // *did* change, so we still refresh the
            // workspace-tree side (sidebar listing + git scan)
            // since downstream queries care about disk state.
            refresh_after_external_change(reread, fs, git_scan_pending);
        }
        (_, true) => {
            // Hash matches our anchor — our own save echoing back
            // or a peer wrote identical bytes. No rope change.
            // (Future: if dirty() is true here it means a local
            //  edit converged with disk; nothing to do.)
        }
    }
}

/// Match the post-save side-effects after an external change:
/// drop the parent dir's cached listing so the sidebar relists,
/// and bump `git_scan_pending` so the next execute phase fires a
/// rescan.
pub(crate) fn refresh_after_external_change(
    reread: &RereadCompletion,
    fs: &mut FsTree,
    git_scan_pending: &mut bool,
) {
    *git_scan_pending = true;
    if let Some(parent) = reread.path.as_path().parent() {
        let parent_canon =
            led_core::UserPath::new(parent.to_path_buf()).canonicalize();
        fs.dir_contents.remove(&parent_canon);
    }
}
