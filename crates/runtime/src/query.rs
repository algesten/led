//! Cross-source query layer.
//!
//! Every memo that combines two or more drivers' sources lives here.
//! The drivers themselves are strictly isolated — they know only their
//! own data. The runtime owns the glue:
//!
//! - **`#[drv::input]` projections** for every subset of a driver
//!   source the runtime needs. Each carries a `new(&source)`
//!   constructor the call site uses to project.
//! - **Memos** that combine those inputs into actionable results:
//!   `LoadAction`s for `FileReadDriver::execute` to consume, and
//!   `Frame`s for `paint` to render.

#[allow(unused_imports)]
use led_core::CanonPath;
use led_driver_buffers_core::{BufferStore, LoadAction, LoadState, SaveAction};
use led_driver_clipboard_core::ClipboardAction;
use led_driver_fs_list_core::ListCmd;
use led_driver_terminal_core::{
    BodyModel, Dims, Frame, Layout, PopoverLine, PopoverModel, PopoverSeverity, Rect,
    SidePanelModel, SidePanelRow, StatusBarModel, TabBarModel, Terminal,
};
use led_state_alerts::AlertState;
use led_state_browser::{BrowserUi, Focus, TreeEntry, TreeEntryKind};
use led_state_clipboard::ClipboardState;
use led_state_buffer_edits::{BufferEdits, EditedBuffer};
use led_state_diagnostics::{
    BufferDiagnostics, Diagnostic, DiagnosticSeverity, DiagnosticsStates, LspServerStatus,
    LspStatuses,
};
use led_state_syntax::{SyntaxState, SyntaxStates, TokenKind, TokenSpan};
use led_state_tabs::{Cursor, Scroll, Tab, TabId, Tabs};
use ropey::Rope;
use std::sync::Arc;

// ── Inputs on Tabs ─────────────────────────────────────────────────────

/// Open-tabs slice (for "what files do we need loaded?" queries).
#[derive(drv::Input, Copy, Clone)]
pub struct TabsOpenInput<'a> {
    pub open: &'a imbl::Vector<Tab>,
}

impl<'a> TabsOpenInput<'a> {
    pub fn new(tabs: &'a Tabs) -> Self {
        Self { open: &tabs.open }
    }
}

/// Active-tab slice (for rendering: both `active` id and the `open`
/// list so the memo can resolve the id to a Tab).
#[derive(drv::Input, Copy, Clone)]
pub struct TabsActiveInput<'a> {
    pub open: &'a imbl::Vector<Tab>,
    pub active: &'a Option<TabId>,
}

impl<'a> TabsActiveInput<'a> {
    pub fn new(tabs: &'a Tabs) -> Self {
        Self {
            open: &tabs.open,
            active: &tabs.active,
        }
    }
}

// ── Inputs on BufferEdits ──────────────────────────────────────────────

/// Full `buffers` map for memos that read rope contents (body_model).
/// On cache hit the projection is a pointer copy; when an edit lands,
/// the `HashMap` pointer changes and the cache invalidates.
#[derive(drv::Input, Copy, Clone)]
pub struct EditedBuffersInput<'a> {
    pub buffers: &'a imbl::HashMap<CanonPath, EditedBuffer>,
}

impl<'a> EditedBuffersInput<'a> {
    pub fn new(edits: &'a BufferEdits) -> Self {
        Self {
            buffers: &edits.buffers,
        }
    }
}

/// Pending-save requests. Narrow projection so a cursor move or a
/// rope edit on `BufferEdits.buffers` doesn't invalidate save-related
/// memo caches.
#[derive(drv::Input, Copy, Clone)]
pub struct PendingSavesInput<'a> {
    pub paths: &'a imbl::HashSet<CanonPath>,
    pub save_as: &'a imbl::HashMap<CanonPath, CanonPath>,
}

impl<'a> PendingSavesInput<'a> {
    pub fn new(edits: &'a BufferEdits) -> Self {
        Self {
            paths: &edits.pending_saves,
            save_as: &edits.pending_save_as,
        }
    }
}

// ── Inputs on BufferStore ──────────────────────────────────────────────

/// Whole-map projection over `BufferStore`'s single field.
#[derive(drv::Input, Copy, Clone)]
pub struct StoreLoadedInput<'a> {
    pub loaded: &'a imbl::HashMap<CanonPath, LoadState>,
}

impl<'a> StoreLoadedInput<'a> {
    pub fn new(store: &'a BufferStore) -> Self {
        Self {
            loaded: &store.loaded,
        }
    }
}

// ── Input on SyntaxStates ──────────────────────────────────────────────

/// Whole-map projection over the `SyntaxStates.by_path` field. Memos
/// that need per-buffer token spans (just `body_model`) lens through
/// this; on cache hit the projection is a pointer copy because the
/// map is an `imbl::HashMap`.
#[derive(drv::Input, Copy, Clone)]
pub struct SyntaxStatesInput<'a> {
    pub by_path: &'a imbl::HashMap<CanonPath, SyntaxState>,
}

impl<'a> SyntaxStatesInput<'a> {
    pub fn new(s: &'a SyntaxStates) -> Self {
        Self {
            by_path: &s.by_path,
        }
    }
}

// ── Input on DiagnosticsStates ─────────────────────────────────────────

/// Per-buffer diagnostic projection. `body_model` consumes this
/// to paint gutter markers + inline underlines on the rendered
/// rows. `imbl::HashMap` keeps projection identity pointer-cheap.
#[derive(drv::Input, Copy, Clone)]
pub struct DiagnosticsStatesInput<'a> {
    pub by_path: &'a imbl::HashMap<CanonPath, BufferDiagnostics>,
}

impl<'a> DiagnosticsStatesInput<'a> {
    pub fn new(d: &'a DiagnosticsStates) -> Self {
        Self {
            by_path: &d.by_path,
        }
    }
}

/// Per-server LSP status projection. Lives on its own
/// projection (not bundled with diagnostics) so progress
/// churn doesn't invalidate the diagnostic-painter memo
/// cache.
#[derive(drv::Input, Copy, Clone)]
pub struct LspStatusesInput<'a> {
    pub by_server: &'a imbl::HashMap<String, LspServerStatus>,
}

impl<'a> LspStatusesInput<'a> {
    pub fn new(s: &'a LspStatuses) -> Self {
        Self {
            by_server: &s.by_server,
        }
    }
}

// ── Input on CompletionsState ─────────────────────────────────────────

/// Session-only projection of [`led_state_completions::CompletionsState`].
/// The memo that renders the popup only cares about the active
/// `session`; `seq_gen` and the pending outboxes mutate every tick
/// and would invalidate the popup memo for no visible change.
#[derive(drv::Input, Copy, Clone)]
pub struct CompletionsSessionInput<'a> {
    pub session: &'a Option<led_state_completions::CompletionSession>,
}

impl<'a> CompletionsSessionInput<'a> {
    pub fn new(s: &'a led_state_completions::CompletionsState) -> Self {
        Self { session: &s.session }
    }
}

// ── Input on LspExtrasState (M18) ─────────────────────────────────────

/// Overlay-only projection of [`led_state_lsp::LspExtrasState`] — just
/// the chrome-relevant fields (rename overlay, later stages will
/// add code-actions + inlay hints here). Pending-request outboxes
/// intentionally aren't part of this input so every outgoing RPC
/// doesn't invalidate chrome memos.
#[derive(drv::Input, Copy, Clone)]
pub struct LspExtrasOverlayInput<'a> {
    pub rename: &'a Option<led_state_lsp::RenameState>,
    pub code_actions: &'a Option<led_state_lsp::CodeActionPickerState>,
}

impl<'a> LspExtrasOverlayInput<'a> {
    pub fn new(s: &'a led_state_lsp::LspExtrasState) -> Self {
        Self {
            rename: &s.rename,
            code_actions: &s.code_actions,
        }
    }
}

// ── Input on GitState (M19) ───────────────────────────────────────────

/// Projection of [`led_state_git::GitState`] for use from the
/// browser memo (file-level categories), gutter memo (per-buffer
/// line statuses), and status-bar memo (branch name).
///
/// Separate input so a branch-only change doesn't invalidate the
/// body-model memo's per-row hot path, and a line-status change
/// for one buffer doesn't invalidate the status-bar memo.
#[derive(drv::Input, Copy, Clone)]
pub struct GitStateInput<'a> {
    pub branch: &'a Option<String>,
    pub file_statuses:
        &'a imbl::HashMap<CanonPath, imbl::HashSet<led_core::IssueCategory>>,
    pub line_statuses:
        &'a imbl::HashMap<CanonPath, Arc<Vec<led_core::git::LineStatus>>>,
}

impl<'a> GitStateInput<'a> {
    pub fn new(g: &'a led_state_git::GitState) -> Self {
        Self {
            branch: &g.branch,
            file_statuses: &g.file_statuses,
            line_statuses: &g.line_statuses,
        }
    }
}

// ── Input on Terminal ──────────────────────────────────────────────────

/// Viewport dims only. A push to `Terminal.pending` is deliberately
/// outside this input so incoming events don't invalidate `render_frame`.
#[derive(drv::Input, Copy, Clone)]
pub struct TerminalDimsInput<'a> {
    pub dims: &'a Option<Dims>,
}

impl<'a> TerminalDimsInput<'a> {
    pub fn new(term: &'a Terminal) -> Self {
        Self { dims: &term.dims }
    }
}

// ── Input on ClipboardState ────────────────────────────────────────────

#[derive(drv::Input, Copy, Clone)]
pub struct ClipboardStateInput<'a> {
    pub pending_yank: &'a Option<TabId>,
    pub read_in_flight: &'a bool,
    pub pending_write: &'a Option<Arc<str>>,
}

impl<'a> ClipboardStateInput<'a> {
    pub fn new(c: &'a ClipboardState) -> Self {
        Self {
            pending_yank: &c.pending_yank,
            read_in_flight: &c.read_in_flight,
            pending_write: &c.pending_write,
        }
    }
}

// ── Input on AlertState ────────────────────────────────────────────────

/// Narrow projection — excludes `info_expires_at` since it changes
/// every 10ms and would thrash the status-bar memo cache. The expiry
/// is the runtime's concern, not the painter's.
#[derive(drv::Input, Copy, Clone)]
pub struct AlertsInput<'a> {
    pub info: &'a Option<String>,
    pub warns: &'a Vec<(String, String)>,
    pub confirm_kill: &'a Option<TabId>,
}

impl<'a> AlertsInput<'a> {
    pub fn new(a: &'a AlertState) -> Self {
        Self {
            info: &a.info,
            warns: &a.warns,
            confirm_kill: &a.confirm_kill,
        }
    }
}

// ── Input on BrowserUi ──────────────────────────────────────────────

/// External-fact projection for [`FsTree`]. Written by the FS driver;
/// consumed by `file_list_action` and (indirectly) `side_panel_model`.
#[derive(drv::Input, Copy, Clone)]
pub struct FsTreeInput<'a> {
    pub root: &'a Option<CanonPath>,
    pub dir_contents: &'a imbl::HashMap<CanonPath, imbl::Vector<led_state_browser::DirEntry>>,
}

impl<'a> FsTreeInput<'a> {
    pub fn new(fs: &'a led_state_browser::FsTree) -> Self {
        Self {
            root: &fs.root,
            dir_contents: &fs.dir_contents,
        }
    }
}

/// User-decision projection for [`BrowserUi`]. Mutated by dispatch.
/// Tree flattening + the resolved selection index are derived —
/// they live in `browser_entries` and `browser_selected_idx`
/// below, not on this struct.
#[derive(drv::Input, Copy, Clone)]
pub struct BrowserUiInput<'a> {
    pub expanded_dirs: &'a imbl::HashSet<CanonPath>,
    pub selected_path: &'a Option<CanonPath>,
    pub scroll_offset: &'a usize,
    pub visible: &'a bool,
    pub focus: &'a Focus,
}

impl<'a> BrowserUiInput<'a> {
    pub fn new(b: &'a BrowserUi) -> Self {
        Self {
            expanded_dirs: &b.expanded_dirs,
            selected_path: &b.selected_path,
            scroll_offset: &b.scroll_offset,
            visible: &b.visible,
            focus: &b.focus,
        }
    }
}

/// Projection for the find-file overlay. Still a named type so
/// the find-file-specific memo (`find_file_action`) can take just
/// this projection without dragging the whole overlay bundle.
#[derive(drv::Input, Copy, Clone)]
pub struct FindFileInput<'a> {
    pub overlay: &'a Option<led_state_find_file::FindFileState>,
}

impl<'a> FindFileInput<'a> {
    pub fn new(ff: &'a Option<led_state_find_file::FindFileState>) -> Self {
        Self { overlay: ff }
    }
}

/// Unified projection over every currently-defined overlay.
///
/// Render and status-bar memos read "whichever overlay is active"
/// — they shouldn't take one input per overlay kind, because the
/// count grows with every milestone (M17 adds LSP completion, M18
/// adds LSP hover, etc.). Instead every overlay gets a field here
/// and the memos destructure at their call site.
///
/// Keeping all overlays in one projection (rather than a nested
/// drv::input) is necessary because drv's input trait uses
/// pointer-FastEq, which doesn't compose through nested input
/// structs. Raw `&Option<...>` fields project fine.
#[derive(drv::Input, Copy, Clone)]
pub struct OverlaysInput<'a> {
    pub find_file: &'a Option<led_state_find_file::FindFileState>,
    pub isearch: &'a Option<led_state_isearch::IsearchState>,
    pub file_search: &'a Option<led_state_file_search::FileSearchState>,
}

impl<'a> OverlaysInput<'a> {
    pub fn new(
        find_file: &'a Option<led_state_find_file::FindFileState>,
        isearch: &'a Option<led_state_isearch::IsearchState>,
        file_search: &'a Option<led_state_file_search::FileSearchState>,
    ) -> Self {
        Self {
            find_file,
            isearch,
            file_search,
        }
    }
}

// ── Memos ──────────────────────────────────────────────────────────────

/// "What files need a load started?"
///
/// Diff between the paths open in tabs and the `BufferStore` map.
/// Absent → `Load`; `Pending | Ready | Error` → skip. Once a load is
/// in flight, the `Pending` entry prevents re-triggering.
///
/// Filters before cloning so we only allocate `CanonPath`s for paths
/// that actually need loading.
#[drv::memo(single)]
pub fn file_load_action<'a, 'b>(
    store: StoreLoadedInput<'a>,
    tabs: TabsOpenInput<'b>,
) -> imbl::Vector<LoadAction> {
    tabs.open
        .iter()
        .filter(|t| !store.loaded.contains_key(&t.path))
        .map(|t| LoadAction::Load(t.path.clone()))
        .collect()
}

/// "What saves should we dispatch now?"
///
/// Diffs the user's save requests (`pending_saves`) against the
/// edited buffers, emitting one `SaveAction` per path that is both
/// requested and dirty. Runtime sync-clears `pending_saves` for the
/// emitted paths before calling `FileWriteDriver::execute` — without
/// that clear the next tick's query would emit the same saves again.
///
/// Idle: `pending_saves` is empty → returns `Vec::new()` (no alloc).
/// Σ(saved_version) across all edited buffers. The runtime
/// compares this against `lsp_requested_state_sum` to decide
/// when to fire `LspCmd::RequestDiagnostics`. Pure derivation
/// of `BufferEdits.buffers` — the atom stores only the sum we
/// last emitted for.
///
/// **Only `saved_version` is summed**, not live `version`. This
/// gates diagnostic requests to save events + the first
/// `lsp_notified` tick (caught by the `Option<u64>` None → Some
/// transition in the caller). Live-edit pulls would make
/// rust-analyzer re-analyze on every keystroke, and the user
/// would see error squiggles pop in mid-typing for transient
/// parser failures that disappear once they finish the word.
/// Save-gated pulls match the "errors appear after save" UX the
/// user expects.
#[drv::memo(single)]
pub fn buffer_state_sum<'b>(buffers: EditedBuffersInput<'b>) -> u64 {
    buffers
        .buffers
        .values()
        .fold(0u64, |acc, eb| acc.wrapping_add(eb.saved_version))
}

#[drv::memo(single)]
pub fn file_save_action<'p, 'b>(
    pending: PendingSavesInput<'p>,
    buffers: EditedBuffersInput<'b>,
) -> Vec<SaveAction> {
    let mut out: Vec<SaveAction> = Vec::new();
    for path in pending.paths.iter() {
        let Some(eb) = buffers.buffers.get(path) else {
            continue;
        };
        // No dirty filter: the user explicitly asked to Save (or
        // SaveNoFormat); writing a byte-identical file is cheap
        // and matches legacy. SaveAll's gating happens in the
        // dispatch helper (`request_save_all`) which only adds
        // dirty paths to `pending_saves` in the first place.
        out.push(SaveAction::Save {
            path: path.clone(),
            rope: eb.rope.clone(),
            version: eb.version,
        });
    }
    // SaveAs commits. Unlike Save, SaveAs doesn't require the buffer
    // to be dirty — the user may want to snapshot a pristine buffer
    // to a new path.
    for (from, to) in pending.save_as.iter() {
        let Some(eb) = buffers.buffers.get(from) else {
            continue;
        };
        out.push(SaveAction::SaveAs {
            from: from.clone(),
            to: to.clone(),
            rope: eb.rope.clone(),
            version: eb.version,
        });
    }
    out
}

/// Tab-bar slice of the render frame.
///
/// Labels are wrapped in `Arc` so cache-hit clones of [`TabBarModel`]
/// (inside `Frame`, deep inside `render_frame`'s cache slot) are a
/// pointer copy.
///
/// Format per label: `<prefix><name>` where `<prefix>` is `●`
/// (filled circle) when the buffer is dirty, else a space. The painter
/// wraps each label in `" <label> "`, so the two cases render as
/// `"  foo.rs "` (clean) and `" ●foo.rs "` (dirty) — the `●`
/// replaces the second leading space, matching the legacy goldens.
#[drv::memo(single)]
pub fn tab_bar_model<'a, 'b>(
    tabs: TabsActiveInput<'a>,
    edits: EditedBuffersInput<'b>,
) -> TabBarModel {
    let labels: Vec<String> = tabs
        .open
        .iter()
        .map(|t| {
            let base = t
                .path
                .file_name()
                .map(|os| os.to_string_lossy().into_owned())
                .unwrap_or_else(|| t.path.display().to_string());
            let dirty = edits
                .buffers
                .get(&t.path)
                .map(|b| b.dirty())
                .unwrap_or(false);
            let mut s = String::with_capacity(base.len() + "\u{25cf}".len());
            if dirty {
                s.push('\u{25cf}'); // ●
            } else {
                s.push(' ');
            }
            s.push_str(&base);
            s
        })
        .collect();
    let active = tabs
        .active
        .and_then(|id| tabs.open.iter().position(|t| t.id == id));
    TabBarModel {
        labels: Arc::new(labels),
        active,
    }
}

// Gutter width reserved on the left of every body row. M9 renders
// two blank cols; future milestones fill col 0 with git marks and
// col 1 with diagnostic severity.
const GUTTER_WIDTH: usize = 2;

/// Trailing column never written to on the right edge of the
/// editor area. Held at `0` now that the painter is
/// cell-grid-diff-based (see `driver-terminal/native/src/{buffer,render}.rs`):
/// it never emits `Clear(UntilNewLine)`, so writing the last
/// column is safe and the soft-wrap `\` lives in the true last
/// col of the terminal. The constant survives as a nameable
/// knob in case we ever need to reserve a gap again (e.g. for a
/// scroll indicator column); keeping it at `0` matches legacy
/// emacs/led behaviour.
const TRAILING_RESERVED_COLS: usize = 0;

