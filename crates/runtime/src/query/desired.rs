//! "Desired state" memos — what the runtime should request next
//! tick of inlay hints, syntax parses, LSP buffer-changed pushes,
//! file watches, and watched-file LSP notifications.

use led_core::{BufferVersion, CanonPath, ServerId};
use led_driver_file_watch_core::{ChangeKinds, FileWatchEvent, Registration};
use led_driver_lsp_core::{FileEvent, FileEventKind, LspCmd};
use led_driver_syntax_core::SyntaxCmd;
use std::collections::HashMap;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

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
/// Reads `eb.live_content_hash` — stamped at every rope mutation
/// site (`bump()`, reload, undo restore, peer sync, LSP text
/// edits, save cleanup) — so this memo never walks the rope.
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
        out.push(LspCmd::BufferChanged {
            path: path.clone(),
            rope: eb.rope.clone(),
            hash: eb.live_content_hash,
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

/// Stage 1 of [`lsp_watched_file_notifications`] — narrow each
/// `FileWatchEvent::Changed` on the root watcher into a stable
/// `(path, FileEventKind)` pair (or drop it).
///
/// Returns the events in their original queue order. Idle ticks
/// (no root-watch entry, or only non-`Changed` events) yield an
/// empty Vec via cache-hit. Caches on `FileWatchEventsInput` only
/// — a glob registration change does not invalidate this stage.
#[drv::memo(single)]
pub fn filtered_watch_events<'a>(
    events: FileWatchEventsInput<'a>,
) -> Arc<Vec<(CanonPath, FileEventKind)>> {
    let Some(queue) = events.recent_events.get(&crate::WATCHER_ID_ROOT)
    else {
        return Arc::new(Vec::new());
    };
    let mut out: Vec<(CanonPath, FileEventKind)> = Vec::new();
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
        out.push((path.clone(), lsp_kind));
    }
    Arc::new(out)
}

/// Stage 2 of [`lsp_watched_file_notifications`] — match each
/// filtered event against every server's registered globs, applying
/// the kind-priority promotion (`Deleted > Changed > Created`) when
/// the same path appears multiple times in one tick.
///
/// Empty when no server has any matching glob. Calls
/// [`filtered_watch_events`] internally so a glob-only change
/// reuses the filtered list via that memo's cache.
#[drv::memo(single)]
pub fn per_server_matched<'a, 'b>(
    events: FileWatchEventsInput<'a>,
    globs: LspWatchedGlobsInput<'b>,
) -> Arc<HashMap<ServerId, HashMap<CanonPath, FileEventKind>>> {
    if globs.by_server.is_empty() {
        return Arc::new(HashMap::new());
    }
    let filtered = filtered_watch_events(events);
    if filtered.is_empty() {
        return Arc::new(HashMap::new());
    }
    let mut per_server: HashMap<ServerId, HashMap<CanonPath, FileEventKind>> =
        HashMap::new();
    for (path, lsp_kind) in filtered.iter() {
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
            let promote = match (entry.get(path), *lsp_kind) {
                (None, _) => true,
                (Some(prev), new) => kind_priority(new) >= kind_priority(*prev),
            };
            if promote {
                entry.insert(path.clone(), *lsp_kind);
            }
        }
    }
    Arc::new(per_server)
}

