//! "Desired state" memos — what the runtime should request next
//! tick of inlay hints, syntax parses, LSP buffer-changed pushes,
//! file watches, and watched-file LSP notifications.

use led_core::{BufferVersion, CanonPath, ServerId};
use led_driver_file_watch_core::{ChangeKinds, FileWatchEvent, Registration};
use led_driver_lsp_core::{FileEvent, FileEventKind, LspCmd};
use led_driver_syntax_core::SyntaxCmd;
use std::sync::Arc;

use super::inputs::*;

/// "Which buffers need a fresh inlay-hint request?"
///
/// Returns one tuple `(path, version, start_line, end_line)` per
/// open buffer whose latest version hasn't been requested yet.
/// Toggle off → empty vec. Idle ticks → cache-hit empty vec.
///
/// Pure: doesn't allocate seqs (that's execute-side via
/// `LspPending::queue_inlay_hints`).
#[drv::memo(single)]
pub fn desired_inlay_hint_requests<'a, 'e, 'r>(
    edits: EditedBuffersInput<'a>,
    extras: LspInlayHintsEnabledInput<'e>,
    requested: LspInlayHintsRequestedInput<'r>,
) -> Arc<Vec<(CanonPath, BufferVersion, u32, u32)>> {
    if !*extras.enabled {
        return Arc::new(Vec::new());
    }
    let mut out: Vec<(CanonPath, BufferVersion, u32, u32)> = Vec::new();
    for (path, eb) in edits.buffers.iter() {
        if requested.by_path.get(path) == Some(&eb.version) {
            continue;
        }
        let end_line = eb
            .rope
            .len_lines()
            .saturating_sub(1)
            .min(u32::MAX as usize) as u32;
        out.push((path.clone(), eb.version, 0, end_line));
    }
    Arc::new(out)
}

/// "Which buffers need a fresh tree-sitter parse?"
///
/// Skips buffers without a known language, buffers whose tokens
/// already track the current rope version, and buffers with an
/// in-flight parse at the same version. Idle ticks return an
/// empty Vec via cache-hit.
#[drv::memo(single)]
pub fn desired_syntax_parses<'s, 'b>(
    syntax: SyntaxStatesInput<'s>,
    edits: EditedBuffersInput<'b>,
) -> Arc<Vec<SyntaxCmd>> {
    let mut out: Vec<SyntaxCmd> = Vec::new();
    for (path, state) in syntax.by_path.iter() {
        let Some(eb) = edits.buffers.get(path) else {
            continue;
        };
        // Needs a parse if we've never parsed this buffer OR the
        // rope has moved past the last-applied tokens. The
        // initial load sits at `eb.version == state.version == 0`,
        // so without the `tree.is_none()` branch the first parse
        // would never fire.
        let needs_parse = state.tree.is_none() || eb.version > state.version;
        if !needs_parse {
            continue;
        }
        if state.in_flight_version == Some(eb.version) {
            continue;
        }
        out.push(SyntaxCmd {
            path: path.clone(),
            version: eb.version,
            rope: eb.rope.clone(),
            language: state.language,
            prev_tree: state.tree.clone(),
            prev_rope: state.tree_rope.clone(),
        });
    }
    Arc::new(out)
}

/// "Which buffers need a `BufferChanged` push to LSP?"
///
/// One `LspCmd::BufferChanged` per buffer whose `version` or
/// `saved_version` has moved past what `lsp_notified` records.
/// Idle ticks (no version moves): empty Arc<Vec>.
///
/// Memoised so the per-buffer `EphemeralContentHash::of_rope`
/// walk doesn't re-fire when nothing has changed.
#[drv::memo(single)]
pub fn desired_lsp_buffer_changed<'a, 'b>(
    edits: EditedBuffersInput<'a>,
    notified: LspNotifiedInput<'b>,
) -> Arc<Vec<LspCmd>> {
    let mut out: Vec<LspCmd> = Vec::new();
    for (path, eb) in edits.buffers.iter() {
        let last = notified.by_path.get(path).copied().unwrap_or_default();
        let version_moved = eb.version > last.version;
        let save_happened = eb.saved_version > last.saved_version;
        if !(version_moved || save_happened) {
            continue;
        }
        // `is_save` = the writer reported this tick (saved_version
        // advanced AND it has caught up to version). Separate from
        // `version_moved` because a pure-save tick (no new edits)
        // still needs `didSave` → cargo check.
        let is_save = save_happened && eb.saved_version.0 == eb.version.0;
        let hash = led_core::EphemeralContentHash::of_rope(&eb.rope).persist();
        out.push(LspCmd::BufferChanged {
            path: path.clone(),
            rope: eb.rope.clone(),
            hash,
            is_save,
        });
    }
    Arc::new(out)
}