/// Body slice of the render frame.
///
/// Reads the active tab's cursor + scroll to produce the visible line
/// slice and a body-relative cursor position. Scroll is source state
/// on [`Tab`]; dispatch maintains the "keep cursor visible" invariant
/// so the cursor is normally inside the returned window.
///
/// Prefers [`BufferEdits`] (the user-edited view) over [`BufferStore`]
/// (the disk snapshot). In steady state — loaded + seeded — the
/// edits branch always wins; the store fallback covers the brief
/// window between a load completion and the runtime's next
/// BufferEdits seed, plus Pending / Error paths that never made it
/// to `Ready`.
/// Bundled input for [`body_model`] — drv 0.4 nested-inputs
/// shape. Reduces the memo signature from 7 positional args to
/// 1. Callers build one labelled struct literal; drv's
/// per-field equality walks into each projection normally.
#[derive(Copy, Clone, drv::Input)]
pub struct BodyInputs<'a> {
    pub edits: EditedBuffersInput<'a>,
    pub store: StoreLoadedInput<'a>,
    pub tabs: TabsActiveInput<'a>,
    pub overlays: OverlaysInput<'a>,
    pub syntax: SyntaxStatesInput<'a>,
    pub diagnostics: DiagnosticsStatesInput<'a>,
    pub git: GitStateInput<'a>,
    pub area: Rect,
}

#[drv::memo(single)]
pub fn body_model<'a>(inputs: BodyInputs<'a>) -> BodyModel {
    let BodyInputs {
        edits,
        store,
        tabs,
        overlays,
        syntax,
        diagnostics,
        git,
        area,
    } = inputs;
    let Some(id) = *tabs.active else {
        return BodyModel::Empty;
    };
    let Some(tab) = tabs.open.iter().find(|t| t.id == id) else {
        return BodyModel::Empty;
    };
    if let Some(eb) = edits.buffers.get(&tab.path) {
        let highlight = active_body_match(&overlays, &tab.path, tab.scroll, area, &eb.rope);
        let spans = rebased_line_spans(syntax, edits, tab.path.clone());
        // No-smear rule: diagnostics render only when their
        // stamped hash matches the buffer's current content.
        // Ingestion stamps at offer-time with the then-current
        // hash, so a mismatch means the user has edited since.
        let current_hash =
            led_core::EphemeralContentHash::of_rope(&eb.rope).persist();
        let diags = diagnostics
            .by_path
            .get(&tab.path)
            .filter(|bd| bd.hash == current_hash)
            .map(|bd| bd.diagnostics.as_slice());
        let line_statuses = git.line_statuses.get(&tab.path).map(|v| v.as_slice());
        return render_content(
            &eb.rope,
            tab.cursor,
            tab.scroll,
            area,
            highlight,
            spans.as_deref().map(|v: &Vec<TokenSpan>| v.as_slice()),
            diags,
            line_statuses,
        );
    }
    // No BufferEdits entry yet — the load hasn't been seeded
    // into the edit-view source. Fall back to what `BufferStore`
    // knows. On Pending / Error / absent we render a blank body
    // (tildes, no content), never an in-buffer placeholder or
    // error message — matches legacy's "empty buffer during the
    // brief load window" UX and keeps surface errors off the
    // user's editing canvas. M21 will surface genuine load
    // failures via the status-bar alert system instead.
    let empty_rope: Arc<Rope> = Arc::new(Rope::new());
    let rope_ref: &Rope = match store.loaded.get(&tab.path) {
        Some(LoadState::Ready(rope)) => rope.as_ref(),
        None | Some(LoadState::Pending) | Some(LoadState::Error(_)) => &empty_rope,
    };
    let highlight = active_body_match(&overlays, &tab.path, tab.scroll, area, rope_ref);
    let line_statuses = git.line_statuses.get(&tab.path).map(|v| v.as_slice());
    render_content(
        rope_ref,
        tab.cursor,
        tab.scroll,
        area,
        highlight,
        None,
        None,
        line_statuses,
    )
}

/// Apply any edits the user made between the parse and now onto
/// the token list so spans still line up with current rope
/// offsets. Memoised on `(SyntaxStatesInput, EditedBuffersInput,
/// path)` — cursor moves, scrolls, overlay changes and resize
/// all invalidate `body_model` but not this memo, so the
/// rebased token list is reused as long as the tokens, the
/// parse-anchor rope and the current rope haven't changed. The
/// output is `Arc`-wrapped so a cache hit is a pointer clone.
///
/// Returns `None` when there's no syntax state yet, no tokens,
/// or no buffer for `path` — caller interprets each as
/// "render plain".
#[drv::memo(single)]
pub fn rebased_line_spans<'s, 'b>(
    syntax: SyntaxStatesInput<'s>,
    edits: EditedBuffersInput<'b>,
    path: CanonPath,
) -> Option<Arc<Vec<TokenSpan>>> {
    let state = syntax.by_path.get(&path)?;
    let eb = edits.buffers.get(&path)?;
    if state.tokens.is_empty() {
        return None;
    }
    // Drv-pure rebase: derive the ops from the two rope
    // snapshots the parser saw vs. the current rope. No
    // history-index counter that could drift across undo/redo.
    let Some(prev_rope) = state.tree_rope.as_ref() else {
        return Some(state.tokens.clone());
    };
    if Arc::ptr_eq(prev_rope, &eb.rope) {
        return Some(state.tokens.clone());
    }
    let Some(diff) = led_state_syntax::RopeDiff::between(prev_rope, &eb.rope) else {
        return Some(state.tokens.clone());
    };
    // Append-past-last-token fast path: if the diff sits
    // entirely past the last token's end (typing trailing
    // whitespace, appending at EOF, editing the tail of the
    // buffer past the highlighted region), no token positions
    // move and the existing Arc<Vec<TokenSpan>> is still
    // correct. Skip the to_vec() and the Arc::new wrap.
    let last_token_end = state
        .tokens
        .last()
        .map(|t| t.char_end)
        .unwrap_or(0);
    if diff.char_start >= last_token_end {
        return Some(state.tokens.clone());
    }
    Some(Arc::new(led_state_syntax::rebase_tokens(
        &state.tokens,
        diff.rebase_ops(),
    )))
}

/// Resolve the file-search overlay's current hit into a visible-row
/// match highlight for the active tab. Returns `None` unless the
/// overlay is open, has a Result selection pointing at a loaded hit,
/// and the hit's path matches `active_path`. The result coords are
/// body-visible (post-scroll, post-gutter) so the painter consumes
/// them directly.
fn active_body_match(
    overlays: &OverlaysInput<'_>,
    active_path: &CanonPath,
    scroll: Scroll,
    area: Rect,
    rope: &Rope,
) -> Option<led_driver_terminal_core::BodyMatch> {
    use led_core::{SubLine, col_to_sub_line, sub_line_count};
    let state = overlays.file_search.as_ref()?;
    let led_state_file_search::FileSearchSelection::Result(i) = state.selection else {
        return None;
    };
    let hit = state.flat_hits.get(i)?;
    if &hit.path != active_path {
        return None;
    }
    let line = hit.line.saturating_sub(1);
    let body_rows = area.rows as usize;
    if body_rows == 0 || line < scroll.top {
        return None;
    }
    let cols = area.cols as usize;
    let content_cols = cols
        .saturating_sub(GUTTER_WIDTH)
        .saturating_sub(TRAILING_RESERVED_COLS);
    let match_char_len = chars_between(&hit.preview, hit.match_start, hit.match_end);
    let col_start_char = hit.col.saturating_sub(1);
    // Pin the match to the sub-line containing its starting col;
    // wrapped matches (that straddle a sub-line boundary) paint
    // only on their first sub-line — consistent with legacy,
    // which didn't split match highlights across visual rows.
    let hit_line_len = line_char_len_rope(rope, line);
    let (match_sub, col_within) =
        col_to_sub_line(col_start_char, hit_line_len, content_cols);
    // Walk sub-line counts to find the visible-row index for
    // (line, match_sub).
    let mut row: usize = 0;
    let mut ln = scroll.top;
    let mut sub_start = scroll.top_sub_line.0;
    while ln < line {
        let len = line_char_len_rope(rope, ln);
        let subs = sub_line_count(len, content_cols);
        let remaining = subs.saturating_sub(sub_start);
        row = row.saturating_add(remaining);
        ln += 1;
        sub_start = 0;
    }
    if match_sub.0 < sub_start {
        return None;
    }
    row = row.saturating_add(match_sub.0 - sub_start);
    if row >= body_rows {
        return None;
    }
    // Columns of the match *within this sub-line*, clamped to
    // the sub-line's content width.
    let within_end = col_within.saturating_add(match_char_len);
    let rel_start = col_within.min(content_cols);
    let rel_end = within_end.min(content_cols);
    if rel_end <= rel_start {
        return None;
    }
    let _ = SubLine(0); // keep import without warning in edge conditions
    Some(led_driver_terminal_core::BodyMatch {
        row: row as u16,
        col_start: (rel_start + GUTTER_WIDTH) as u16,
        col_end: (rel_end + GUTTER_WIDTH) as u16,
    })
}

#[allow(clippy::too_many_arguments)]
fn render_content(
    rope: &Rope,
    cursor: Cursor,
    scroll: Scroll,
    area: Rect,
    match_highlight: Option<led_driver_terminal_core::BodyMatch>,
    rebased_tokens: Option<&[TokenSpan]>,
    diagnostics: Option<&[Diagnostic]>,
    git_line_statuses: Option<&[led_core::git::LineStatus]>,
) -> BodyModel {
    use led_driver_terminal_core::BodyLine;
    use led_core::{SubLine, sub_line_count, sub_line_range};

    let body_rows = area.rows as usize;
    let line_count = rope.len_lines();
    let cols = area.cols as usize;
    let content_cols = cols
        .saturating_sub(GUTTER_WIDTH)
        .saturating_sub(TRAILING_RESERVED_COLS);

    let mut lines: Vec<BodyLine> = Vec::with_capacity(body_rows);
    let mut ln = scroll.top;
    let mut sub = scroll.top_sub_line;
    for _ in 0..body_rows {
        if ln >= line_count {
            lines.push(BodyLine {
                text: "~ ".to_string(),
                spans: Vec::new(),
                gutter_diagnostic: None,
                gutter_category: None,
                diagnostics: Vec::new(),
            });
            continue;
        }
        let line_char_start = rope.line_to_char(ln);
        let mut full_line = rope.line(ln).to_string();
        strip_trailing_newline(&mut full_line);
        let line_char_len = full_line.chars().count();
        let max_sub = sub_line_count(line_char_len, content_cols);
        // Clamp `sub` to a valid range; a previous width change
        // could have left `scroll.top_sub_line` past the end of
        // the current line. Render the first sub-line instead
        // of producing an empty row.
        if sub.0 >= max_sub {
            sub = SubLine(0);
        }
        let (col_start, col_end) = sub_line_range(sub, line_char_len, content_cols);
        let slice: String = full_line.chars().skip(col_start).take(col_end - col_start).collect();
        let sub_char_start = line_char_start + col_start;
        let is_continued = led_core::is_continued(sub, line_char_len, content_cols);
        let mut s = String::with_capacity(cols);
        s.push_str("  ");
        // Expand tabs to 4 spaces so the painter doesn't ship a
        // raw `\t` byte to vt100 (which would jump the cursor to
        // the next 8-col tab stop, leaving a one-cell gap and
        // shifting everything right). Matches legacy
        // `core/src/wrap.rs::expand_tabs` (also 4-space).
        for ch in slice.chars() {
            if ch == '\t' {
                s.push_str("    ");
            } else {
                s.push(ch);
            }
        }
        if is_continued {
            // Non-last sub-line: emit `<content><\>`. Wrap
            // geometry reserves exactly one trailing col for the
            // glyph (wrap_width = content_cols - 1), so `\` lands
            // at the editor area's last column, flush against the
            // terminal's right edge — no interior blank, no
            // trailing blank. Matches emacs's display.
            s.push('\\');
        }
        let spans = rebased_tokens
            .map(|tokens| {
                tokens_to_line_spans(
                    tokens,
                    sub_char_start,
                    col_end - col_start,
                    content_cols,
                )
            })
            .unwrap_or_default();
        let (gutter_diag, row_diagnostics) = diagnostics
            .map(|diags| diagnostics_for_sub_line(diags, ln, col_start, col_end, content_cols))
            .unwrap_or_default();
        // Merged gutter category (M19 D7): the highest-precedence
        // `IssueCategory` for the gutter bar (git / PR only). Only
        // paints on the first sub-line of a wrapped row — matches
        // legacy's "col 1 marker on chunk 0".
        let is_first_sub = sub == SubLine(0);
        let gutter_category = if is_first_sub {
            merged_gutter_category(git_line_statuses, ln)
        } else {
            None
        };
        lines.push(BodyLine {
            text: s,
            spans,
            gutter_diagnostic: gutter_diag,
            gutter_category,
            diagnostics: row_diagnostics,
        });
        // Advance to the next visible sub-line; cross into the
        // next logical line when we run past the current one's
        // sub-line count.
        sub = SubLine(sub.0 + 1);
        if sub.0 >= max_sub {
            ln += 1;
            sub = SubLine(0);
        }
    }

    BodyModel::Content {
        lines: Arc::new(lines),
        cursor: visible_cursor(cursor, scroll, area, rope, content_cols),
        match_highlight,
    }
}

/// Project the buffer-wide diagnostic list onto one rendered
/// sub-line: pick the highest-severity diagnostic whose range
/// intersects the logical line for the gutter mark (so every
/// sub-line of a wrapped line carries the dot), and emit an
/// underline clipped to the sub-line's `[sub_col_start, sub_col_end)`
/// range — diagnostics that fall outside the sub-line simply
/// don't appear on that row.
///
/// Severity ordering for the gutter: Error > Warning > Info > Hint.
fn diagnostics_for_sub_line(
    diags: &[Diagnostic],
    line_num: usize,
    sub_col_start: usize,
    sub_col_end: usize,
    content_cols: usize,
) -> (
    Option<DiagnosticSeverity>,
    Vec<led_driver_terminal_core::BodyDiagnostic>,
) {
    let mut gutter: Option<DiagnosticSeverity> = None;
    let mut out = Vec::new();
    for d in diags {
        if line_num < d.start_line || line_num > d.end_line {
            continue;
        }
        // Legacy filters Info / Hint out of both gutter dots and
        // inline underlines (display.rs:357-365 for gutter,
        // 506-508 for underlines). They're still available for
        // diagnostic counts + cursor popover, but don't paint
        // chrome — too noisy given how many info notes a typical
        // LSP emits.
        if !matches!(d.severity, DiagnosticSeverity::Error | DiagnosticSeverity::Warning) {
            continue;
        }
        // Gutter tracks "any Err/Warn on this logical line" — a
        // wrapped line shows a dot on every sub-line so the user
        // sees it no matter which part of the line they're on.
        gutter = Some(match gutter {
            Some(existing) => higher(existing, d.severity),
            None => d.severity,
        });
        // Diagnostic column range ON THIS LOGICAL LINE.
        let line_col_start = if line_num == d.start_line { d.start_col } else { 0 };
        let line_col_end = if line_num == d.end_line {
            d.end_col
        } else {
            sub_col_end // clamped to sub-line end; spans run off visually
        };
        // Clip against the sub-line's column range, then make it
        // relative to the sub-line's own col 0.
        let clip_start = line_col_start.max(sub_col_start);
        let clip_end = line_col_end.min(sub_col_end);
        if clip_end <= clip_start {
            continue;
        }
        let rel_start = clip_start - sub_col_start;
        let rel_end = clip_end - sub_col_start;
        let vis_start = rel_start.min(content_cols) + GUTTER_WIDTH;
        let vis_end = rel_end.min(content_cols) + GUTTER_WIDTH;
        if vis_end <= vis_start {
            continue;
        }
        out.push(led_driver_terminal_core::BodyDiagnostic {
            col_start: vis_start as u16,
            col_end: vis_end as u16,
            severity: d.severity,
        });
    }
    (gutter, out)
}

/// Pick the precedence-winning `IssueCategory` for the gutter
/// bar (col 0) at `row` from git / PR line statuses. LSP severity
/// is intentionally excluded — diagnostics get their own glyph in
/// gutter col 1 (the `●`), so painting the bar from LSP too would
/// double up the indicator. Mirrors legacy `display.rs:328` which
/// queries only `buffer_line_annotations` (git + PR diff/comment,
/// no LSP). The precedence ladder in `IssueCategory::precedence`
/// still includes LSP because other consumers (browser
/// `resolve_display`) tie-break across all categories.
fn merged_gutter_category(
    line_statuses: Option<&[led_core::git::LineStatus]>,
    row: usize,
) -> Option<led_core::IssueCategory> {
    line_statuses.and_then(|s| led_core::git::best_category_at(s, row))
}

fn higher(a: DiagnosticSeverity, b: DiagnosticSeverity) -> DiagnosticSeverity {
    use DiagnosticSeverity::*;
    fn rank(s: DiagnosticSeverity) -> u8 {
        match s {
            Error => 3,
            Warning => 2,
            Info => 1,
            Hint => 0,
        }
    }
    if rank(a) >= rank(b) { a } else { b }
}

/// Slice the buffer-wide token list into the subset that falls on a
/// single rendered row, translating char offsets into row-relative
/// column positions (gutter included, right-edge-clamped).
///
/// A span that crosses the row boundary is clipped to the row; a
/// span that ends past the truncation point is clipped to
/// `content_cols`. Tokens whose kind is `Default` are dropped
/// because emitting a span that styles nothing would force the
/// painter to reset unnecessarily.
fn tokens_to_line_spans(
    tokens: &[TokenSpan],
    line_char_start: usize,
    line_char_len: usize,
    content_cols: usize,
) -> Vec<led_driver_terminal_core::LineSpan> {
    let line_end = line_char_start + line_char_len;
    let mut out = Vec::new();
    // Binary-search the first span whose end > line_char_start to
    // skip the prefix that lives on earlier lines. Tokens are sorted
    // by (start, end) in the worker, so this stays O(log n + k).
    let start_ix = tokens.partition_point(|t| t.char_end <= line_char_start);
    for t in &tokens[start_ix..] {
        if t.char_start >= line_end {
            break;
        }
        if matches!(t.kind, TokenKind::Default) {
            continue;
        }
        let rel_start = t.char_start.saturating_sub(line_char_start);
        let rel_end = t.char_end.saturating_sub(line_char_start).min(line_char_len);
        let col_start = (rel_start.min(content_cols) + GUTTER_WIDTH) as u16;
        let col_end = (rel_end.min(content_cols) + GUTTER_WIDTH) as u16;
        if col_end <= col_start {
            continue;
        }
        out.push(led_driver_terminal_core::LineSpan {
            col_start,
            col_end,
            kind: t.kind,
        });
    }
    out
}