/// "Which language servers should be notified of which file
/// changes this tick?" Walks the root-recursive watcher's events,
/// drops `.git/` internal noise, and matches each surviving event
/// against every server's registered globs. Returns one
/// `LspCmd::DidChangeWatchedFiles` per affected server with a
/// stable-sorted batch.
///
/// Composed from [`filtered_watch_events`] +
/// [`per_server_matched`]. This top-level memo only re-runs when
/// the per-server map actually changes (it Arc-clones that result
/// from stage 2, then sorts each server's batch + wraps it as
/// `LspCmd`). Idle / no-event ticks: empty `recent_events` →
/// cache-hit the empty Vec all the way through.
#[drv::memo(single)]
pub fn lsp_watched_file_notifications<'a, 'b>(
    events: FileWatchEventsInput<'a>,
    globs: LspWatchedGlobsInput<'b>,
) -> Arc<Vec<LspCmd>> {
    let per_server = per_server_matched(events, globs);
    if per_server.is_empty() {
        return Arc::new(Vec::new());
    }
    let cmds = per_server
        .iter()
        .map(|(server, by_path)| {
            let mut changes: Vec<FileEvent> = by_path
                .iter()
                .map(|(path, kind)| FileEvent {
                    path: path.clone(),
                    kind: *kind,
                })
                .collect();
            changes.sort_by(|a, b| a.path.as_path().cmp(b.path.as_path()));
            LspCmd::DidChangeWatchedFiles {
                server: server.clone(),
                changes,
            }
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

/// The indent string the editor should use for `path`'s `line` —
/// the tree-sitter `suggest_indent` result when a language and a
/// parse tree are available, falling back to the line's existing
/// leading whitespace otherwise.
///
/// Pure derivation: no rope mutation, no cursor read. The caller
/// (`insert_newline` / `insert_tab` in dispatch) consumes the
/// resolved indent string and applies it.
///
/// Returns `None` when the buffer is not loaded (the caller skips
/// the edit entirely in that case). The `Indent` payload spells
/// out which path produced the result so callers can branch on it
/// without re-deriving — `InsertTab` replaces the existing indent
/// when the tree path fired, but only when the cursor sits
/// inside the indent prefix; the fallback path inserts at the
/// cursor instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesiredIndent {
    /// The tree-sitter indent query produced this string. Caller
    /// may use it both for newline (just splice it) and for tab
    /// (replace existing leading whitespace).
    FromTree(Arc<str>),
    /// No tree available (no language / no parsed tree / no
    /// suggestion at this line); the string is the verbatim copy
    /// of the line's current leading whitespace.
    Fallback(Arc<str>),
}

impl DesiredIndent {
    /// The resolved indent string itself.
    pub fn as_str(&self) -> &str {
        match self {
            DesiredIndent::FromTree(s) | DesiredIndent::Fallback(s) => s.as_ref(),
        }
    }

    /// Indent length in grapheme clusters — what `Cursor::col`
    /// measures.
    pub fn grapheme_len(&self) -> usize {
        self.as_str().graphemes(true).count()
    }
}

/// One position-keyed replace in a save-cleanup batch. `at` is a
/// char index into the rope; the dispatch reducer applies the
/// batch in descending-`at` order so each remove + insert stays
/// position-valid as the rope shrinks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveCleanupReplace {
    pub at: usize,
    pub removed: Arc<str>,
    pub inserted: Arc<str>,
}

/// "What pre-save cleanup edits should we apply to `path`?"
///
/// One memo call per save request. Returns the sorted list of
/// `(at, removed, inserted)` replace ops the dispatcher should
/// apply to the buffer — trailing-whitespace strips per line,
/// plus a final-newline append when the rope doesn't end in
/// one. Already-clean buffers (or unloaded paths) return an
/// empty Vec → caller short-circuits.
///
/// The output is sorted descending by `at` so the caller's
/// remove + insert loop stays index-valid (a higher-position
/// edit doesn't shift any lower-position edit).
///
/// Pure: no rope mutation, no version bump, no history record.
/// The dispatcher reads the plan and applies it (the apply must
/// stay in dispatch because it touches multiple sources: rope,
/// version, hash, history, cursor).
#[drv::memo(single)]
pub fn save_cleanup_plan<'b>(
    edits: EditedBuffersInput<'b>,
    path: &CanonPath,
) -> Arc<Vec<SaveCleanupReplace>> {
    let Some(eb) = edits.buffers.get(path) else {
        return Arc::new(Vec::new());
    };
    let total_chars = eb.rope.len_chars();
    if total_chars == 0 {
        return Arc::new(Vec::new());
    }
    let line_count = eb.rope.len_lines();
    let mut replaces: Vec<SaveCleanupReplace> = Vec::new();
    if eb.rope.char(total_chars - 1) != '\n' {
        replaces.push(SaveCleanupReplace {
            at: total_chars,
            removed: Arc::from(""),
            inserted: Arc::from("\n"),
        });
    }
    for line_idx in 0..line_count {
        let line_slice = eb.rope.line(line_idx);
        let line_str: String = line_slice.chars().collect();
        let body = line_str.trim_end_matches(['\n', '\r']);
        let body_chars = body.chars().count();
        if body_chars == 0 {
            continue;
        }
        let trimmed = body.trim_end_matches([' ', '\t']);
        let trimmed_chars = trimmed.chars().count();
        if trimmed_chars == body_chars {
            continue;
        }
        let line_start_char = eb.rope.line_to_char(line_idx);
        let strip_start = line_start_char + trimmed_chars;
        let strip_end = line_start_char + body_chars;
        let removed: String = eb.rope.slice(strip_start..strip_end).to_string();
        replaces.push(SaveCleanupReplace {
            at: strip_start,
            removed: Arc::from(removed.as_str()),
            inserted: Arc::from(""),
        });
    }
    // Descending position so the highest-position edit applies
    // first; strips at lower positions stay valid because the
    // higher-position edits don't shift them.
    replaces.sort_by_key(|r| std::cmp::Reverse(r.at));
    Arc::new(replaces)
}