/// "What watches do we want active right now?"
///
/// Returns a path-keyed map: workspace root recursive, the
/// `<config>/notify/` directory, and one parent dir per open
/// buffer whose parent isn't already covered by the root watch.
///
/// Pure: no `std::fs::canonicalize` syscalls (per-buffer parents
/// inherit canonical-ness from the buffer path itself), no
/// `WatchSeq` allocation (id minting is execute-side concern).
/// Idle ticks cache-hit.
#[drv::memo(single)]
pub fn desired_watches<'r, 'n, 'b>(
    root: FsRootInput<'r>,
    notify_dir: NotifyDirInput<'n>,
    edits: EditedBuffersInput<'b>,
) -> Arc<imbl::HashMap<CanonPath, Registration>> {
    let mut out: imbl::HashMap<CanonPath, Registration> = imbl::HashMap::new();
    let (Some(root_path), Some(notify_path)) = (root.root.as_ref(), notify_dir.notify_dir.as_ref())
    else {
        return Arc::new(out);
    };
    out.insert(
        root_path.clone(),
        Registration {
            path: root_path.clone(),
            recursive: true,
            debounce_ms: 0,
        },
    );
    out.insert(
        notify_path.clone(),
        Registration {
            path: notify_path.clone(),
            recursive: false,
            debounce_ms: 100,
        },
    );
    let root_p = root_path.as_path();
    for path in edits.buffers.keys() {
        let Some(parent) = path.parent_canon() else {
            continue;
        };
        // notify refuses to watch the same path twice; events
        // already arrive on the root watch when the parent is
        // covered there.
        if parent.as_path() == root_p || parent.as_path().starts_with(root_p) {
            continue;
        }
        out.insert(
            parent.clone(),
            Registration {
                path: parent,
                recursive: false,
                debounce_ms: 0,
            },
        );
    }
    Arc::new(out)
}

/// "Which language servers should be notified of which file
/// changes this tick?" Walks the root-recursive watcher's events,
/// drops `.git/` internal noise, and matches each surviving event
/// against every server's registered globs. Returns one
/// `LspCmd::DidChangeWatchedFiles` per affected server with a
/// stable-sorted batch.
///
/// Idle / no-event ticks: empty `recent_events` → cache-hit the
/// empty Vec.
#[drv::memo(single)]
pub fn lsp_watched_file_notifications<'a, 'b>(
    events: FileWatchEventsInput<'a>,
    globs: LspWatchedGlobsInput<'b>,
) -> Arc<Vec<LspCmd>> {
    if globs.by_server.is_empty() {
        return Arc::new(Vec::new());
    }
    let Some(queue) = events.recent_events.get(&crate::WATCHER_ID_ROOT)
    else {
        return Arc::new(Vec::new());
    };
    let mut per_server: std::collections::HashMap<
        ServerId,
        std::collections::HashMap<CanonPath, FileEventKind>,
    > = std::collections::HashMap::new();
    for ev in queue {
        let FileWatchEvent::Changed { path, kinds, .. } = ev else {
            continue;
        };
        let lsp_kind = if kinds.contains_any(ChangeKinds::REMOVED) {
            FileEventKind::Deleted
        } else if kinds.contains_any(ChangeKinds::MODIFIED) {
            FileEventKind::Changed
        } else if kinds.contains_any(ChangeKinds::CREATED) {
            FileEventKind::Created
        } else {
            continue;
        };
        let kind_bit: u8 = match lsp_kind {
            FileEventKind::Created => ChangeKinds::CREATED,
            FileEventKind::Changed => ChangeKinds::MODIFIED,
            FileEventKind::Deleted => ChangeKinds::REMOVED,
        };
        let path_for_match = path.as_path();
        for (server, registrations) in globs.by_server.iter() {
            let matched = registrations.values().any(|globs| {
                globs.iter().any(|g| {
                    g.kinds & kind_bit != 0 && g.matcher.is_match(path_for_match)
                })
            });
            if !matched {
                continue;
            }
            let entry = per_server.entry(server.clone()).or_default();
            let promote = match (entry.get(path), lsp_kind) {
                (None, _) => true,
                (Some(prev), new) => kind_priority(new) >= kind_priority(*prev),
            };
            if promote {
                entry.insert(path.clone(), lsp_kind);
            }
        }
    }
    let cmds = per_server
        .into_iter()
        .map(|(server, by_path)| {
            let mut changes: Vec<FileEvent> = by_path
                .into_iter()
                .map(|(path, kind)| FileEvent { path, kind })
                .collect();
            changes.sort_by(|a, b| a.path.as_path().cmp(b.path.as_path()));
            LspCmd::DidChangeWatchedFiles { server, changes }
        })
        .collect();
    Arc::new(cmds)
}

fn kind_priority(k: FileEventKind) -> u8 {
    match k {
        FileEventKind::Created => 1,
        FileEventKind::Changed => 2,
        FileEventKind::Deleted => 3,
    }
}