/// Count how many visible body rows sit between the scroll
/// anchor and the cursor's sub-line. Returns `None` when the
/// cursor is above the scroll anchor or past the body bottom.
///
/// Walks logical lines one at a time — on soft-wrap buffers each
/// logical line may contribute multiple visible rows. Cheap in
/// practice because `body_rows` is tiny (20-50) and the walk
/// short-circuits as soon as we pass the cursor's line.
fn visible_cursor(
    c: Cursor,
    s: Scroll,
    area: Rect,
    rope: &Rope,
    content_cols: usize,
) -> Option<(u16, u16)> {
    use led_core::{col_to_sub_line, sub_line_count};
    let body_rows = area.rows as usize;
    if body_rows == 0 || c.line < s.top {
        return None;
    }
    // Cursor's own sub-line + column within that sub-line.
    let cur_line_len = line_char_len_rope(rope, c.line);
    let (cur_sub, col_within) = col_to_sub_line(c.col, cur_line_len, content_cols);
    // Count visible rows from (s.top, s.top_sub_line) to (c.line, cur_sub).
    let mut row: usize = 0;
    let line_count = rope.len_lines();
    let mut ln = s.top;
    let mut sub_start = s.top_sub_line.0;
    while ln < c.line {
        if ln >= line_count {
            return None;
        }
        let len = line_char_len_rope(rope, ln);
        let subs = sub_line_count(len, content_cols);
        let remaining = subs.saturating_sub(sub_start);
        row = row.saturating_add(remaining);
        ln += 1;
        sub_start = 0;
    }
    // Same logical line: add the sub-line offset, clamped to 0
    // if scroll started past this cursor's sub-line (caller's
    // adjust_scroll should prevent that).
    if cur_sub.0 < sub_start {
        return None;
    }
    row = row.saturating_add(cur_sub.0 - sub_start);
    if row >= body_rows {
        return None;
    }
    let max_col = (area.cols as usize).saturating_sub(1);
    let col = (col_within + GUTTER_WIDTH).min(max_col) as u16;
    Some((row as u16, col))
}

fn line_char_len_rope(rope: &Rope, line: usize) -> usize {
    let line_count = rope.len_lines();
    if line >= line_count {
        return 0;
    }
    let mut s = rope.line(line).to_string();
    strip_trailing_newline(&mut s);
    s.chars().count()
}

fn strip_trailing_newline(s: &mut String) {
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
}

/// Status-bar slice of the render frame.
///
/// Priority chain (highest wins):
/// 1. **Confirm-kill prompt** — blocks other content; no position
///    indicator. Matches legacy dismiss-on-first-keystroke UX.
/// 2. **Info alert** — transient; shown alongside the position.
/// 3. **Warn alert** — persistent; shown alongside the position with
///    the `is_warn` flag set (painter renders white-on-red-bold).
/// 4. **Default** — left is `"  ●"` when the active buffer is dirty
///    else empty; right is `"L<row>:C<col> "` (1-indexed human
///    coords, trailing space).
///
/// All strings are `Arc<str>` so cache-hit clones of
/// [`StatusBarModel`] are a pointer copy.
///
/// Bundled input — drv 0.4 nested-inputs shape. Reduces the
/// memo signature from 8 positional args to 1.
#[derive(Copy, Clone, drv::Input)]
pub struct StatusBarInputs<'a> {
    pub alerts: AlertsInput<'a>,
    pub tabs: TabsActiveInput<'a>,
    pub edits: EditedBuffersInput<'a>,
    pub overlays: OverlaysInput<'a>,
    pub diagnostics: DiagnosticsStatesInput<'a>,
    pub lsp: LspStatusesInput<'a>,
    pub lsp_extras: LspExtrasOverlayInput<'a>,
    pub git: GitStateInput<'a>,
    pub render_tick: u64,
}

#[drv::memo(single)]
pub fn status_bar_model<'a>(inputs: StatusBarInputs<'a>) -> StatusBarModel {
    let StatusBarInputs {
        alerts,
        tabs,
        edits,
        overlays,
        diagnostics,
        lsp,
        lsp_extras,
        git,
        render_tick,
    } = inputs;
    // The rename overlay used to take the status-bar prompt slot
    // here; legacy renders it as an in-buffer popup anchored on
    // the row below the cursor instead. See `rename_popup_model`.
    let _ = lsp_extras;
    // Priority 0b — in-buffer isearch prompt. Matches legacy
    // `display.rs` "Failing search:" / "Search:" wording so the
    // failed-state and live-state share a single prefix slot.
    if let Some(state) = overlays.isearch.as_ref() {
        let hint_len = state.query.hint.as_ref().map(|h| h.len() + 1).unwrap_or(0);
        let mut left = String::with_capacity(state.query.text.len() + 18 + hint_len);
        if state.failed {
            left.push_str(" Failing search: ");
        } else {
            left.push_str(" Search: ");
        }
        left.push_str(&state.query.text);
        if let Some(hint) = state.query.hint.as_ref() {
            left.push(' ');
            left.push_str(hint);
        }
        return StatusBarModel {
            left: Arc::from(left),
            right: Arc::from(""),
            is_warn: false,
        };
    }

    // Priority 0 — find-file overlay prompt. Replaces the whole
    // status bar content: left is `Find file: <input>` /
    // `Save as: <input>`, right is empty (no position indicator
    // while the overlay has focus). Matches legacy goldens.
    //
    // An active `hint` (e.g. "[No match]") appends after one space
    // of padding — Emacs-style transient feedback.
    if let Some(state) = overlays.find_file.as_ref() {
        let label = match state.mode {
            led_state_find_file::FindFileMode::Open => "Find file",
            led_state_find_file::FindFileMode::SaveAs => "Save as",
        };
        let hint_len = state.input.hint.as_ref().map(|h| h.len() + 1).unwrap_or(0);
        let mut left = String::with_capacity(state.input.text.len() + label.len() + 3 + hint_len);
        left.push(' ');
        left.push_str(label);
        left.push_str(": ");
        left.push_str(&state.input.text);
        if let Some(hint) = state.input.hint.as_ref() {
            left.push(' ');
            left.push_str(hint);
        }
        return StatusBarModel {
            left: Arc::from(left),
            right: Arc::from(""),
            is_warn: false,
        };
    }

    // Priority 1 — confirm-kill prompt.
    if let Some(kill_id) = *alerts.confirm_kill {
        let name = tabs
            .open
            .iter()
            .find(|t| t.id == kill_id)
            .and_then(|t| t.path.file_name().map(|os| os.to_string_lossy().into_owned()))
            .unwrap_or_default();
        return StatusBarModel {
            left: Arc::from(format!(" Kill buffer '{name}'? (y/N) ")),
            right: Arc::from(""),
            is_warn: false,
        };
    }

    let right = position_string(tabs, edits, diagnostics);

    // Priority 2 — info alert.
    if let Some(msg) = alerts.info.as_deref() {
        let mut left = String::with_capacity(msg.len() + 1);
        left.push(' ');
        left.push_str(msg);
        return StatusBarModel {
            left: Arc::from(left),
            right,
            is_warn: false,
        };
    }

    // Priority 3 — warn alert (first-arrived).
    if let Some((_, msg)) = alerts.warns.first() {
        let mut left = String::with_capacity(msg.len() + 1);
        left.push(' ');
        left.push_str(msg);
        return StatusBarModel {
            left: Arc::from(left),
            right,
            is_warn: true,
        };
    }

    // Priority 4 — default left half: ` {branch}{modified}{lsp}`.
    // Legacy composes this as ` {branch}{modified}{pr}{lsp}`; PR
    // lands at M27. `lsp_progress_message` always returns `Some`
    // once a server is registered, so "rust-analyzer" stays
    // visible both during indexing and idle. The branch segment
    // is empty when `git.branch` is `None` (detached HEAD or
    // non-repo workspace) so the bar collapses back to the
    // pre-M19 shape automatically.
    let dirty = active_is_dirty(tabs, edits);
    let modified = if dirty { " \u{25cf}" } else { "" };
    let branch = git.branch.as_deref().unwrap_or("");
    let branch_segment = if branch.is_empty() {
        String::new()
    } else {
        format!(" {branch}")
    };
    let lsp_str = lsp_progress_message(lsp, render_tick).unwrap_or_default();
    let left: Arc<str> = Arc::from(format!(" {branch_segment}{modified}{lsp_str}"));
    StatusBarModel {
        left,
        right,
        is_warn: false,
    }
}

fn active_tab<'t>(tabs: TabsActiveInput<'t>) -> Option<&'t Tab> {
    let id = (*tabs.active)?;
    tabs.open.iter().find(|t| t.id == id)
}

fn active_is_dirty(tabs: TabsActiveInput<'_>, edits: EditedBuffersInput<'_>) -> bool {
    let Some(tab) = active_tab(tabs) else {
        return false;
    };
    edits
        .buffers
        .get(&tab.path)
        .map(|eb| eb.dirty())
        .unwrap_or(false)
}

/// Format one LSP server's status line matching legacy's
/// `format_lsp_status` (`/crates/ui/src/display.rs:803` on
/// main). Shape:
///
/// - Busy, no detail:    `  ⠋ rust-analyzer`
/// - Busy, with detail:  `  ⠋ rust-analyzer  ⠹ indexing crates`
/// - Idle, with detail:  `  rust-analyzer  indexing crates`
/// - Idle, no detail:    `  rust-analyzer`
/// - Empty server name:  `""` (no row)
///
/// `render_tick` is the current time in 80ms buckets so the
/// spinner animates across frames: each 80ms bucket advances
/// one frame in a 10-frame braille cycle. Two spinners are used
/// when detail is present, with an offset between them so they
/// animate out of phase (matches legacy's 400-bucket offset).
fn format_lsp_status(server_name: &str, busy: bool, detail: Option<&str>, render_tick: u64) -> String {
    if server_name.is_empty() {
        return String::new();
    }
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let spinner_char = |offset: u64| -> char {
        FRAMES[((render_tick + offset) as usize) % FRAMES.len()]
    };
    let spinner = if busy {
        format!("{} ", spinner_char(0))
    } else {
        String::new()
    };
    let detail_str = detail
        .filter(|d| !d.is_empty())
        .map(|d| {
            if busy {
                // Legacy offsets by 400ms-worth (= 5 buckets) so
                // the two spinners animate staggered.
                format!("  {} {d}", spinner_char(5))
            } else {
                format!("  {d}")
            }
        })
        .unwrap_or_default();
    format!("  {spinner}{server_name}{detail_str}")
}

/// Rendered LSP status line for the status bar's left half.
/// Returns `None` only when no server is registered yet;
/// otherwise a server is picked and shown persistently — busy
/// with detail, busy alone, idle with detail, or just the
/// server name when idle with no detail. Legacy does the same:
/// once rust-analyzer is up, its name stays visible in the
/// status bar, spinner and detail come and go around it.
///
/// Selection: prefer a busy server; else a server that has a
/// non-empty detail; else just pick one (iteration order is
/// fine — typically there's only one).
fn lsp_progress_message(lsp: LspStatusesInput<'_>, render_tick: u64) -> Option<String> {
    let (server, status) = lsp
        .by_server
        .iter()
        .find(|(_, s)| s.busy)
        .or_else(|| {
            lsp.by_server
                .iter()
                .find(|(_, s)| s.detail.as_deref().is_some_and(|d| !d.is_empty()))
        })
        .or_else(|| lsp.by_server.iter().next())
        .map(|(name, s)| (name.clone(), s.clone()))?;
    let formatted =
        format_lsp_status(&server, status.busy, status.detail.as_deref(), render_tick);
    if formatted.is_empty() {
        None
    } else {
        Some(formatted)
    }
}

fn position_string(
    tabs: TabsActiveInput<'_>,
    _edits: EditedBuffersInput<'_>,
    _diagnostics: DiagnosticsStatesInput<'_>,
) -> Arc<str> {
    // 1-indexed for human display — matches legacy goldens.
    // Falls back to `L1:C1` when no tab is active so post-kill /
    // empty-workspace status bars still anchor a position
    // string (legacy `display.rs` uses `s.cursor_row/col` which
    // default to zero in the same case).
    let (row, col) = active_tab(tabs)
        .map(|t| (t.cursor.line + 1, t.cursor.col + 1))
        .unwrap_or((1, 1));
    Arc::from(format!("L{row}:C{col} "))
}

/// Side-panel slice of the render frame. Walks the visible window
/// of `browser.entries` and produces one `SidePanelRow` per row.
/// Empty when the browser has no entries.
///
/// Overlay priority (highest first):
/// - File-search active → render its header (toggle row + query
///   input + optional replace input + results tree).
/// - Find-file overlay active with `show_side=true` → render the
///   completions list.
/// - Otherwise → render the file-browser tree.
///
/// Bundled input — drv 0.4 nested-inputs shape.
#[derive(Copy, Clone, drv::Input)]
pub struct SidePanelInputs<'a> {
    pub fs: FsTreeInput<'a>,
    pub browser: BrowserUiInput<'a>,
    pub overlays: OverlaysInput<'a>,
    pub tabs: TabsActiveInput<'a>,
    pub diagnostics: DiagnosticsStatesInput<'a>,
    pub git: GitStateInput<'a>,
    pub edits: EditedBuffersInput<'a>,
    pub rows: u16,
}

#[drv::memo(single)]
pub fn side_panel_model<'a>(inputs: SidePanelInputs<'a>) -> SidePanelModel {
    let SidePanelInputs {
        fs,
        browser,
        overlays,
        tabs,
        diagnostics,
        git,
        edits,
        rows,
    } = inputs;
    if let Some(state) = overlays.file_search.as_ref() {
        return file_search_side_panel(state, rows);
    }
    if let Some(state) = overlays.find_file.as_ref()
        && state.show_side
    {
        return completions_side_panel(state, rows);
    }
    let entries = browser_entries(BrowserDerivedInputs {
        fs,
        ui: browser,
        tabs,
        edits,
    });
    let selected = browser_selected_idx(&entries, browser.selected_path.as_ref());
    let rows = rows as usize;
    let start = *browser.scroll_offset;
    let end = start.saturating_add(rows).min(entries.len());
    let focused = *browser.focus == Focus::Side;
    // Per-file category map — used for both file rows (direct
    // lookup) and directory rows (union over descendants).
    let categories = file_categories_map(diagnostics, git);
    let mut out: Vec<SidePanelRow> = Vec::with_capacity(end.saturating_sub(start));
    for (i, entry) in entries[start..end].iter().enumerate() {
        let chevron = match entry.kind {
            TreeEntryKind::File => None,
            TreeEntryKind::Directory { expanded } => Some(expanded),
        };
        // Resolve category per legacy:
        //  - Files look up their own categories.
        //  - Directories aggregate child categories via
        //    `directory_categories`, then always render as a
        //    bullet (letter forced regardless of resolver).
        let status = match entry.kind {
            TreeEntryKind::File => categories
                .get(&entry.path)
                .and_then(|cats| led_core::resolve_display(cats))
                .map(|d| led_driver_terminal_core::RowStatus {
                    category: d.category,
                    letter: d.letter,
                }),
            TreeEntryKind::Directory { .. } => {
                let cats = led_core::directory_categories(&categories, &entry.path);
                led_core::resolve_display(&cats).map(|d| {
                    led_driver_terminal_core::RowStatus {
                        category: d.category,
                        // Directories always bullet — matches legacy
                        // display.rs:1396-1402.
                        letter: '\u{2022}',
                    }
                })
            }
        };
        out.push(SidePanelRow {
            depth: entry.depth as u16,
            chevron,
            name: Arc::<str>::from(entry.name.as_str()),
            selected: start + i == selected,
            match_range: None,
            replaced: false,
            status,
        });
    }
    SidePanelModel {
        rows: Arc::new(out),
        focused,
        mode: led_driver_terminal_core::SidePanelMode::Browser,
    }
}

/// Build a side-panel model from the find-file completions list.
/// Selection highlights the arrow-selected row; `focused` is always
/// `false` because the side panel never "has focus" in overlay mode
/// — keystrokes go through the overlay's own handler, and the
/// painter uses the flag to distinguish focused vs unfocused
/// selection styling (M14b chrome theming).
fn completions_side_panel(
    state: &led_state_find_file::FindFileState,
    rows: u16,
) -> SidePanelModel {
    let rows = rows as usize;
    let end = state.completions.len().min(rows);
    let mut out: Vec<SidePanelRow> = Vec::with_capacity(end);
    for (i, entry) in state.completions[..end].iter().enumerate() {
        out.push(SidePanelRow {
            depth: 0,
            chevron: None,
            name: Arc::<str>::from(entry.name.as_str()),
            selected: state.selected == Some(i),
            match_range: None,
            replaced: false,
            status: None,
        });
    }
    SidePanelModel {
        rows: Arc::new(out),
        focused: false,
        mode: led_driver_terminal_core::SidePanelMode::Completions,
    }
}

/// Build a side-panel model from the file-search overlay.
///
/// Layout:
/// - Row 0: toggle header " Aa   .*   =>" — the three toggles for
///   case-sensitive, regex, replace-mode. Later stages will style
///   active toggles distinctly (reverse video); for now the
///   characters appear regardless.
/// - Row 1: query input row.
/// - Row 2: replace input row — only when `replace_mode`.
/// - Rows 3+: results tree — one row per file group header, then
///   one row per hit formatted `"   <line>: <preview>"` (3-space
///   indent matching legacy). The tree scrolls to follow the
///   selection when the user arrows past the bottom edge; inputs
///   stay pinned on the first 1–2 rows.
///
/// `focused=false` because M14b chrome theming hasn't picked a
/// focused side-panel style for this overlay yet.
fn file_search_side_panel(
    state: &led_state_file_search::FileSearchState,
    rows: u16,
) -> SidePanelModel {
    let total = rows as usize;
    let mut out: Vec<SidePanelRow> = Vec::new();
    let mode = led_driver_terminal_core::SidePanelMode::FileSearch {
        case_sensitive: state.case_sensitive,
        use_regex: state.use_regex,
        replace_mode: state.replace_mode,
    };

    if total == 0 {
        return SidePanelModel {
            rows: Arc::new(out),
            focused: false,
            mode,
        };
    }

    out.push(SidePanelRow {
        depth: 0,
        chevron: None,
        name: Arc::<str>::from(" Aa   .*   =>"),
        selected: false,
        match_range: None,
        replaced: false,
            status: None,
    });

    if total > out.len() {
        out.push(SidePanelRow {
            depth: 0,
            chevron: None,
            name: Arc::<str>::from(state.query.text.as_str()),
            selected: matches!(
                state.selection,
                led_state_file_search::FileSearchSelection::SearchInput
            ),
            match_range: None,
            replaced: false,
            status: None,
        });
    }
    if state.replace_mode && total > out.len() {
        out.push(SidePanelRow {
            depth: 0,
            chevron: None,
            name: Arc::<str>::from(state.replace.text.as_str()),
            selected: matches!(
                state.selection,
                led_state_file_search::FileSearchSelection::ReplaceInput
            ),
            match_range: None,
            replaced: false,
            status: None,
        });
    }

    // Selected flat-hit index (if the cursor is on a result row).
    let selected_hit_idx = match state.selection {
        led_state_file_search::FileSearchSelection::Result(i) => Some(i),
        _ => None,
    };

    // Rows remaining for the results tree after the pinned inputs.
    let tree_rows_avail = total.saturating_sub(out.len());
    if tree_rows_avail == 0 {
        return SidePanelModel {
            rows: Arc::new(out),
            focused: false,
            mode,
        };
    }

    // `scroll_offset` is maintained by dispatch's move_selection —
    // it already points at the correct top-of-tree row for the
    // current selection, so the renderer doesn't re-derive.
    let effective_scroll = state.scroll_offset;

    // Flatten results: one row per group header + one row per hit.
    let mut skipped = 0usize;
    let mut hit_idx: usize = 0;
    'outer: for group in state.results.iter() {
        // Group header row.
        if skipped < effective_scroll {
            skipped += 1;
        } else {
            if total <= out.len() {
                break 'outer;
            }
            out.push(SidePanelRow {
                depth: 0,
                chevron: None,
                name: Arc::<str>::from(group.relative.as_str()),
                selected: false,
                match_range: None,
                replaced: false,
            status: None,
            });
        }
        for hit in &group.hits {
            if skipped < effective_scroll {
                skipped += 1;
            } else {
                if total <= out.len() {
                    break 'outer;
                }
                let is_replaced = state
                    .hit_replacements
                    .get(hit_idx)
                    .and_then(|e| e.as_ref())
                    .is_some();
                let prefix_chars = 3 + count_chars_of_usize(hit.line) + 2;
                // Side panel content area is 24 cols (see Layout in
                // driver-terminal/core); the prefix eats `prefix_chars`,
                // the rest is what the preview can fill before the
                // border. Trim only when the raw preview wouldn't fit.
                let preview_budget = 24usize.saturating_sub(prefix_chars);
                let (preview, match_preview_idx) = trimmed_preview(hit, preview_budget);
                let match_len = chars_between(&hit.preview, hit.match_start, hit.match_end);
                let match_start = (prefix_chars + match_preview_idx) as u16;
                let match_end = match_start.saturating_add(match_len as u16);
                let name = format!("   {}: {}", hit.line, preview);
                out.push(SidePanelRow {
                    depth: 0,
                    chevron: None,
                    name: Arc::<str>::from(name.as_str()),
                    selected: selected_hit_idx == Some(hit_idx),
                    // Suppress the match highlight on replaced rows
                    // — the dim replaced style reads better without
                    // the yellow/bold overlay competing.
                    match_range: if is_replaced {
                        None
                    } else {
                        Some((match_start, match_end))
                    },
                    replaced: is_replaced,
                    status: None,
                });
            }
            hit_idx += 1;
        }
    }

    SidePanelModel {
        rows: Arc::new(out),
        focused: false,
        mode,
    }
}