/// Outcome of running the completion refilter against the
/// current cursor / rope. Pure data — the dispatch reducer
/// applies it to `CompletionsState`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionRefilterOutcome {
    /// No active session to refilter.
    NoSession,
    /// Session has lost its anchor (tab gone, cursor wandered off
    /// the prefix line, rope desynced, every candidate dropped,
    /// or the sole candidate equals the typed prefix). Caller
    /// dismisses.
    Dismiss,
    /// Refilter result is non-empty and still meaningful. Caller
    /// writes these back into the session.
    Update {
        filtered: Arc<Vec<usize>>,
        selected: usize,
        /// Caller should clamp `session.scroll` down to `selected`
        /// when `selected < session.scroll`.
        scroll_max: usize,
    },
}

/// Outcome of inspecting an active tab for a completion request
/// anchor (line + utf-16 col). Pure data; the caller queues the
/// LSP request via its outbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionAnchorOutcome {
    /// No active tab, no loaded buffer, or the tab is a preview —
    /// caller silently drops the trigger.
    Skip,
    /// Anchor resolved. `path` is the buffer's canonical path;
    /// `line` and `col_utf16` are the LSP request coordinates.
    Anchor {
        path: CanonPath,
        line: u32,
        col_utf16: u32,
    },
}

/// "Where would a fresh completion request anchor itself right
/// now?" — single source of truth for the LSP request
/// coordinates dispatch ships on identifier-char auto-trigger.
///
/// Reads the active tab + its buffer. Returns `Skip` when no
/// active tab, no loaded buffer, or the tab is preview-only.
/// Otherwise returns the canonical path plus the LSP
/// `Position::character` (UTF-16 code units, per
/// `PositionEncodingKind`) derived from the cursor's grapheme
/// col.
#[drv::memo(single)]
pub fn completion_request_anchor<'t, 'b>(
    tabs: TabsActiveInput<'t>,
    edits: EditedBuffersInput<'b>,
) -> CompletionAnchorOutcome {
    let Some(id) = tabs.active else {
        return CompletionAnchorOutcome::Skip;
    };
    let Some(tab) = tabs.open.iter().find(|t| t.id == *id) else {
        return CompletionAnchorOutcome::Skip;
    };
    if tab.preview {
        return CompletionAnchorOutcome::Skip;
    }
    let Some(eb) = edits.buffers.get(&tab.path) else {
        return CompletionAnchorOutcome::Skip;
    };
    let line = tab.cursor.line as u32;
    let col_utf16 = if tab.cursor.line < eb.rope.len_lines() {
        led_core::grapheme_col_to_utf16_units(eb.rope.line(tab.cursor.line), tab.cursor.col)
    } else {
        0
    };
    CompletionAnchorOutcome::Anchor {
        path: tab.path.clone(),
        line,
        col_utf16,
    }
}