// ── Popover (diagnostic hover) ───────────────────────────────────────

/// Max content width inside the popover box (excluding the 1-col
/// padding on each side). Matches legacy's ceiling so the wrap
/// looks identical in golden traces.
const POPOVER_MAX_CONTENT: usize = 58;

/// Build the cursor-line diagnostic popover.
///
/// Returns `None` (no popover) when any of:
/// - An overlay has input focus (find-file / file-search / isearch).
/// - The browser is focused.
/// - No active tab, or the active buffer isn't loaded yet.
/// - `DiagnosticsStates` has nothing for the active path.
/// - The stamped content hash doesn't match the buffer's current
///   content (no-smear: hide rather than show stale).
/// - No Error/Warning diagnostic covers the cursor row
///   (Info/Hint are silent, matching legacy).
pub fn popover_model(
    edits: EditedBuffersInput<'_>,
    tabs: TabsActiveInput<'_>,
    overlays: OverlaysInput<'_>,
    browser: BrowserUiInput<'_>,
    diagnostics: DiagnosticsStatesInput<'_>,
    editor_area: Rect,
) -> Option<PopoverModel> {
    if overlays.find_file.is_some()
        || overlays.file_search.is_some()
        || overlays.isearch.is_some()
    {
        return None;
    }
    if *browser.focus == Focus::Side {
        return None;
    }
    let id = (*tabs.active)?;
    let tab = tabs.open.iter().find(|t| t.id == id)?;
    let eb = edits.buffers.get(&tab.path)?;
    let bd = diagnostics.by_path.get(&tab.path)?;
    let current_hash = led_core::EphemeralContentHash::of_rope(&eb.rope).persist();
    if bd.hash != current_hash {
        return None;
    }
    let cursor_row = tab.cursor.line;
    let mut hits: Vec<&Diagnostic> = bd
        .diagnostics
        .iter()
        .filter(|d| {
            cursor_row >= d.start_line
                && cursor_row <= d.end_line
                && matches!(
                    d.severity,
                    DiagnosticSeverity::Error | DiagnosticSeverity::Warning
                )
        })
        .collect();
    if hits.is_empty() {
        return None;
    }
    // Stable order: error before warning, then by start position.
    hits.sort_by_key(|d| (severity_rank(d.severity), d.start_line, d.start_col));
    // Dedupe by `(severity, message)`. Parsers (especially
    // rust-analyzer) cascade identical "expected X" errors across
    // several positions on the same line — showing the same
    // sentence three times in a vertical stack is noise.
    hits.dedup_by(|a, b| a.severity == b.severity && a.message == b.message);

    let max_content = POPOVER_MAX_CONTENT.min(editor_area.rows.saturating_sub(0) as usize);
    // Cap width by editor area so the box never exceeds the edit
    // region minus a 2-col margin (padding inside the box).
    let max_content = max_content.min(
        editor_area
            .cols
            .saturating_sub(4)
            .max(1) as usize,
    );

    let mut lines: Vec<PopoverLine> = Vec::new();
    for (i, d) in hits.iter().enumerate() {
        if i > 0 {
            lines.push(PopoverLine {
                text: Arc::<str>::from(""),
                severity: None,
            });
        }
        let severity = Some(match d.severity {
            DiagnosticSeverity::Error => PopoverSeverity::Error,
            DiagnosticSeverity::Warning => PopoverSeverity::Warning,
            DiagnosticSeverity::Info => PopoverSeverity::Info,
            DiagnosticSeverity::Hint => PopoverSeverity::Hint,
        });
        for wrapped in word_wrap(&d.message, max_content) {
            lines.push(PopoverLine {
                text: Arc::<str>::from(wrapped.as_str()),
                severity,
            });
        }
    }

    // Anchor in absolute terminal coords: cursor position inside
    // the editor area. Mirrors `visible_cursor` so the popover
    // sits exactly over the cursor cell, gutter offset included
    // and sub-line column for soft-wrapped lines.
    let scroll_row = tab.scroll.top;
    if cursor_row < scroll_row {
        return None;
    }
    let row_in_area = (cursor_row - scroll_row) as u16;
    if row_in_area >= editor_area.rows {
        return None;
    }
    use led_core::col_to_sub_line;
    let content_cols = (editor_area.cols as usize)
        .saturating_sub(GUTTER_WIDTH)
        .saturating_sub(TRAILING_RESERVED_COLS);
    let cur_line_len = line_char_len_rope(&eb.rope, cursor_row);
    let (_, col_within) = col_to_sub_line(tab.cursor.col, cur_line_len, content_cols);
    let anchor_x = editor_area
        .x
        .saturating_add(GUTTER_WIDTH as u16)
        .saturating_add(col_within as u16);
    let anchor_y = editor_area.y.saturating_add(row_in_area);

    Some(PopoverModel {
        lines: Arc::new(lines),
        anchor: (anchor_x, anchor_y),
    })
}

/// Maximum rows the completion popup displays at once. Matches
/// legacy's fixed window — users scroll the list with
/// Up/Down beyond this.
const COMPLETION_MAX_ROWS: usize = 10;

/// "What should the completion popup look like right now?"
///
/// Builds the visible window (`scroll..scroll + COMPLETION_MAX_ROWS`)
/// from the active session's filtered items, computes the label /
/// detail column widths the painter needs, and anchors the popup
/// at the cursor's terminal position.
///
/// Returns `None` when no session is active, the session's tab
/// isn't the current active tab (user navigated away), or the
/// filtered list is empty (the dispatch-side dismiss should have
/// caught this, but guard anyway so a stale frame doesn't paint
/// an empty box).
#[drv::memo(single)]
pub fn completion_popup_model<'c, 't>(
    completions: CompletionsSessionInput<'c>,
    tabs: TabsActiveInput<'t>,
    editor_area: Rect,
) -> Option<led_driver_terminal_core::CompletionPopupModel> {
    use led_driver_terminal_core::{CompletionPopupModel, CompletionRow};
    let session = completions.session.as_ref()?;
    let active = (*tabs.active)?;
    if session.tab != active {
        return None;
    }
    if session.filtered.is_empty() {
        return None;
    }
    // Visible window — scroll..scroll + MAX, clamped to the
    // filtered length. `selected` has already been scroll-
    // adjusted by the overlay dispatch (ensure_visible); the
    // memo just paints what it sees.
    let total = session.filtered.len();
    let scroll = session.scroll.min(total.saturating_sub(1));
    let end = (scroll + COMPLETION_MAX_ROWS).min(total);
    let mut rows: Vec<CompletionRow> = Vec::with_capacity(end - scroll);
    let mut label_width: usize = 0;
    let mut detail_width: usize = 0;
    for &item_ix in &session.filtered[scroll..end] {
        let item = &session.items[item_ix];
        let label_cols = item.label.chars().count();
        label_width = label_width.max(label_cols);
        if let Some(d) = item.detail.as_ref() {
            detail_width = detail_width.max(d.chars().count());
        }
        rows.push(CompletionRow {
            label: item.label.clone(),
            detail: item.detail.clone(),
        });
    }
    // Anchor at cursor's terminal position. Painter flips above
    // or below based on remaining rows below the cursor.
    let tab = tabs.open.iter().find(|t| t.id == session.tab)?;
    let cursor_col = tab.cursor.col as u16;
    let cursor_row = tab.cursor.line as u16;
    let anchor = (
        editor_area.x.saturating_add(GUTTER_WIDTH as u16).saturating_add(cursor_col),
        editor_area.y.saturating_add(cursor_row),
    );
    let selected_in_window = session.selected.saturating_sub(scroll);
    Some(CompletionPopupModel {
        rows: Arc::new(rows),
        selected: selected_in_window,
        anchor,
        label_width: label_width.min(u16::MAX as usize) as u16,
        detail_width: detail_width.min(u16::MAX as usize) as u16,
    })
}

/// Build the code-action picker popup. Reuses `CompletionPopupModel`
/// because the painter for completion popups is the right
/// visual shape (list of titles) and we don't want two popup
/// paint paths.
///
/// Only the title is surfaced — legacy hides `kind` from the
/// picker (display.rs:972-983), so we follow suit and leave
/// `detail` empty.
pub fn code_action_popup_model<'e, 't>(
    lsp_extras: LspExtrasOverlayInput<'e>,
    tabs: TabsActiveInput<'t>,
    editor_area: Rect,
) -> Option<led_driver_terminal_core::CompletionPopupModel> {
    use led_driver_terminal_core::{CompletionPopupModel, CompletionRow};
    let picker = lsp_extras.code_actions.as_ref()?;
    if picker.items.is_empty() {
        return None;
    }
    let total = picker.items.len();
    let scroll = picker.scroll.min(total.saturating_sub(1));
    let end = (scroll + COMPLETION_MAX_ROWS).min(total);
    let mut rows: Vec<CompletionRow> = Vec::with_capacity(end - scroll);
    let mut label_width: usize = 0;
    let detail_width: usize = 0;
    for item in &picker.items[scroll..end] {
        let label_cols = item.title.chars().count();
        label_width = label_width.max(label_cols);
        rows.push(CompletionRow {
            label: item.title.clone(),
            detail: None,
        });
    }
    // Anchor at the active tab's cursor. The picker is a
    // transient modal — rendering it where completions render
    // is the most natural place.
    let active = (*tabs.active)?;
    let tab = tabs.open.iter().find(|t| t.id == active)?;
    let cursor_col = tab.cursor.col as u16;
    let cursor_row = tab.cursor.line as u16;
    let anchor = (
        editor_area.x.saturating_add(GUTTER_WIDTH as u16).saturating_add(cursor_col),
        editor_area.y.saturating_add(cursor_row),
    );
    let selected_in_window = picker.selected.saturating_sub(scroll);
    Some(CompletionPopupModel {
        rows: Arc::new(rows),
        selected: selected_in_window,
        anchor,
        label_width: label_width.min(u16::MAX as usize) as u16,
        detail_width: detail_width.min(u16::MAX as usize) as u16,
    })
}

/// Build the LSP rename overlay's in-buffer popup. Mirrors
/// legacy `OverlayContent::Rename`: anchor at one row below the
/// cursor (or the cursor row when there is no row below), at
/// the cursor's screen column. Width is sized to fit
/// `" Rename: <input> "` with a 2-col padding tail so the box
/// reads cleanly even with short input.
pub fn rename_popup_model(
    lsp_extras: LspExtrasOverlayInput<'_>,
    body: &BodyModel,
    editor_area: Rect,
) -> Option<led_driver_terminal_core::RenamePopupModel> {
    use led_driver_terminal_core::RenamePopupModel;
    let state = lsp_extras.rename.as_ref()?;
    let (cur_row, cur_col) = match body {
        BodyModel::Content {
            cursor: Some((r, c)),
            ..
        } => (*r, *c),
        _ => return None,
    };
    // Legacy width: " Rename: " (9) + input chars + 2 trailing
    // padding cols. Keeps the box visibly distinct from
    // surrounding buffer content even on empty input.
    let input_chars = state.input.text.chars().count();
    let label_cols: u16 = 9; // " Rename: "
    let width_unclamped = label_cols
        .saturating_add(input_chars as u16)
        .saturating_add(2);
    // Cursor offset within `input.text` measured in chars (not
    // bytes) — `TextInput.cursor` is a byte index but always
    // sits on a char boundary by construction.
    let input_cursor_chars =
        state.input.text[..state.input.cursor].chars().count() as u16;
    let anchor_x = editor_area.x.saturating_add(cur_col);
    let anchor_y_row = (cur_row as usize)
        .saturating_add(1)
        .min(editor_area.rows.saturating_sub(1) as usize) as u16;
    let anchor_y = editor_area.y.saturating_add(anchor_y_row);
    // Clamp width so the popup never spills past the editor's
    // right edge.
    let area_right = editor_area.x.saturating_add(editor_area.cols);
    let max_width = area_right.saturating_sub(anchor_x);
    let width = width_unclamped.min(max_width);
    if width < label_cols {
        return None;
    }
    Some(RenamePopupModel {
        input: Arc::<str>::from(state.input.text.as_str()),
        input_cursor: input_cursor_chars,
        anchor: (anchor_x, anchor_y),
        width,
    })
}

fn severity_rank(s: DiagnosticSeverity) -> u8 {
    match s {
        DiagnosticSeverity::Error => 0,
        DiagnosticSeverity::Warning => 1,
        DiagnosticSeverity::Info => 2,
        DiagnosticSeverity::Hint => 3,
    }
}

/// Ratatui-compatible greedy word wrap. Breaks at ASCII spaces;
/// long tokens with no whitespace are split at the width. Output
/// lines have no trailing whitespace.
fn word_wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        if word_len > width {
            // Split oversized word across lines.
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            let chars: Vec<char> = word.chars().collect();
            for chunk in chars.chunks(width) {
                out.push(chunk.iter().collect());
            }
            continue;
        }
        let sep_len = if line.is_empty() { 0 } else { 1 };
        if line.chars().count() + sep_len + word_len > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Center-window trim for a hit's preview so the match sits in
/// the middle of the visible column. Returns the trimmed preview
/// and the 0-indexed char offset at which the match starts inside
/// it — the painter uses the second value to draw the match-
/// highlight segment.
///
/// Uses `hit.col` (1-indexed character offset) rather than
/// `match_start` (byte offset), so multi-byte UTF-8 content doesn't
/// miscount. Mirrors legacy `display.rs::file_search_hit_spans`
/// (centers the match within `avail`, clamps the window to the
/// preview length, no ellipsis — narrow column gets a literal
/// substring slice).
fn trimmed_preview(
    hit: &led_state_file_search::FileSearchHit,
    budget: usize,
) -> (String, usize) {
    let match_char_idx = hit.col.saturating_sub(1);
    let preview_chars: Vec<char> = hit.preview.chars().collect();
    let preview_len = preview_chars.len();
    if preview_len <= budget {
        return (hit.preview.clone(), match_char_idx);
    }
    let match_len = chars_between(&hit.preview, hit.match_start, hit.match_end);
    let context_before = budget.saturating_sub(match_len) / 2;
    let mut win_start = match_char_idx.saturating_sub(context_before);
    let win_end = (win_start + budget).min(preview_len);
    if win_end.saturating_sub(budget) < win_start {
        win_start = win_end.saturating_sub(budget);
    }
    let visible: String = preview_chars[win_start..win_end].iter().collect();
    let match_in_window = match_char_idx.saturating_sub(win_start);
    (visible, match_in_window)
}

/// Test helper — accept the budget the caller wants so each
/// test can verify the centering behaviour with a realistic
/// (or deliberately tiny) column budget.
#[cfg(test)]
fn trim_preview_at_budget(
    hit: &led_state_file_search::FileSearchHit,
    budget: usize,
) -> String {
    trimmed_preview(hit, budget).0
}

/// Char count of an unsigned integer rendered via `Display` — used
/// to compute the width of the `"{line}"` segment in a hit row.
fn count_chars_of_usize(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut n = n;
    let mut c = 0;
    while n > 0 {
        n /= 10;
        c += 1;
    }
    c
}

/// Number of characters (not bytes) in `s[byte_start..byte_end]`.
/// Clamps a bad byte range to an empty slice rather than panicking —
/// the driver sets sensible offsets, but a defensive cast keeps
/// malformed hits from crashing the painter.
fn chars_between(s: &str, byte_start: usize, byte_end: usize) -> usize {
    if byte_end <= byte_start
        || byte_end > s.len()
        || !s.is_char_boundary(byte_start)
        || !s.is_char_boundary(byte_end)
    {
        return 0;
    }
    s[byte_start..byte_end].chars().count()
}

/// "What clipboard action should we fire this tick?"
///
/// Returns `None` on an idle tick (no yank pending, no write
/// queued). Returns `Some(Read)` when a yank is pending and no
/// read is in flight. Returns `Some(Write(_))` with a clone of the
/// pending text when a kill queued one. When both signals are
/// live, yank wins — matches legacy ordering.
///
/// Zero allocation on idle (returns a simple `Option`); one Arc
/// clone on the Write path, which is the same as the driver's own
/// execute.
#[drv::memo(single)]
pub fn clipboard_action<'c>(clip: ClipboardStateInput<'c>) -> Option<ClipboardAction> {
    if clip.pending_yank.is_some() && !*clip.read_in_flight {
        Some(ClipboardAction::Read)
    } else {
        clip.pending_write
            .as_ref()
            .map(|text| ClipboardAction::Write(text.clone()))
    }
}

/// "What completion request does the find-file overlay need fired?"
///
/// Derives a `FindFileCmd` from the overlay's `input` by splitting at
/// the last `/`: trailing-slash inputs list the dir itself with an
/// empty prefix, otherwise we list the parent with the leaf as the
/// case-insensitive prefix. `show_hidden` flips on when the prefix
/// starts with `.`.
///
/// The memo cache-keys on `FindFileInput`, so an unchanged input
/// string returns the same `Vec` and the driver sees no re-fire.
/// Activation changes `input` (None → Some(...)) — the memo
/// recomputes and emits the initial request. Input edits each
/// change the string and re-fire.
///
/// Returns an empty `Vec` when the overlay is inactive.
#[drv::memo(single)]
pub fn find_file_action<'f>(
    ff: FindFileInput<'f>,
) -> Vec<led_driver_find_file_core::FindFileCmd> {
    // Execute-pattern: dispatch pushed one `FindFileCmd` per input
    // edit into the state's queue; the memo ships the whole queue,
    // and the main loop drains it after execute. Inactive overlay
    // or empty queue → empty Vec (zero alloc hot path).
    let Some(state) = ff.overlay.as_ref() else {
        return Vec::new();
    };
    state.pending_find_file_list.clone()
}