/// "Given the live completion session, the active tab's cursor,
/// and the buffer rope — what should the popup look like after
/// this keystroke?" Single source of truth for the refilter
/// branch.
///
/// Pure derivation: walks the same dismissal checks
/// `refresh_completion_filter` used to do inline (tab/buffer
/// gone, cursor off the prefix line, cursor left of
/// `prefix_start_col`, rope shorter than expected), runs the
/// fuzzy refilter, and folds in the "sole candidate equals
/// prefix" suppression. The reducer in dispatch reads the
/// outcome and writes the corresponding mutation onto
/// `CompletionsState`.
///
/// The cache here is most useful for idle ticks (cursor parked,
/// session steady) — every keystroke moves the cursor and
/// invalidates.
#[drv::memo(single)]
pub fn completion_refilter_outcome<'c, 't, 'b>(
    completions: CompletionsSessionInput<'c>,
    tabs: TabsActiveInput<'t>,
    edits: EditedBuffersInput<'b>,
) -> CompletionRefilterOutcome {
    let Some(session) = completions.session.as_ref() else {
        return CompletionRefilterOutcome::NoSession;
    };
    let Some(tab) = tabs.open.iter().find(|t| t.id == session.tab) else {
        return CompletionRefilterOutcome::Dismiss;
    };
    let Some(eb) = edits.buffers.get(&tab.path) else {
        return CompletionRefilterOutcome::Dismiss;
    };
    if tab.cursor.line as u32 != session.prefix_line {
        return CompletionRefilterOutcome::Dismiss;
    }
    if (tab.cursor.col as u32) < session.prefix_start_col {
        return CompletionRefilterOutcome::Dismiss;
    }
    let line_idx = session.prefix_line as usize;
    if line_idx >= eb.rope.len_lines() {
        return CompletionRefilterOutcome::Dismiss;
    }
    let line_slice = eb.rope.line(line_idx);
    let line_start = eb.rope.line_to_char(line_idx);
    let from =
        line_start + led_core::grapheme_col_to_char(line_slice, session.prefix_start_col as usize);
    let to = line_start + led_core::grapheme_col_to_char(line_slice, tab.cursor.col);
    if to < from || to > eb.rope.len_chars() {
        return CompletionRefilterOutcome::Dismiss;
    }
    let prefix: String = eb.rope.slice(from..to).to_string();
    let filtered = led_state_completions::refilter(&session.items, &prefix);
    if filtered.is_empty() {
        return CompletionRefilterOutcome::Dismiss;
    }
    // Sole candidate equals the typed prefix — committing would
    // be a no-op, so the popup is pure noise.
    if filtered.len() == 1
        && led_state_completions::is_identity_match(&session.items[filtered[0]], &prefix)
    {
        return CompletionRefilterOutcome::Dismiss;
    }
    // Preserve the highlighted label across the refilter when
    // possible — matches the UX users expect.
    let prev_selected_item = session.filtered.get(session.selected).copied();
    let new_selected = prev_selected_item
        .and_then(|item_ix| filtered.iter().position(|&i| i == item_ix))
        .unwrap_or(0);
    CompletionRefilterOutcome::Update {
        filtered: Arc::new(filtered),
        selected: new_selected,
        scroll_max: new_selected,
    }
}

/// "What's the desired leading indent for `path`'s line `line`?"
///
/// Single source of truth for the indent string used by Enter
/// (insert at cursor → push current indent onto the new line) and
/// Tab (replace existing indent with the structural one). The
/// reducer in dispatch reads the answer and applies it; the
/// derivation lives here so the "what should the indent be?"
/// decision isn't tangled with the rope mutation.
///
/// Returns `None` when the buffer is unloaded.
#[drv::memo(single)]
pub fn desired_indent_for_line<'s, 'b>(
    syntax: SyntaxStatesInput<'s>,
    edits: EditedBuffersInput<'b>,
    path: &CanonPath,
    line: usize,
) -> Option<DesiredIndent> {
    let eb = edits.buffers.get(path)?;
    // M23: ask the tree-sitter indent query what the *current*
    // line's structural indent should be.
    //
    // Asking for `line + 1` (the about-to-be-created line) gets
    // confused when the line below `line` is itself a closing
    // bracket (`}` / `)` / `]`): `suggest_indent`'s closing-bracket
    // short-circuit kicks in and returns the OPENER's line indent
    // (often empty), which would land the cursor flush-left.
    // Asking for `line` instead returns the structural indent of
    // the line we're splitting — which is what Enter and Tab both
    // want.
    let tree_indent = syntax
        .by_path
        .get(path)
        .and_then(|s| s.tree.as_ref().map(|t| (s.language, t)))
        .and_then(|(lang, tree)| {
            led_state_syntax::indent::suggest_indent(lang, tree, &eb.rope, line)
        });
    if let Some(s) = tree_indent {
        return Some(DesiredIndent::FromTree(Arc::from(s.as_str())));
    }
    // Fallback: copy the line's leading whitespace verbatim.
    let leading: String = eb
        .rope
        .line(line)
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    Some(DesiredIndent::Fallback(Arc::from(leading.as_str())))
}