/// Per-file category set for the whole workspace. Mirrors legacy
/// `led_state::annotations::file_categories_map` and feeds the
/// browser painter + (later) the Alt-./ nav cycle.
///
/// **M19 scope:** LSP Error / Warning, plus git file-level
/// categories (Unstaged, StagedModified, StagedNew, Untracked).
/// Info / Hint are filtered out per legacy — they never colour
/// the browser. PR membership (PrComment / PrDiff) joins via the
/// same `IssueCategory` pipeline at M27.
#[drv::memo(single)]
pub fn file_categories_map<'d>(
    diagnostics: DiagnosticsStatesInput<'d>,
    git: GitStateInput<'d>,
) -> Arc<imbl::HashMap<CanonPath, imbl::HashSet<led_core::IssueCategory>>> {
    let mut map: imbl::HashMap<CanonPath, imbl::HashSet<led_core::IssueCategory>> =
        imbl::HashMap::default();

    // LSP diagnostics — Error/Warning only, Info/Hint silent.
    for (path, bd) in diagnostics.by_path.iter() {
        for d in bd.diagnostics.iter() {
            let cat = match d.severity {
                led_state_diagnostics::DiagnosticSeverity::Error => {
                    led_core::IssueCategory::LspError
                }
                led_state_diagnostics::DiagnosticSeverity::Warning => {
                    led_core::IssueCategory::LspWarning
                }
                _ => continue,
            };
            map.entry(path.clone())
                .or_insert_with(imbl::HashSet::default)
                .insert(cat);
        }
    }

    // Git file-level statuses. `IssueCategory::resolve_display`
    // picks the winning letter / colour when a path carries both
    // a diagnostic and a git category (LSP precedes git by
    // `IssueCategory::precedence`).
    for (path, cats) in git.file_statuses.iter() {
        let entry = map.entry(path.clone()).or_default();
        for c in cats.iter() {
            entry.insert(*c);
        }
    }

    // PR membership arrives at M27 via the same merge pattern.

    Arc::new(map)
}

/// Shared input for the three browser-derived memos
/// (`browser_auto_expanded`, `browser_entries`, `file_list_action`).
/// All three read the same triple — drv 0.4 nested-inputs shape
/// lets them share the bundle instead of each taking three
/// positional args.
#[derive(Copy, Clone, drv::Input)]
pub struct BrowserDerivedInputs<'a> {
    pub fs: FsTreeInput<'a>,
    pub ui: BrowserUiInput<'a>,
    pub tabs: TabsActiveInput<'a>,
    pub edits: EditedBuffersInput<'a>,
}

/// Auto-expanded ancestor chain for the active tab, excluding
/// user-pinned dirs. Pure derivation — no state written anywhere.
/// Memoized so downstream consumers (entries walk, list-action
/// emitter, painter) share the computation.
///
/// Persistent ancestor reveal is handled separately: the runtime
/// writes ancestors of any newly-activated tab into
/// `browser.expanded_dirs` once, mirroring legacy's
/// `reveal_active_buffer` (`led/src/model/action/helpers.rs:36`).
/// Once persisted there, the user can collapse them at will and
/// the collapse sticks.
#[drv::memo(single)]
pub fn browser_auto_expanded<'a>(
    inputs: BrowserDerivedInputs<'a>,
) -> Arc<imbl::HashSet<CanonPath>> {
    let BrowserDerivedInputs { fs, ui, tabs, edits: _ } = inputs;
    let active_path = (*tabs.active)
        .and_then(|id| tabs.open.iter().find(|t| t.id == id))
        .map(|t| t.path.clone());
    Arc::new(led_state_browser::ancestors_of(
        &led_state_browser::FsTree {
            root: fs.root.clone(),
            dir_contents: fs.dir_contents.clone(),
        },
        ui.expanded_dirs,
        active_path.as_ref(),
    ))
}

/// Flattened browser tree — the single visible-row list every
/// consumer walks. Pure derivation of
/// `(fs, expanded_dirs ∪ auto_expanded_dirs)`. `Arc`-wrapped so
/// the memo cache holds the same allocation across cache hits.
#[drv::memo(single)]
pub fn browser_entries<'a>(
    inputs: BrowserDerivedInputs<'a>,
) -> Arc<Vec<TreeEntry>> {
    let BrowserDerivedInputs { fs, ui, tabs: _, edits: _ } = inputs;
    // Ancestor reveal lives in `expanded_dirs` itself — the runtime
    // persists ancestors of any newly-activated tab on the
    // file_load completion path (legacy `reveal_active_buffer`).
    // No transient overlay; collapse_dir / collapse_all stick.
    let fs_copy = led_state_browser::FsTree {
        root: fs.root.clone(),
        dir_contents: fs.dir_contents.clone(),
    };
    let entries = led_state_browser::walk_tree(&fs_copy, ui.expanded_dirs);
    Arc::new(entries)
}

/// Resolve `selected_path` to a row index in the current
/// entries. Used by dispatch (arrow nav, expand/collapse) and
/// the painter (which row to highlight). Returns 0 when the
/// selected path is absent, falls outside the current tree, or
/// the entries list is empty — matching the historical
/// `selected: usize = 0` default.
pub fn browser_selected_idx(
    entries: &[TreeEntry],
    selected_path: Option<&CanonPath>,
) -> usize {
    let Some(target) = selected_path else {
        return 0;
    };
    entries
        .iter()
        .position(|e| &e.path == target)
        .unwrap_or(0)
}

/// "What directory listings do we still need?"
///
/// Emits one `ListCmd::List` per path that's expected to have a
/// listing (workspace root, every user-expanded dir, every
/// auto-revealed ancestor of the active tab) but isn't in
/// `dir_contents` yet. Used to drive `FsListDriver::execute`.
#[drv::memo(single)]
pub fn file_list_action<'a>(
    inputs: BrowserDerivedInputs<'a>,
) -> Vec<ListCmd> {
    let BrowserDerivedInputs { fs, ui, tabs: _, edits: _ } = inputs;
    let mut out: Vec<ListCmd> = Vec::new();
    if let Some(root) = fs.root.as_ref()
        && !fs.dir_contents.contains_key(root)
    {
        out.push(ListCmd::List(root.clone()));
    }
    for dir in ui.expanded_dirs.iter() {
        if !fs.dir_contents.contains_key(dir) {
            out.push(ListCmd::List(dir.clone()));
        }
    }
    // Auto-reveal listings come for free here: the runtime
    // persists ancestor expansions into `expanded_dirs` on the
    // file_load completion path (mirrors legacy
    // `reveal_active_buffer`), so the loop above already covers
    // them. We don't need a separate auto-reveal pass.
    out
}

/// Top-level render model. Composes the per-region memos — each
/// independently cached in its own per-memo thread-local cache.
///
/// Bundle of every projection the top-level render memo reads.
/// Composed of 13 narrower projection inputs (plus the
/// `render_tick` scalar), each of which is itself a
/// `#[derive(drv::Input)]` — the drv 0.4 nested-inputs shape.
/// Callers construct one labelled struct literal instead of
/// positionally lining up fourteen arguments; the inner memo
/// takes this whole bundle, and drv's per-field `eq_static`
/// walks into each projection so a single-field change still
/// invalidates correctly.
#[derive(Copy, Clone, drv::Input)]
pub struct RenderInputs<'a> {
    pub term: TerminalDimsInput<'a>,
    pub edits: EditedBuffersInput<'a>,
    pub store: StoreLoadedInput<'a>,
    pub tabs: TabsActiveInput<'a>,
    pub alerts: AlertsInput<'a>,
    pub browser: BrowserUiInput<'a>,
    pub fs: FsTreeInput<'a>,
    pub overlays: OverlaysInput<'a>,
    pub syntax: SyntaxStatesInput<'a>,
    pub diagnostics: DiagnosticsStatesInput<'a>,
    pub lsp: LspStatusesInput<'a>,
    pub completions: CompletionsSessionInput<'a>,
    pub lsp_extras: LspExtrasOverlayInput<'a>,
    pub git: GitStateInput<'a>,
    /// Current frame in 80ms buckets. Used by the status-bar
    /// spinner formatter; the main loop quantises wall-clock
    /// millis to 80 so the memo only invalidates once per
    /// spinner frame, not on every recompute. Pin to `0` when
    /// no LSP server is busy so the memo stays warm.
    pub render_tick: u64,
}

#[drv::memo(single)]
pub fn render_frame<'a>(inputs: RenderInputs<'a>) -> Option<Frame> {
    let RenderInputs {
        term,
        edits,
        store,
        tabs,
        alerts,
        browser,
        fs,
        overlays,
        syntax,
        diagnostics,
        lsp,
        completions,
        lsp_extras,
        git,
        render_tick,
    } = inputs;
    let dims = (*term.dims)?;
    let layout = Layout::compute(dims, *browser.visible);
    let tab_bar = tab_bar_model(tabs, edits);
    let body = body_model(BodyInputs {
        edits,
        store,
        tabs,
        overlays,
        syntax,
        diagnostics,
        git,
        area: layout.editor_area,
    });
    let status_bar = status_bar_model(StatusBarInputs {
        alerts,
        tabs,
        edits,
        overlays,
        diagnostics,
        lsp,
        lsp_extras,
        git,
        render_tick,
    });
    let side_panel = layout
        .side_area
        .map(|area| {
            side_panel_model(SidePanelInputs {
                fs,
                browser,
                overlays,
                tabs,
                diagnostics,
                git,
                edits,
                rows: area.rows,
            })
        });
    let popover =
        popover_model(edits, tabs, overlays, browser, diagnostics, layout.editor_area);
    // Code-action picker wins when live — the rename overlay
    // and code-action picker are mutually exclusive with
    // completions (dispatch guards that in `run_command`), so
    // whichever is populated paints into the shared
    // `completion` slot of the frame.
    let completion = code_action_popup_model(lsp_extras, tabs, layout.editor_area)
        .or_else(|| completion_popup_model(completions, tabs, layout.editor_area));
    let rename_popup = rename_popup_model(lsp_extras, &body, layout.editor_area);
    // Cursor placement, in priority order:
    //
    // 1. Find-file overlay active → status-bar row, column = prompt
    //    length + overlay input cursor. Byte offsets are ASCII-safe
    //    for the English prompts; if `input` ever carries non-ASCII
    //    the overlay's own cursor field is already a char-boundary
    //    byte index, and we convert to display columns here.
    // 2. Side-panel focus → no cursor (M11 cursor-hide rule).
    // 3. Otherwise, map the body cursor from editor-area-relative
    //    coords to absolute terminal coords.
    let cursor = if let Some(popup) = rename_popup.as_ref() {
        // Inside the rename popup, after " Rename: " (9 cols) plus
        // however many input chars precede the input cursor.
        let prefix_cols: u16 = 9;
        Some((
            popup.anchor.0
                .saturating_add(prefix_cols)
                .saturating_add(popup.input_cursor),
            popup.anchor.1,
        ))
    } else if let Some(state) = overlays.find_file.as_ref() {
        let prefix_cols: u16 = match state.mode {
            led_state_find_file::FindFileMode::Open => 12, // " Find file: "
            led_state_find_file::FindFileMode::SaveAs => 10, // " Save as: "
        };
        let input_col = state.input.text[..state.input.cursor].chars().count() as u16;
        Some((
            prefix_cols.saturating_add(input_col),
            layout.status_bar.y,
        ))
    } else if *browser.focus == Focus::Side {
        None
    } else {
        match &body {
            BodyModel::Content {
                cursor: Some((row, col)),
                ..
            } => Some((
                layout.editor_area.x.saturating_add(*col),
                layout.editor_area.y.saturating_add(*row),
            )),
            _ => None,
        }
    };
    Some(Frame {
        tab_bar,
        body,
        status_bar,
        side_panel,
        popover,
        completion,
        rename_popup,
        layout,
        cursor,
        dims,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;
    use ropey::Rope;
    use std::sync::Arc;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn fixture(
        paths: &[(&str, u64)],
        active: Option<u64>,
        loaded: &[(&str, LoadState)],
        dims: Option<Dims>,
    ) -> (Tabs, BufferEdits, BufferStore, Terminal) {
        let mut t = Tabs::default();
        for (p, id) in paths {
            t.open.push_back(Tab {
                id: TabId(*id),
                path: canon(p),
                ..Default::default()
            });
        }
        t.active = active.map(TabId);

        let mut s = BufferStore::default();
        for (p, st) in loaded {
            s.loaded.insert(canon(p), st.clone());
        }

        // M3 default: tests exercise the fallback (no edits seeded).
        // Individual cases that want to exercise the edits path seed
        // entries directly before rendering.
        let e = BufferEdits::default();

        let term = Terminal {
            dims,
            ..Default::default()
        };

        (t, e, s, term)
    }

    #[test]
    fn load_action_emits_load_for_absent_paths() {
        let store = BufferStore::default();
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            ..Default::default()
        });
        tabs.open.push_back(Tab {
            id: TabId(2),
            path: canon("b.rs"),
            ..Default::default()
        });

        let acts = file_load_action(
            StoreLoadedInput::new(&store),
            TabsOpenInput::new(&tabs),
        );
        assert_eq!(acts.len(), 2);
    }

    #[test]
    fn load_action_skips_already_tracked() {
        let mut store = BufferStore::default();
        store.loaded.insert(canon("pending.rs"), LoadState::Pending);
        store.loaded.insert(
            canon("ready.rs"),
            LoadState::Ready(Arc::new(Rope::from_str("x"))),
        );

        let mut tabs = Tabs::default();
        for (i, p) in ["pending.rs", "ready.rs", "new.rs"].iter().enumerate() {
            tabs.open.push_back(Tab {
                id: TabId(i as u64 + 1),
                path: canon(p),
                ..Default::default()
            });
        }

        let acts = file_load_action(
            StoreLoadedInput::new(&store),
            TabsOpenInput::new(&tabs),
        );
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0], LoadAction::Load(canon("new.rs")));
    }

    fn render(t: &Tabs, e: &BufferEdits, s: &BufferStore, term: &Terminal) -> Option<Frame> {
        let alerts = AlertState::default();
        // Tests render without the side panel so body layout matches
        // the pre-M11 assertions — M11 tests for the side panel are
        // separate.
        let browser = BrowserUi {
            visible: false,
            ..Default::default()
        };
        let ff = None;
        let is = None;
        let syntax = SyntaxStates::default();
        let diags = DiagnosticsStates::default();
        let lsp = LspStatuses::default();
        render_frame(RenderInputs {
            term: TerminalDimsInput::new(term),
            edits: EditedBuffersInput::new(e),
            store: StoreLoadedInput::new(s),
            tabs: TabsActiveInput::new(t),
            alerts: AlertsInput::new(&alerts),
            browser: BrowserUiInput::new(&browser),
            fs: FsTreeInput::new(&led_state_browser::FsTree::default()),
            overlays: OverlaysInput::new(&ff, &is, &None),
            syntax: SyntaxStatesInput::new(&syntax),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            lsp: LspStatusesInput::new(&lsp),
            completions: CompletionsSessionInput::new(
                &led_state_completions::CompletionsState::default(),
            ),
            lsp_extras: LspExtrasOverlayInput::new(&led_state_lsp::LspExtrasState::default()),
            git: GitStateInput::new(&led_state_git::GitState::default()),
            render_tick: 0,
        })
    }

    #[test]
    fn render_frame_none_until_dims_known() {
        let (t, e, s, term) = fixture(&[("a.rs", 1)], Some(1), &[], None);
        assert!(render(&t, &e, &s, &term).is_none());
    }

    #[test]
    fn render_frame_shows_pending_before_content_arrives() {
        let (t, e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[],
            Some(Dims { cols: 80, rows: 24 }),
        );
        let frame = render(&t, &e, &s, &term).expect("dims set");
        assert_eq!(*frame.tab_bar.labels, vec![" a.rs".to_string()]);
        assert_eq!(frame.tab_bar.active, Some(0));
        // Pre-load body renders as a blank Content frame: the
        // single rope line (len_lines=1) paints as an empty body
        // row, every row past it paints as a tilde. No inline
        // "loading..." placeholder — we keep the editing canvas
        // clean while the async read resolves.
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                // First body row is the empty content line.
                assert_eq!(lines[0].text.trim_end(), "");
                // Every row past line 0 is a tilde (past-EOF).
                assert!(
                    lines[1..].iter().all(|l| l.text.starts_with("~ ")),
                    "rows past line 0 should all be tildes",
                );
            }
            other => panic!("expected Content (blank), got {other:?}"),
        }
    }

    #[test]
    fn render_frame_parks_cursor_in_status_bar_when_find_file_active() {
        use led_state_find_file::{FindFileState, FindFileMode};
        // Any body content works — when the overlay is open the
        // cursor moves to the status-bar prompt regardless of what
        // the buffer contains.
        let (t, e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[("a.rs", LoadState::Ready(Arc::new(Rope::from_str("hi"))))],
            Some(Dims { cols: 80, rows: 24 }),
        );
        let alerts = AlertState::default();
        let browser = BrowserUi { visible: false, ..Default::default() };
        // Open mode: prefix " Find file: " is 12 cols; `input.cursor`
        // at byte 4 in "abcd" is 4 chars → absolute col 16.
        let mut ff_state = FindFileState::open("abcd".to_string());
        ff_state.input.cursor = 4;
        assert_eq!(ff_state.mode, FindFileMode::Open);
        let ff = Some(ff_state);
        let is = None;

        let syntax = SyntaxStates::default();
        let diags = DiagnosticsStates::default();
        let lsp = LspStatuses::default();
        let frame = render_frame(RenderInputs {
            term: TerminalDimsInput::new(&term),
            edits: EditedBuffersInput::new(&e),
            store: StoreLoadedInput::new(&s),
            tabs: TabsActiveInput::new(&t),
            alerts: AlertsInput::new(&alerts),
            browser: BrowserUiInput::new(&browser),
            fs: FsTreeInput::new(&led_state_browser::FsTree::default()),
            overlays: OverlaysInput::new(&ff, &is, &None),
            syntax: SyntaxStatesInput::new(&syntax),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            lsp: LspStatusesInput::new(&lsp),
            completions: CompletionsSessionInput::new(
                &led_state_completions::CompletionsState::default(),
            ),
            lsp_extras: LspExtrasOverlayInput::new(&led_state_lsp::LspExtrasState::default()),
            git: GitStateInput::new(&led_state_git::GitState::default()),
            render_tick: 0,
        })
        .expect("dims set");

        // dims.rows = 24 → status bar at row 23.
        assert_eq!(frame.cursor, Some((16, 23)));
    }

    #[test]
    fn render_frame_hides_cursor_when_side_panel_focused() {
        // With focus on the side panel the editor cursor must be
        // `None` — otherwise crossterm would `Show` it at the body
        // origin while the user is navigating the tree.
        let body = "hello".to_string();
        let (t, e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[("a.rs", LoadState::Ready(Arc::new(Rope::from_str(&body))))],
            Some(Dims { cols: 80, rows: 24 }),
        );
        let alerts = AlertState::default();
        let browser = BrowserUi {
            visible: false,
            focus: Focus::Side,
            ..Default::default()
        };
        let ff = None;
        let is = None;
        let syntax = SyntaxStates::default();
        let diags = DiagnosticsStates::default();
        let lsp = LspStatuses::default();
        let frame = render_frame(RenderInputs {
            term: TerminalDimsInput::new(&term),
            edits: EditedBuffersInput::new(&e),
            store: StoreLoadedInput::new(&s),
            tabs: TabsActiveInput::new(&t),
            alerts: AlertsInput::new(&alerts),
            browser: BrowserUiInput::new(&browser),
            fs: FsTreeInput::new(&led_state_browser::FsTree::default()),
            overlays: OverlaysInput::new(&ff, &is, &None),
            syntax: SyntaxStatesInput::new(&syntax),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            lsp: LspStatusesInput::new(&lsp),
            completions: CompletionsSessionInput::new(
                &led_state_completions::CompletionsState::default(),
            ),
            lsp_extras: LspExtrasOverlayInput::new(&led_state_lsp::LspExtrasState::default()),
            git: GitStateInput::new(&led_state_git::GitState::default()),
            render_tick: 0,
        })
        .expect("dims set");
        assert_eq!(frame.cursor, None);
    }

    #[test]
    fn render_frame_shows_content_truncated_to_viewport() {
        let body = (0..30).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let (t, e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[("a.rs", LoadState::Ready(Arc::new(Rope::from_str(&body))))],
            Some(Dims { cols: 10, rows: 5 }),
        );
        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, cursor, .. } => {
                // body_rows = dims.rows - 2 (tab bar + status bar).
                assert_eq!(lines.len(), 3);
                // Each content row is 2-col gutter + truncated content.
                assert_eq!(lines[0].text, "  line 0");
                assert_eq!(lines[2].text, "  line 2");
                // Default cursor at (0, 0) → gutter-shifted to col 2.
                assert_eq!(*cursor, Some((0, 2)));
            }
            other => panic!("expected Content, got {other:?}"),
        }
        // Body starts at row 0 now — no +1.
        assert_eq!(frame.cursor, Some((2, 0)));
    }

    #[test]
    fn body_model_scrolls_and_reports_cursor_inside_window() {
        let body = (0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let (mut t, e, s, term) = fixture(
            &[("big.rs", 1)],
            Some(1),
            &[("big.rs", LoadState::Ready(Arc::new(Rope::from_str(&body))))],
            Some(Dims { cols: 40, rows: 11 }), // body_rows = 11 - 2 = 9
        );
        // Place cursor at line 25 with scroll.top = 20 → cursor visible at row 5.
        t.open[0].cursor = Cursor { line: 25, col: 2, preferred_col: 2 };
        t.open[0].scroll = Scroll { top: 20, top_sub_line: led_core::SubLine(0) };

        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, cursor, .. } => {
                assert_eq!(lines.len(), 9);
                assert_eq!(lines[0].text, "  line 20");
                assert_eq!(lines[5].text, "  line 25");
                // Cursor col 2 → screen col 4 (gutter shift).
                assert_eq!(*cursor, Some((5, 4)));
            }
            other => panic!("expected Content, got {other:?}"),
        }
        // Absolute frame cursor = (col, row) — body starts at row 0.
        assert_eq!(frame.cursor, Some((4, 5)));
    }

    #[test]
    fn body_model_wraps_long_logical_line_across_multiple_body_rows() {
        // cols=12 → editor_area.cols=12; minus 2 gutter + 0
        // trailing reserved col = content_cols 10, wrap_width 9
        // (one trailing col per non-last sub: `\`).
        // A 50-char line splits into 6 sub-lines of widths
        // 9/9/9/9/9/5.
        let rope = Arc::new(Rope::from_str(
            "abcdefghij0123456789ABCDEFGHIJ!@#$%^&*()qwertyuiop",
        ));
        let (mut t, e, s, term) = fixture(
            &[("wide.rs", 1)],
            Some(1),
            &[("wide.rs", LoadState::Ready(rope))],
            Some(Dims { cols: 12, rows: 11 }), // body_rows = 9
        );
        // Cursor at col 25 → sub 25/9 = 2, within 25 % 9 = 7.
        t.open[0].cursor = Cursor { line: 0, col: 25, preferred_col: 7 };
        t.open[0].scroll = Scroll { top: 0, top_sub_line: led_core::SubLine(0) };

        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, cursor, .. } => {
                assert_eq!(lines.len(), 9);
                assert_eq!(lines[0].text, "  abcdefghi\\");
                assert_eq!(lines[1].text, "  j01234567\\");
                assert_eq!(lines[2].text, "  89ABCDEFG\\");
                assert_eq!(lines[3].text, "  HIJ!@#$%^\\");
                assert_eq!(lines[4].text, "  &*()qwert\\");
                assert_eq!(lines[5].text, "  yuiop");
                assert_eq!(lines[6].text, "~ ");
                // Cursor on sub 2 within 7 → body row 2, screen col 2+7=9.
                assert_eq!(*cursor, Some((2, 9)));
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn body_model_renders_wrap_glyph_on_every_non_last_sub_of_long_line() {
        // Regression guard for the README.md rendering bug where
        // the last visible wrap row on a long logical line came
        // out without its trailing `\` (the user saw `algesten/s`
        // where `algesten/s\` was expected, with `tr0m).` on the
        // next row). The line is the full M1 README warning
        // paragraph (410 chars). At content_cols=102 it wraps
        // into 5 sub-lines: 4 non-last + 1 last. Every non-last
        // sub must carry `\` regardless of whether it's the final
        // row the body rendered.
        let line = "> **Vibe coded.** This project is an experiment in getting an \
AI assistant to follow Functional Reactive Programming (FRP) principles and \
produce reasonable code within that discipline. I've focused on the overall \
architecture rather than reviewing the code output in detail. For projects \
I've mostly written by hand, see [ureq](https://github.com/algesten/ureq) and \
[str0m](https://github.com/algesten/str0m).";
        assert_eq!(line.chars().count(), 410);
        let rope = Arc::new(Rope::from_str(line));
        let (t, e, s, term) = fixture(
            &[("README.md", 1)],
            Some(1),
            &[("README.md", LoadState::Ready(rope))],
            // cols=104 → editor_area.cols=104; content_cols=102;
            // wrap_width=101 → 5 sub-lines (101/101/101/101/6).
            // rows=8 gives body_rows=6, enough to show all 5 +
            // one tilde row.
            Some(Dims { cols: 104, rows: 8 }),
        );
        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                // Subs 0..3 are non-last → must end with `\`.
                for i in 0..4 {
                    assert!(
                        lines[i].text.ends_with('\\'),
                        "sub {i} missing wrap glyph: {:?}",
                        lines[i].text
                    );
                }
                // Sub 4 is last → no `\`.
                assert!(!lines[4].text.ends_with('\\'));
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn body_model_wrap_glyph_survives_full_paint_pipeline() {
        // End-to-end regression for the reported `\ missing on
        // wrapped rows` bug. A 100-char logical line at a realistic
        // editor width should produce `\` at the right edge of each
        // non-last sub-line, visible in the painted byte stream.
        let text: String = (0..100).map(|i| (b'A' + (i % 26) as u8) as char).collect();
        let rope = Arc::new(Rope::from_str(&text));
        let (t, e, s, term) = fixture(
            &[("long.md", 1)],
            Some(1),
            &[("long.md", LoadState::Ready(rope))],
            // rows=8 → body_rows=6; cols=30 → editor_area.cols=30;
            // content_cols=28, wrap_width=27 → sub-lines of 27/27/27/19.
            Some(Dims { cols: 30, rows: 8 }),
        );
        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                // Sub 0/1/2 non-last → end in `\`; sub 3 last → no `\`.
                assert!(
                    lines[0].text.ends_with('\\'),
                    "sub 0 missing wrap glyph: {:?}",
                    lines[0].text
                );
                assert!(lines[1].text.ends_with('\\'));
                assert!(lines[2].text.ends_with('\\'));
                assert!(!lines[3].text.ends_with('\\'));
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn body_model_honours_scroll_top_sub_line_on_wrapped_line() {
        // Start scrolled past the first two sub-lines of the same
        // logical line — body must show sub-line 2 onward.
        // body_rows = 4, content_cols = 10, wrap_width = 9.
        let rope = Arc::new(Rope::from_str(
            "abcdefghij0123456789ABCDEFGHIJ!@#$%^&*()qwertyuiop",
        ));
        let (mut t, e, s, term) = fixture(
            &[("wide.rs", 1)],
            Some(1),
            &[("wide.rs", LoadState::Ready(rope))],
            Some(Dims { cols: 12, rows: 6 }),
        );
        t.open[0].cursor = Cursor { line: 0, col: 25, preferred_col: 7 };
        t.open[0].scroll = Scroll { top: 0, top_sub_line: led_core::SubLine(2) };

        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, cursor, .. } => {
                assert_eq!(lines[0].text, "  89ABCDEFG\\");
                assert_eq!(lines[1].text, "  HIJ!@#$%^\\");
                assert_eq!(lines[2].text, "  &*()qwert\\");
                assert_eq!(lines[3].text, "  yuiop");
                // Cursor on sub 2 within 7 → body row 0, screen col 2+7=9.
                assert_eq!(*cursor, Some((0, 9)));
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn body_model_hides_cursor_when_scrolled_away() {
        let body = (0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let (mut t, e, s, term) = fixture(
            &[("big.rs", 1)],
            Some(1),
            &[("big.rs", LoadState::Ready(Arc::new(Rope::from_str(&body))))],
            Some(Dims { cols: 40, rows: 6 }), // 5 body rows
        );
        // Cursor far outside the scroll window.
        t.open[0].cursor = Cursor { line: 40, col: 0, preferred_col: 0 };
        t.open[0].scroll = Scroll { top: 0, top_sub_line: led_core::SubLine(0) };

        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { cursor, .. } => assert_eq!(*cursor, None),
            other => panic!("expected Content, got {other:?}"),
        }
        assert_eq!(frame.cursor, None);
    }

    #[test]
    fn render_frame_shows_blank_body_when_load_failed() {
        // Legitimate load errors (permission denied, etc.) render as
        // a blank body instead of painting the `io::Error` message
        // inside the editing canvas. Future milestones surface the
        // failure as a status-bar alert; the body stays clean.
        let (t, e, s, term) = fixture(
            &[("bad.rs", 1)],
            Some(1),
            &[("bad.rs", LoadState::Error(Arc::new("No such file".into())))],
            Some(Dims { cols: 80, rows: 24 }),
        );
        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                // Empty-rope body: first row blank, rest tildes.
                assert_eq!(lines[0].text.trim_end(), "");
                assert!(
                    lines[1..].iter().all(|l| l.text.starts_with("~ ")),
                    "error body should paint tildes, not inline the error message",
                );
                // The `io::Error` message must NOT appear anywhere
                // in the body text.
                for l in lines.iter() {
                    assert!(
                        !l.text.contains("No such file"),
                        "body rendered the error message inline: {:?}",
                        l.text,
                    );
                }
            }
            other => panic!("expected Content (blank), got {other:?}"),
        }
    }

    #[test]
    fn render_frame_body_empty_when_no_tabs() {
        let (t, e, s, term) = fixture(&[], None, &[], Some(Dims { cols: 80, rows: 24 }));
        let frame = render(&t, &e, &s, &term).expect("dims set");
        assert!(frame.tab_bar.labels.is_empty());
        assert_eq!(frame.tab_bar.active, None);
        assert!(matches!(frame.body, BodyModel::Empty));
    }

    // ── M3: edits-first body + dirty-prefixed tab bar ───────────────────

    #[test]
    fn body_model_prefers_edits_over_store() {
        let (t, mut e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[(
                "a.rs",
                LoadState::Ready(Arc::new(Rope::from_str("disk-version"))),
            )],
            Some(Dims { cols: 40, rows: 5 }),
        );
        // Seed edits with a different rope — this is what the user sees.
        e.buffers.insert(
            canon("a.rs"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("edited-version")),
                version: 1,
                saved_version: 0,
                disk_content_hash: led_core::PersistedContentHash::default(),
                history: Default::default(),
            },
        );

        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                assert_eq!(lines[0].text, "  edited-version");
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn body_model_falls_back_to_store_when_edits_absent() {
        let (t, e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[(
                "a.rs",
                LoadState::Ready(Arc::new(Rope::from_str("from-disk"))),
            )],
            Some(Dims { cols: 40, rows: 5 }),
        );
        // No seed → fallback path.
        assert!(e.buffers.is_empty());
        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                assert_eq!(lines[0].text, "  from-disk");
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    // ── M4: save action memo ────────────────────────────────────────────

    #[test]
    fn file_save_action_empty_when_nothing_pending() {
        let e = BufferEdits::default();
        let actions = file_save_action(
            PendingSavesInput::new(&e),
            EditedBuffersInput::new(&e),
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn file_save_action_emits_save_for_pending_dirty_buffer() {
        let mut e = BufferEdits::default();
        let path = canon("a.rs");
        let rope = Arc::new(Rope::from_str("payload"));
        e.buffers.insert(
            path.clone(),
            EditedBuffer {
                rope: rope.clone(),
                version: 3,
                saved_version: 0,
                disk_content_hash: led_core::PersistedContentHash::default(),
                history: Default::default(),
            },
        );
        e.pending_saves.insert(path.clone());

        let actions = file_save_action(
            PendingSavesInput::new(&e),
            EditedBuffersInput::new(&e),
        );
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            SaveAction::Save {
                path: p,
                rope: r,
                version,
            } => {
                assert_eq!(p, &path);
                assert!(Arc::ptr_eq(r, &rope));
                assert_eq!(*version, 3);
            }
            SaveAction::SaveAs { .. } => panic!("unexpected SaveAs"),
        }
    }

    #[test]
    fn file_save_action_emits_save_as_from_pending_map() {
        let mut e = BufferEdits::default();
        let from = canon("a.rs");
        let to = canon("b.rs");
        let rope = Arc::new(Rope::from_str("payload"));
        e.buffers.insert(
            from.clone(),
            EditedBuffer {
                rope: rope.clone(),
                version: 2,
                saved_version: 2, // pristine — SaveAs still fires
                disk_content_hash: led_core::PersistedContentHash::default(),
                history: Default::default(),
            },
        );
        e.pending_save_as.insert(from.clone(), to.clone());

        let actions = file_save_action(
            PendingSavesInput::new(&e),
            EditedBuffersInput::new(&e),
        );
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            SaveAction::SaveAs {
                from: f,
                to: t,
                rope: r,
                version,
            } => {
                assert_eq!(f, &from);
                assert_eq!(t, &to);
                assert!(Arc::ptr_eq(r, &rope));
                assert_eq!(*version, 2);
            }
            SaveAction::Save { .. } => panic!("unexpected Save"),
        }
    }

    #[test]
    fn file_save_action_emits_save_for_clean_buffer_too() {
        // "Save should always save": dispatch only inserts a path
        // into `pending_saves` when the user explicitly asks (Save
        // / SaveNoFormat). The query honours that intent and emits
        // a `Save` action even when the buffer is byte-identical
        // to disk — a no-op on disk, but the user's request still
        // round-trips through the file-write driver. SaveAll is
        // the gated path; it filters dirty buffers in
        // `request_save_all` before populating `pending_saves`.
        let mut e = BufferEdits::default();
        let path = canon("clean.rs");
        let rope = Arc::new(Rope::from_str("x"));
        e.buffers.insert(
            path.clone(),
            EditedBuffer {
                rope: rope.clone(),
                version: 0,
                saved_version: 0, // dirty() == false
                disk_content_hash: led_core::EphemeralContentHash::of_rope(&rope).persist(),
                history: Default::default(),
            },
        );
        e.pending_saves.insert(path.clone());

        let actions = file_save_action(
            PendingSavesInput::new(&e),
            EditedBuffersInput::new(&e),
        );
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            SaveAction::Save { path: p, .. } => assert_eq!(p, &path),
            other => panic!("expected SaveAction::Save, got {:?}", other),
        }
    }

    #[test]
    fn file_save_action_skips_pending_paths_with_no_buffer() {
        // Could happen if pending entry leaked past a tab close. Memo
        // must not panic or emit phantom saves.
        let mut e = BufferEdits::default();
        e.pending_saves.insert(canon("ghost.rs"));

        let actions = file_save_action(
            PendingSavesInput::new(&e),
            EditedBuffersInput::new(&e),
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn tab_bar_prefixes_dirty_labels_with_dot() {
        let (t, mut e, s, term) = fixture(
            &[("a.rs", 1), ("b.rs", 2)],
            Some(1),
            &[
                (
                    "a.rs",
                    LoadState::Ready(Arc::new(Rope::from_str("x"))),
                ),
                (
                    "b.rs",
                    LoadState::Ready(Arc::new(Rope::from_str("y"))),
                ),
            ],
            Some(Dims { cols: 40, rows: 5 }),
        );
        // a.rs clean, b.rs dirty.
        let a_rope = Arc::new(Rope::from_str("x"));
        e.buffers.insert(
            canon("a.rs"),
            EditedBuffer {
                rope: a_rope.clone(),
                version: 0,
                saved_version: 0,
                disk_content_hash: led_core::EphemeralContentHash::of_rope(&a_rope).persist(),
                history: Default::default(),
            },
        );
        e.buffers.insert(
            canon("b.rs"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("yy")),
                version: 1,
                saved_version: 0,
                disk_content_hash: led_core::PersistedContentHash::default(),
                history: Default::default(),
            },
        );

        let frame = render(&t, &e, &s, &term).expect("dims set");
        assert_eq!(
            *frame.tab_bar.labels,
            vec![" a.rs".to_string(), "\u{25cf}b.rs".to_string()]
        );
    }

    // ── M19: gutter category ─────────────────────────────────────────────

    #[test]
    fn merged_gutter_picks_git_unstaged() {
        // Bar is git/PR only — LSP severity is rendered separately
        // as the diagnostic dot in gutter col 1, never as the bar.
        use led_core::IssueCategory;
        use led_core::git::LineStatus;
        let statuses = vec![LineStatus {
            category: IssueCategory::Unstaged,
            rows: 0..1,
        }];
        let cat = merged_gutter_category(Some(&statuses), 0);
        assert_eq!(cat, Some(IssueCategory::Unstaged));
    }

    #[test]
    fn merged_gutter_falls_back_to_none_without_git_status() {
        // No git line status on the row → no bar, regardless of
        // any LSP severity that may live there.
        assert_eq!(merged_gutter_category(None, 0), None);
        let statuses: Vec<led_core::git::LineStatus> = Vec::new();
        assert_eq!(merged_gutter_category(Some(&statuses), 0), None);
    }

    #[test]
    fn body_model_paints_git_gutter_on_unstaged_line() {
        // Two-line rope. Line 1 carries a git Unstaged range; the
        // rendered row should carry `gutter_category = Some(Unstaged)`.
        use led_core::git::LineStatus;
        use led_core::IssueCategory;
        let (t, e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[(
                "a.rs",
                LoadState::Ready(Arc::new(Rope::from_str("clean\ndirty"))),
            )],
            Some(Dims { cols: 20, rows: 5 }),
        );
        let path = &t.open[0].path;
        let mut git = led_state_git::GitState::default();
        git.line_statuses.insert(
            path.clone(),
            Arc::new(vec![LineStatus {
                category: IssueCategory::Unstaged,
                rows: 1..2,
            }]),
        );
        let alerts = AlertState::default();
        let browser = BrowserUi {
            visible: false,
            ..Default::default()
        };
        let ff = None;
        let is = None;
        let fsrch = None;
        let syntax = SyntaxStates::default();
        let diags = DiagnosticsStates::default();
        let lsp = LspStatuses::default();
        let frame = render_frame(RenderInputs {
            term: TerminalDimsInput::new(&term),
            edits: EditedBuffersInput::new(&e),
            store: StoreLoadedInput::new(&s),
            tabs: TabsActiveInput::new(&t),
            alerts: AlertsInput::new(&alerts),
            browser: BrowserUiInput::new(&browser),
            fs: FsTreeInput::new(&led_state_browser::FsTree::default()),
            overlays: OverlaysInput::new(&ff, &is, &fsrch),
            syntax: SyntaxStatesInput::new(&syntax),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            lsp: LspStatusesInput::new(&lsp),
            completions: CompletionsSessionInput::new(
                &led_state_completions::CompletionsState::default(),
            ),
            lsp_extras: LspExtrasOverlayInput::new(
                &led_state_lsp::LspExtrasState::default(),
            ),
            git: GitStateInput::new(&git),
            render_tick: 0,
        })
        .expect("dims");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                assert!(lines[0].gutter_category.is_none());
                assert_eq!(
                    lines[1].gutter_category,
                    Some(IssueCategory::Unstaged),
                );
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    // ── M9: past-EOF tildes ─────────────────────────────────────────────

    #[test]
    fn body_model_fills_past_eof_rows_with_tilde() {
        // Two-line rope in a six-row viewport: body_rows = 4, so rows
        // 2 and 3 are past-EOF.
        let (t, e, s, term) = fixture(
            &[("short.rs", 1)],
            Some(1),
            &[(
                "short.rs",
                LoadState::Ready(Arc::new(Rope::from_str("one\ntwo"))),
            )],
            Some(Dims { cols: 20, rows: 6 }),
        );
        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                assert_eq!(lines.len(), 4);
                assert_eq!(lines[0].text, "  one");
                assert_eq!(lines[1].text, "  two");
                assert_eq!(lines[2].text, "~ ");
                assert_eq!(lines[3].text, "~ ");
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    // ── M9: status bar model ────────────────────────────────────────────

    fn status(a: &AlertState, t: &Tabs, e: &BufferEdits) -> StatusBarModel {
        let ff = None;
        let is = None;
        let diags = DiagnosticsStates::default();
        let lsp = LspStatuses::default();
        let git = led_state_git::GitState::default();
        status_bar_model(StatusBarInputs {
            alerts: AlertsInput::new(a),
            tabs: TabsActiveInput::new(t),
            edits: EditedBuffersInput::new(e),
            overlays: OverlaysInput::new(&ff, &is, &None),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            lsp: LspStatusesInput::new(&lsp),
            lsp_extras: LspExtrasOverlayInput::new(
                &led_state_lsp::LspExtrasState::default(),
            ),
            git: GitStateInput::new(&git),
            render_tick: 0,
        })
    }

    fn status_with_git(
        a: &AlertState,
        t: &Tabs,
        e: &BufferEdits,
        g: &led_state_git::GitState,
    ) -> StatusBarModel {
        let ff = None;
        let is = None;
        let diags = DiagnosticsStates::default();
        let lsp = LspStatuses::default();
        status_bar_model(StatusBarInputs {
            alerts: AlertsInput::new(a),
            tabs: TabsActiveInput::new(t),
            edits: EditedBuffersInput::new(e),
            overlays: OverlaysInput::new(&ff, &is, &None),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            lsp: LspStatusesInput::new(&lsp),
            lsp_extras: LspExtrasOverlayInput::new(
                &led_state_lsp::LspExtrasState::default(),
            ),
            git: GitStateInput::new(g),
            render_tick: 0,
        })
    }

    #[test]
    fn status_bar_default_empty_when_no_tab() {
        // Legacy shape: ` {branch}{modified}{pr}{lsp}` → always
        // has the one leading space, even when every dynamic
        // piece is empty. The right-side position string falls
        // back to `L1:C1 ` when no tab is active so the post-kill
        // status bar still anchors a position (matches legacy
        // `display.rs` reading the zero-init cursor row/col).
        let s = status(&AlertState::default(), &Tabs::default(), &BufferEdits::default());
        assert_eq!(&*s.left, " ");
        assert_eq!(&*s.right, "L1:C1 ");
        assert!(!s.is_warn);
    }

    #[test]
    fn status_bar_default_clean_shows_position_only() {
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            cursor: Cursor { line: 0, col: 0, preferred_col: 0 },
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let s = status(&AlertState::default(), &tabs, &BufferEdits::default());
        // Leading space from legacy's ` {modified}{lsp}` format
        // prefix, even with nothing in the dynamic slots.
        assert_eq!(&*s.left, " ");
        assert_eq!(&*s.right, "L1:C1 ");
        assert!(!s.is_warn);
    }

    #[test]
    fn status_bar_default_with_branch_prepends_name() {
        // M19: a live workspace branch shows as ` main …`.
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            cursor: Cursor { line: 0, col: 0, preferred_col: 0 },
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let mut git = led_state_git::GitState::default();
        git.branch = Some("main".into());
        let s = status_with_git(
            &AlertState::default(),
            &tabs,
            &BufferEdits::default(),
            &git,
        );
        // Legacy shape: ` {branch}{modified}{pr}{lsp}` — leading
        // space, then " main", nothing further.
        assert_eq!(&*s.left, "  main");
    }

    #[test]
    fn status_bar_default_with_branch_and_dirty() {
        // Dirty buffer + branch.
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            cursor: Cursor { line: 0, col: 0, preferred_col: 0 },
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a.rs"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("x")),
                version: 2,
                saved_version: 1,
                disk_content_hash: led_core::PersistedContentHash::default(),
                history: Default::default(),
            },
        );
        let mut git = led_state_git::GitState::default();
        git.branch = Some("feature/xyz".into());
        let s = status_with_git(&AlertState::default(), &tabs, &edits, &git);
        // ` ` + ` feature/xyz` + ` ●`.
        assert_eq!(&*s.left, "  feature/xyz \u{25cf}");
    }

    #[test]
    fn status_bar_default_dirty_shows_dot_and_position() {
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            cursor: Cursor { line: 4, col: 10, preferred_col: 10 },
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a.rs"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("x")),
                version: 3,
                saved_version: 1, // dirty
                disk_content_hash: led_core::PersistedContentHash::default(),
                history: Default::default(),
            },
        );
        let s = status(&AlertState::default(), &tabs, &edits);
        assert_eq!(&*s.left, "  \u{25cf}");
        assert_eq!(&*s.right, "L5:C11 ");
        assert!(!s.is_warn);
    }

    #[test]
    fn status_bar_shows_info_alert() {
        let a = AlertState {
            info: Some("Saved foo.rs".into()),
            ..Default::default()
        };
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let s = status(&a, &tabs, &BufferEdits::default());
        assert_eq!(&*s.left, " Saved foo.rs");
        assert_eq!(&*s.right, "L1:C1 ");
        assert!(!s.is_warn);
    }

    #[test]
    fn status_bar_shows_warn_with_warn_flag() {
        let a = AlertState {
            warns: vec![("a.rs".into(), "save a.rs: permission denied".into())],
            ..Default::default()
        };
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let s = status(&a, &tabs, &BufferEdits::default());
        assert_eq!(&*s.left, " save a.rs: permission denied");
        assert!(s.is_warn);
    }

    #[test]
    fn status_bar_info_wins_over_warn() {
        let a = AlertState {
            info: Some("Saved".into()),
            warns: vec![("k".into(), "oh no".into())],
            ..Default::default()
        };
        let s = status(&a, &Tabs::default(), &BufferEdits::default());
        assert_eq!(&*s.left, " Saved");
        assert!(!s.is_warn);
    }

    #[test]
    fn status_bar_confirm_kill_wins_over_info() {
        let a = AlertState {
            confirm_kill: Some(TabId(1)),
            info: Some("Saved".into()),
            ..Default::default()
        };
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("draft.txt"),
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let s = status(&a, &tabs, &BufferEdits::default());
        assert_eq!(&*s.left, " Kill buffer 'draft.txt'? (y/N) ");
        assert_eq!(&*s.right, "");
    }

    #[test]
    fn render_frame_composes_status_bar() {
        let (t, e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[("a.rs", LoadState::Ready(Arc::new(Rope::from_str("x"))))],
            Some(Dims { cols: 40, rows: 5 }),
        );
        let frame = render(&t, &e, &s, &term).expect("dims set");
        assert_eq!(&*frame.status_bar.right, "L1:C1 ");
    }

    // ── file-search side-panel scroll-follow ───────────────────────

    fn fs_group(
        relative: &str,
        hits: usize,
    ) -> led_state_file_search::FileSearchGroup {
        let path = canon(relative);
        let hits = (1..=hits)
            .map(|i| led_state_file_search::FileSearchHit {
                path: path.clone(),
                line: i,
                col: 1,
                preview: format!("hit {i}"),
                match_start: 0,
                match_end: 0,
            })
            .collect();
        led_state_file_search::FileSearchGroup {
            path,
            relative: relative.into(),
            hits,
        }
    }

    fn fs_state_with_results(
        groups: Vec<led_state_file_search::FileSearchGroup>,
        selection: led_state_file_search::FileSearchSelection,
    ) -> led_state_file_search::FileSearchState {
        let flat: Vec<_> = groups.iter().flat_map(|g| g.hits.iter().cloned()).collect();
        let mut query = led_core::TextInput::default();
        query.set("needle");
        led_state_file_search::FileSearchState {
            query,
            results: groups,
            flat_hits: flat,
            selection,
            ..Default::default()
        }
    }

    #[test]
    fn body_model_carries_match_highlight_when_preview_hit_is_on_active_tab() {
        use led_state_file_search::{FileSearchGroup, FileSearchHit};

        let path = canon("a.rs");
        let rope = Arc::new(Rope::from_str("line zero\nline one\n    foo here\nlast\n"));

        // Active tab is a.rs, scrolled to line 2 ("    foo here").
        let tab = Tab {
            id: TabId(1),
            path: path.clone(),
            cursor: Cursor::default(),
            scroll: Scroll { top: 2, top_sub_line: led_core::SubLine(0) },
            ..Default::default()
        };
        let mut t = Tabs::default();
        t.open.push_back(tab);
        t.active = Some(TabId(1));

        let mut e = BufferEdits::default();
        e.buffers.insert(
            path.clone(),
            led_state_buffer_edits::EditedBuffer::fresh(rope.clone()),
        );
        let s = BufferStore::default();

        // File-search overlay with selection on the hit: line 3
        // (1-indexed), "foo" at col 5 (0-indexed char 4), match
        // len 3.
        let hit = FileSearchHit {
            path: path.clone(),
            line: 3,
            col: 5,
            preview: "    foo here".into(),
            match_start: 4,
            match_end: 7,
        };
        let fs = Some(led_state_file_search::FileSearchState {
            results: vec![FileSearchGroup {
                path: path.clone(),
                relative: "a.rs".into(),
                hits: vec![hit.clone()],
            }],
            flat_hits: vec![hit],
            selection: led_state_file_search::FileSearchSelection::Result(0),
            ..Default::default()
        });
        let is = None;

        let syntax = SyntaxStates::default();
        let diags = DiagnosticsStates::default();
        let model = body_model(BodyInputs {
            edits: EditedBuffersInput::new(&e),
            store: StoreLoadedInput::new(&s),
            tabs: TabsActiveInput::new(&t),
            overlays: OverlaysInput::new(&None, &is, &fs),
            syntax: SyntaxStatesInput::new(&syntax),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            git: GitStateInput::new(&led_state_git::GitState::default()),
            area: Rect { x: 0, y: 0, cols: 40, rows: 5 },
        });
        match model {
            BodyModel::Content {
                match_highlight: Some(mh),
                ..
            } => {
                // Scroll.top = 2, hit line = 2 → body row 0.
                assert_eq!(mh.row, 0);
                // col_start = 4 + GUTTER_WIDTH(2) = 6.
                assert_eq!(mh.col_start, 6);
                // col_end = 7 + GUTTER_WIDTH = 9.
                assert_eq!(mh.col_end, 9);
            }
            other => panic!("expected Content with highlight, got {other:?}"),
        }
    }

    #[test]
    fn body_model_has_no_highlight_when_active_tab_differs_from_hit() {
        use led_state_file_search::{FileSearchGroup, FileSearchHit};

        let a = canon("a.rs");
        let b = canon("b.rs");
        let rope = Arc::new(Rope::from_str("text\n"));

        let mut t = Tabs::default();
        t.open.push_back(Tab {
            id: TabId(1),
            path: b.clone(), // active tab = b.rs
            ..Default::default()
        });
        t.active = Some(TabId(1));

        let mut e = BufferEdits::default();
        e.buffers.insert(
            b.clone(),
            led_state_buffer_edits::EditedBuffer::fresh(rope),
        );
        let s = BufferStore::default();

        // Hit lives on a.rs — should NOT paint a highlight on
        // b.rs's body.
        let hit = FileSearchHit {
            path: a.clone(),
            line: 1,
            col: 1,
            preview: "text".into(),
            match_start: 0,
            match_end: 4,
        };
        let fs = Some(led_state_file_search::FileSearchState {
            results: vec![FileSearchGroup {
                path: a.clone(),
                relative: "a.rs".into(),
                hits: vec![hit.clone()],
            }],
            flat_hits: vec![hit],
            selection: led_state_file_search::FileSearchSelection::Result(0),
            ..Default::default()
        });
        let is = None;

        let syntax = SyntaxStates::default();
        let diags = DiagnosticsStates::default();
        let model = body_model(BodyInputs {
            edits: EditedBuffersInput::new(&e),
            store: StoreLoadedInput::new(&s),
            tabs: TabsActiveInput::new(&t),
            overlays: OverlaysInput::new(&None, &is, &fs),
            syntax: SyntaxStatesInput::new(&syntax),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            git: GitStateInput::new(&led_state_git::GitState::default()),
            area: Rect { x: 0, y: 0, cols: 40, rows: 5 },
        });
        match model {
            BodyModel::Content { match_highlight, .. } => {
                assert_eq!(match_highlight, None);
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn file_search_sidebar_renders_from_explicit_scroll_offset() {
        // `scroll_offset` is maintained by dispatch; the renderer
        // just trusts it. Scroll=4 means tree rendering starts at
        // stream row 4 (hits 4 + 5 of a 6-hit group).
        let state = fs_state_with_results(
            vec![fs_group("a.rs", 6)],
            led_state_file_search::FileSearchSelection::Result(4),
        );
        let mut state = state;
        state.scroll_offset = 4;
        let model = file_search_side_panel(&state, 4);
        let names: Vec<&str> = model.rows.iter().map(|r| &*r.name).collect();
        assert_eq!(
            names,
            vec![" Aa   .*   =>", "needle", "   4: hit 4", "   5: hit 5"],
        );
        assert!(model.rows[3].selected);
    }

    #[test]
    fn trim_preview_centers_match_when_line_overflows_budget() {
        use led_state_file_search::FileSearchHit;
        let path = canon("a.rs");
        // 28-char line, "needle" (6 chars) starts at col 18
        // (char idx 17). With a 12-char budget the centering
        // window picks up `needle` plus three chars of context
        // on each side: matches legacy
        // `display.rs::file_search_hit_spans` (`context_before
        // = (avail - match_len) / 2`).
        let hit = FileSearchHit {
            path: path.clone(),
            line: 42,
            col: 18,
            preview: "aaaabbbbccccdddd_needle_xxxx".into(),
            match_start: 17,
            match_end: 23,
        };
        assert_eq!(trim_preview_at_budget(&hit, 12), "dd_needle_xx");
    }

    #[test]
    fn trim_preview_is_a_noop_when_line_fits_in_the_budget() {
        use led_state_file_search::FileSearchHit;
        let path = canon("a.rs");
        // "  needle at start" is 17 chars; with a 24-char
        // budget it fits whole, so the preview is returned
        // untouched (no center, no ellipsis).
        let hit = FileSearchHit {
            path: path.clone(),
            line: 1,
            col: 3,
            preview: "  needle at start".into(),
            match_start: 2,
            match_end: 8,
        };
        assert_eq!(trim_preview_at_budget(&hit, 24), "  needle at start");
    }

    #[test]
    fn hit_row_carries_match_range_covering_the_query() {
        // Short line, match at col 5 for a 3-char query. Row name
        // = "   42: aaaabbb". Prefix = 3 + 2 + 2 = 7 chars. Match
        // starts at char 5-1=4 in the preview (no trim), so
        // match_range = (7+4, 7+4+3) = (11, 14).
        use led_state_file_search::{FileSearchGroup, FileSearchHit};

        let path = canon("a.rs");
        let hit = FileSearchHit {
            path: path.clone(),
            line: 42,
            col: 5,
            preview: "aaaabbbcccc".into(),
            match_start: 4,
            match_end: 7, // 3-char match ("bbb")
        };
        let state = led_state_file_search::FileSearchState {
            results: vec![FileSearchGroup {
                path: path.clone(),
                relative: "a.rs".into(),
                hits: vec![hit.clone()],
            }],
            flat_hits: vec![hit],
            selection: led_state_file_search::FileSearchSelection::SearchInput,
            ..Default::default()
        };
        let model = file_search_side_panel(&state, 20);
        // Row 0 = header, row 1 = query, row 2 = group header, row 3 = hit.
        let hit_row = &model.rows[3];
        assert_eq!(&*hit_row.name, "   42: aaaabbbcccc");
        assert_eq!(hit_row.match_range, Some((11, 14)));
    }

    #[test]
    fn hit_row_match_range_tracks_through_the_centered_window() {
        // Long line: "aaaabbbbccccdddd_needle_xxxx" — "needle"
        // (6 chars) starts at char 17, col=18 (1-indexed). Side
        // panel content cols = 24, prefix `   1: ` = 6 chars,
        // preview budget = 18. Centering picks the rightmost
        // 18-char window that contains the match: chars[10..28]
        // = "ccdddd_needle_xxxx". Match offset in the window =
        // 17 - 10 = 7, so the row's match_range =
        // (6 + 7, 6 + 7 + 6) = (13, 19), and the chars at
        // that range spell `needle`.
        use led_state_file_search::{FileSearchGroup, FileSearchHit};

        let path = canon("a.rs");
        let hit = FileSearchHit {
            path: path.clone(),
            line: 1,
            col: 18,
            preview: "aaaabbbbccccdddd_needle_xxxx".into(),
            match_start: 17,
            match_end: 23,
        };
        let state = led_state_file_search::FileSearchState {
            results: vec![FileSearchGroup {
                path: path.clone(),
                relative: "a.rs".into(),
                hits: vec![hit.clone()],
            }],
            flat_hits: vec![hit],
            selection: led_state_file_search::FileSearchSelection::SearchInput,
            ..Default::default()
        };
        let model = file_search_side_panel(&state, 20);
        let hit_row = &model.rows[3];
        assert_eq!(hit_row.match_range, Some((13, 19)));
        // The chars at the computed range spell out "needle".
        let chars: Vec<char> = hit_row.name.chars().collect();
        let (s, e) = hit_row.match_range.unwrap();
        let slice: String = chars[s as usize..e as usize].iter().collect();
        assert_eq!(slice, "needle");
    }

    #[test]
    fn trim_preview_handles_multibyte_chars_via_col_count() {
        use led_state_file_search::FileSearchHit;
        let path = canon("a.rs");
        // "🎈🎈🎈🎈🎈 needle" — five balloons (1 char each, 4 bytes
        // each in UTF-8), a space, then "needle" starting at char
        // index 6 (col=7 1-indexed). 12-char preview, 8-char
        // budget → centering window keeps `needle` visible while
        // dropping balloons from the left.
        let hit = FileSearchHit {
            path,
            line: 1,
            col: 7,
            preview: "🎈🎈🎈🎈🎈 needle".into(),
            match_start: "🎈🎈🎈🎈🎈 ".len(),
            match_end: "🎈🎈🎈🎈🎈 needle".len(),
        };
        let trimmed = trim_preview_at_budget(&hit, 8);
        assert!(trimmed.contains("needle"), "got {trimmed:?}");
        assert_eq!(trimmed.chars().count(), 8);
    }

    // ── Syntax span projection ───────────────────────────────────────

    #[test]
    fn tokens_to_line_spans_slices_on_a_single_line() {
        // Rope: "fn main\n"  → line 0 starts at char 0, length 7.
        // Pretend "fn" is a keyword (chars 0..2), "main" is a
        // function (chars 3..7).
        let tokens = vec![
            TokenSpan {
                char_start: 0,
                char_end: 2,
                kind: TokenKind::Keyword,
            },
            TokenSpan {
                char_start: 3,
                char_end: 7,
                kind: TokenKind::Function,
            },
        ];
        let spans = tokens_to_line_spans(&tokens, /* line_char_start */ 0, /* line_char_len */ 7, /* content_cols */ 40);
        // GUTTER_WIDTH is 2 — columns shift right by 2.
        assert_eq!(
            spans,
            vec![
                led_driver_terminal_core::LineSpan {
                    col_start: 2,
                    col_end: 4,
                    kind: TokenKind::Keyword,
                },
                led_driver_terminal_core::LineSpan {
                    col_start: 5,
                    col_end: 9,
                    kind: TokenKind::Function,
                },
            ]
        );
    }

    #[test]
    fn tokens_to_line_spans_clips_spans_crossing_line_boundaries() {
        // Single token `char_start=3, char_end=9`; line starts at 5
        // and has length 10. The [3, 9) overlap with [5, 15) is
        // [5, 9) → rel_start=0, rel_end=4.
        let tokens = vec![TokenSpan {
            char_start: 3,
            char_end: 9,
            kind: TokenKind::String,
        }];
        let spans = tokens_to_line_spans(&tokens, 5, 10, 40);
        assert_eq!(
            spans,
            vec![led_driver_terminal_core::LineSpan {
                col_start: 2,
                col_end: 6,
                kind: TokenKind::String,
            }]
        );
    }

    #[test]
    fn tokens_to_line_spans_drops_default_kind() {
        let tokens = vec![
            TokenSpan {
                char_start: 0,
                char_end: 5,
                kind: TokenKind::Default,
            },
            TokenSpan {
                char_start: 5,
                char_end: 10,
                kind: TokenKind::Keyword,
            },
        ];
        let spans = tokens_to_line_spans(&tokens, 0, 10, 40);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, TokenKind::Keyword);
    }

    #[test]
    fn tokens_to_line_spans_clamps_to_content_cols() {
        // Span extends past the truncated row — clamp col_end.
        let tokens = vec![TokenSpan {
            char_start: 0,
            char_end: 20,
            kind: TokenKind::Comment,
        }];
        // line_char_len = 20 but content_cols = 5 → clip to col 5 + gutter.
        let spans = tokens_to_line_spans(&tokens, 0, 20, 5);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].col_end, (5 + GUTTER_WIDTH) as u16);
    }

    // ── LSP status formatter (matches legacy's
    // `format_lsp_status` at /crates/ui/src/display.rs:803) ──

    #[test]
    fn format_lsp_status_empty_server_returns_empty() {
        assert_eq!(format_lsp_status("", true, Some("indexing"), 0), "");
    }

    #[test]
    fn format_lsp_status_busy_no_detail_shows_spinner_and_name() {
        // Tick=0 → frame 0 = '⠋'. Two leading spaces + spinner + space + name.
        assert_eq!(format_lsp_status("rust-analyzer", true, None, 0), "  ⠋ rust-analyzer");
    }

    #[test]
    fn format_lsp_status_busy_with_detail_has_two_spinners() {
        // Tick=0 → main spinner frame 0 = '⠋'. Detail spinner
        // offset by 5 (≈ 400ms out of phase) → frame 5 = '⠴'.
        // Separator: two spaces between name and detail.
        let s = format_lsp_status("rust-analyzer", true, Some("indexing crates"), 0);
        assert_eq!(s, "  ⠋ rust-analyzer  ⠴ indexing crates");
    }

    #[test]
    fn format_lsp_status_idle_with_detail_omits_spinners() {
        let s = format_lsp_status("rust-analyzer", false, Some("indexing crates"), 0);
        assert_eq!(s, "  rust-analyzer  indexing crates");
    }

    #[test]
    fn format_lsp_status_idle_no_detail_just_name() {
        let s = format_lsp_status("rust-analyzer", false, None, 0);
        assert_eq!(s, "  rust-analyzer");
    }

    #[test]
    fn format_lsp_status_empty_detail_treated_as_none() {
        // Legacy's `.filter(|d| !d.is_empty())` drops empty detail.
        let s = format_lsp_status("rust-analyzer", true, Some(""), 0);
        assert_eq!(s, "  ⠋ rust-analyzer");
    }

    // ── file_categories_map / browser row status ──────────

    #[test]
    fn file_categories_map_emits_lsp_error_and_warning_only() {
        let mut diags = DiagnosticsStates::default();
        let mut items = Vec::new();
        items.push(Diagnostic {
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 3,
            severity: DiagnosticSeverity::Error,
            message: String::new(),
            source: None,
            code: None,
        });
        items.push(Diagnostic {
            start_line: 1,
            start_col: 0,
            end_line: 1,
            end_col: 3,
            severity: DiagnosticSeverity::Warning,
            message: String::new(),
            source: None,
            code: None,
        });
        items.push(Diagnostic {
            start_line: 2,
            start_col: 0,
            end_line: 2,
            end_col: 3,
            severity: DiagnosticSeverity::Info,
            message: String::new(),
            source: None,
            code: None,
        });
        items.push(Diagnostic {
            start_line: 3,
            start_col: 0,
            end_line: 3,
            end_col: 3,
            severity: DiagnosticSeverity::Hint,
            message: String::new(),
            source: None,
            code: None,
        });
        diags.by_path.insert(
            canon("/p/a.rs"),
            BufferDiagnostics::new(
                led_core::PersistedContentHash(1),
                items,
            ),
        );
        let git = led_state_git::GitState::default();
        let map = file_categories_map(
            DiagnosticsStatesInput::new(&diags),
            GitStateInput::new(&git),
        );
        let cats = map.get(&canon("/p/a.rs")).expect("entry");
        assert!(cats.contains(&led_core::IssueCategory::LspError));
        assert!(cats.contains(&led_core::IssueCategory::LspWarning));
        // Info / Hint MUST NOT make it into the map.
        assert_eq!(cats.len(), 2, "only Error + Warning colour the browser");
    }

    #[test]
    fn file_categories_map_empty_when_no_diagnostics() {
        let diags = DiagnosticsStates::default();
        let git = led_state_git::GitState::default();
        let map = file_categories_map(
            DiagnosticsStatesInput::new(&diags),
            GitStateInput::new(&git),
        );
        assert!(map.is_empty());
    }

    #[test]
    fn file_categories_map_merges_git_and_lsp() {
        // Same file carries both an LSP error and a git Unstaged
        // category. `resolve_display` should pick LspError by
        // precedence (LspError < Unstaged in numeric value).
        let p = canon("/p/a.rs");
        let mut diags = DiagnosticsStates::default();
        diags.by_path.insert(
            p.clone(),
            BufferDiagnostics::new(
                led_core::PersistedContentHash(1),
                vec![Diagnostic {
                    start_line: 0,
                    start_col: 0,
                    end_line: 0,
                    end_col: 3,
                    severity: DiagnosticSeverity::Error,
                    message: String::new(),
                    source: None,
                    code: None,
                }],
            ),
        );
        let mut git = led_state_git::GitState::default();
        let mut cats_for_file = imbl::HashSet::default();
        cats_for_file.insert(led_core::IssueCategory::Unstaged);
        git.file_statuses.insert(p.clone(), cats_for_file);

        let map = file_categories_map(
            DiagnosticsStatesInput::new(&diags),
            GitStateInput::new(&git),
        );
        let cats = map.get(&p).expect("merged");
        assert!(cats.contains(&led_core::IssueCategory::LspError));
        assert!(cats.contains(&led_core::IssueCategory::Unstaged));
        // `resolve_display` selects the precedence-winning category
        // (LspError) even though both are present.
        let shown = led_core::resolve_display(cats).expect("some");
        assert_eq!(shown.category, led_core::IssueCategory::LspError);
    }

    #[test]
    fn file_categories_map_includes_git_only_file() {
        // Untracked file with no diagnostics — carries through.
        let p = canon("/p/new.rs");
        let diags = DiagnosticsStates::default();
        let mut git = led_state_git::GitState::default();
        let mut cats_for_file = imbl::HashSet::default();
        cats_for_file.insert(led_core::IssueCategory::Untracked);
        git.file_statuses.insert(p.clone(), cats_for_file);

        let map = file_categories_map(
            DiagnosticsStatesInput::new(&diags),
            GitStateInput::new(&git),
        );
        let cats = map.get(&p).expect("git-only file present");
        assert!(cats.contains(&led_core::IssueCategory::Untracked));
    }

    #[test]
    fn side_panel_row_status_marks_error_file_and_parent_dir() {
        use imbl::Vector;
        use led_state_browser::{BrowserUi, DirEntry, DirEntryKind, FsTree};
        // Tree: /p/sub/err.rs (with LspError), /p/sub/ok.rs (clean).
        let mut fs = FsTree {
            root: Some(canon("/p")),
            ..Default::default()
        };
        let mut root_kids = Vector::new();
        root_kids.push_back(DirEntry {
            name: "sub".into(),
            path: canon("/p/sub"),
            kind: DirEntryKind::Directory,
        });
        fs.dir_contents.insert(canon("/p"), root_kids);
        let mut sub_kids = Vector::new();
        sub_kids.push_back(DirEntry {
            name: "err.rs".into(),
            path: canon("/p/sub/err.rs"),
            kind: DirEntryKind::File,
        });
        sub_kids.push_back(DirEntry {
            name: "ok.rs".into(),
            path: canon("/p/sub/ok.rs"),
            kind: DirEntryKind::File,
        });
        fs.dir_contents.insert(canon("/p/sub"), sub_kids);

        let mut browser = BrowserUi::default();
        browser.expanded_dirs.insert(canon("/p/sub"));

        let mut diags = DiagnosticsStates::default();
        diags.by_path.insert(
            canon("/p/sub/err.rs"),
            BufferDiagnostics::new(
                led_core::PersistedContentHash(1),
                vec![Diagnostic {
                    start_line: 0,
                    start_col: 0,
                    end_line: 0,
                    end_col: 3,
                    severity: DiagnosticSeverity::Error,
                    message: String::new(),
                    source: None,
                    code: None,
                }],
            ),
        );

        let tabs = Tabs::default();
        let ff = None;
        let is = None;
        let fsrch = None;
        let git = led_state_git::GitState::default();
        let edits = led_state_buffer_edits::BufferEdits::default();
        let panel = side_panel_model(SidePanelInputs {
            fs: FsTreeInput::new(&fs),
            browser: BrowserUiInput::new(&browser),
            overlays: OverlaysInput::new(&ff, &is, &fsrch),
            tabs: TabsActiveInput::new(&tabs),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            git: GitStateInput::new(&git),
            edits: EditedBuffersInput::new(&edits),
            rows: 10,
        });
        let rows: &Vec<SidePanelRow> = &panel.rows;
        let by_name = |name: &str| rows.iter().find(|r| &*r.name == name).cloned();
        // `/p/sub` (directory) — aggregates LspError from descendant.
        let sub = by_name("sub").expect("sub row");
        let sub_status = sub.status.expect("sub row inherits descendant error");
        assert_eq!(sub_status.category, led_core::IssueCategory::LspError);
        assert_eq!(sub_status.letter, '\u{2022}'); // directories always bullet
        // `err.rs` (file) — direct LspError, bullet letter (Error
        // has no `browser_letter`).
        let err = by_name("err.rs").expect("err row");
        let err_status = err.status.expect("err file has status");
        assert_eq!(err_status.category, led_core::IssueCategory::LspError);
        assert_eq!(err_status.letter, '\u{2022}');
        // `ok.rs` (file) — no diagnostic → no status.
        let ok = by_name("ok.rs").expect("ok row");
        assert!(ok.status.is_none(), "clean file has no status");
    }

    #[test]
    fn lsp_progress_message_persists_server_name_while_idle() {
        // Regression: an idle server with no progress detail
        // must still surface its name. Legacy shows
        // "  rust-analyzer" on the status bar for the entire
        // lifetime of the server — busy/idle just toggles the
        // spinner and detail *around* the name. A previous
        // incarnation of this function returned `None` here,
        // making "rust-analyzer" disappear the instant
        // indexing finished.
        let mut lsp = LspStatuses::default();
        lsp.by_server.insert(
            "rust-analyzer".into(),
            LspServerStatus {
                busy: false,
                detail: None,
                ready: true,
            },
        );
        let msg = lsp_progress_message(LspStatusesInput::new(&lsp), 0)
            .expect("server visible while idle");
        assert!(msg.contains("rust-analyzer"), "got: {msg:?}");
    }

    #[test]
    fn format_lsp_status_spinner_advances_with_tick() {
        // Each tick bucket (80ms) advances the main spinner by
        // one frame in the 10-frame cycle.
        let t0 = format_lsp_status("ra", true, None, 0);
        let t1 = format_lsp_status("ra", true, None, 1);
        let t2 = format_lsp_status("ra", true, None, 2);
        assert_ne!(t0, t1);
        assert_ne!(t1, t2);
        // After 10 buckets the cycle wraps.
        assert_eq!(t0, format_lsp_status("ra", true, None, 10));
    }

    // ── popover_model ────────────────────────────────────────

    fn popover_fixture(
        cursor_line: usize,
        diag_start_line: usize,
        diag_end_line: usize,
        severity: DiagnosticSeverity,
        message: &str,
        buf_version: u64,
        // `false` stamps the diagnostic with the buffer's actual
        // content hash (popover-visible); `true` stamps with a
        // deliberately-wrong hash so the no-smear gate hides it.
        diag_hash_mismatches: bool,
    ) -> (
        Tabs,
        BufferEdits,
        BrowserUi,
        DiagnosticsStates,
        Option<led_state_find_file::FindFileState>,
        Option<led_state_isearch::IsearchState>,
        Option<led_state_file_search::FileSearchState>,
    ) {
        let path = canon("a.rs");
        let mut t = Tabs::default();
        t.open.push_back(Tab {
            id: TabId(1),
            path: path.clone(),
            cursor: Cursor {
                line: cursor_line,
                col: 0,
                preferred_col: 0,
            },
            ..Default::default()
        });
        t.active = Some(TabId(1));

        let rope = Arc::new(Rope::from_str("line\n"));
        let buf_hash = led_core::EphemeralContentHash::of_rope(&rope).persist();
        let mut e = BufferEdits::default();
        let mut eb = led_state_buffer_edits::EditedBuffer::fresh(rope);
        eb.version = buf_version;
        e.buffers.insert(path.clone(), eb);

        let browser = BrowserUi {
            visible: false,
            ..Default::default()
        };

        let diag_hash = if diag_hash_mismatches {
            // Force a deliberate mismatch by xor'ing the low bit.
            led_core::PersistedContentHash(buf_hash.0 ^ 1)
        } else {
            buf_hash
        };

        let mut diags = DiagnosticsStates::default();
        diags.by_path.insert(
            path,
            BufferDiagnostics::new(
                diag_hash,
                vec![Diagnostic {
                    start_line: diag_start_line,
                    start_col: 0,
                    end_line: diag_end_line,
                    end_col: 5,
                    severity,
                    message: message.to_string(),
                    source: None,
                    code: None,
                }],
            ),
        );

        (t, e, browser, diags, None, None, None)
    }

    fn call_popover(
        t: &Tabs,
        e: &BufferEdits,
        browser: &BrowserUi,
        diags: &DiagnosticsStates,
        ff: &Option<led_state_find_file::FindFileState>,
        is: &Option<led_state_isearch::IsearchState>,
        fs: &Option<led_state_file_search::FileSearchState>,
    ) -> Option<PopoverModel> {
        popover_model(
            EditedBuffersInput::new(e),
            TabsActiveInput::new(t),
            OverlaysInput::new(ff, is, fs),
            BrowserUiInput::new(browser),
            DiagnosticsStatesInput::new(diags),
            Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 24,
            },
        )
    }

    #[test]
    fn popover_shows_for_error_on_cursor_row() {
        let (t, e, br, d, ff, is, fs) = popover_fixture(
            3,
            3,
            3,
            DiagnosticSeverity::Error,
            "expected `;`",
            0,
            false,
        );
        let pop = call_popover(&t, &e, &br, &d, &ff, &is, &fs).expect("popover");
        assert_eq!(pop.lines.len(), 1);
        assert_eq!(&*pop.lines[0].text, "expected `;`");
        assert_eq!(pop.lines[0].severity, Some(PopoverSeverity::Error));
    }

    #[test]
    fn popover_hidden_when_cursor_above_diagnostic_range() {
        let (t, e, br, d, ff, is, fs) = popover_fixture(
            1,
            3,
            3,
            DiagnosticSeverity::Error,
            "x",
            0,
            false,
        );
        assert!(call_popover(&t, &e, &br, &d, &ff, &is, &fs).is_none());
    }

    #[test]
    fn popover_hidden_when_hash_stale_no_smear() {
        // Diagnostic stamped with a content hash that doesn't
        // match the buffer's current hash — hide rather than
        // show stale.
        let (t, e, br, d, ff, is, fs) = popover_fixture(
            3,
            3,
            3,
            DiagnosticSeverity::Error,
            "x",
            2,
            true,
        );
        assert!(call_popover(&t, &e, &br, &d, &ff, &is, &fs).is_none());
    }

    #[test]
    fn popover_hidden_for_info_and_hint_severity() {
        for sev in [DiagnosticSeverity::Info, DiagnosticSeverity::Hint] {
            let (t, e, br, d, ff, is, fs) =
                popover_fixture(3, 3, 3, sev, "x", 0, false);
            assert!(
                call_popover(&t, &e, &br, &d, &ff, &is, &fs).is_none(),
                "severity {sev:?} must be silent"
            );
        }
    }

    #[test]
    fn popover_hidden_when_find_file_overlay_active() {
        let (t, e, br, d, _, is, fs) = popover_fixture(
            3,
            3,
            3,
            DiagnosticSeverity::Error,
            "x",
            0,
            false,
        );
        let ff = Some(led_state_find_file::FindFileState::open(String::new()));
        assert!(call_popover(&t, &e, &br, &d, &ff, &is, &fs).is_none());
    }

    #[test]
    fn popover_hidden_when_browser_focused() {
        let (t, e, mut br, d, ff, is, fs) = popover_fixture(
            3,
            3,
            3,
            DiagnosticSeverity::Error,
            "x",
            0,
            false,
        );
        br.focus = Focus::Side;
        assert!(call_popover(&t, &e, &br, &d, &ff, &is, &fs).is_none());
    }

    #[test]
    fn popover_wraps_long_message_into_multiple_lines() {
        let msg = "this is a long diagnostic message that should wrap across several lines when rendered in the popover box";
        let (t, e, br, d, ff, is, fs) = popover_fixture(
            3,
            3,
            3,
            DiagnosticSeverity::Error,
            msg,
            0,
            false,
        );
        let pop = call_popover(&t, &e, &br, &d, &ff, &is, &fs).expect("popover");
        assert!(pop.lines.len() >= 2, "wrap produces multiple lines");
    }

    #[test]
    fn popover_shows_when_cursor_on_middle_of_multiline_diagnostic() {
        // Diagnostic spans rows 3..=5; cursor on row 4 must still
        // produce a popover.
        let (t, e, br, d, ff, is, fs) = popover_fixture(
            4,
            3,
            5,
            DiagnosticSeverity::Warning,
            "spans three lines",
            0,
            false,
        );
        let pop = call_popover(&t, &e, &br, &d, &ff, &is, &fs).expect("popover");
        assert_eq!(pop.lines[0].severity, Some(PopoverSeverity::Warning));
    }
}
