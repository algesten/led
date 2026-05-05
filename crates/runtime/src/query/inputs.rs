//! Input projections for cross-source query memos.
//!
//! Every `*Input` newtype wrapper that projects a slice of a driver
//! source for consumption by a memo lives here. Each carries a
//! `new(&source)` constructor the call site uses to project.

#[allow(unused_imports)]
use led_core::{BufferStateSum, BufferVersion, CanonPath, SavedVersion, ServerId};
use led_driver_buffers_core::{BufferStore, LoadState};
use led_driver_file_watch_core::{
    FileWatchEvent, FileWatchState, Registration, WatchSeq,
};
use led_state_lsp::LspWatchedGlobs;
use led_driver_terminal_core::{
    Dims, Terminal,
};
use led_state_alerts::AlertState;
use led_state_kbd_macro::KbdMacroState;
use led_state_session::SessionState;
use led_state_browser::{BrowserUi, Focus};
use led_state_clipboard::ClipboardState;
use led_state_buffer_edits::{BufferEdits, EditedBuffer};
use led_state_diagnostics::{
    BufferDiagnostics, DiagnosticsStates, LspServerStatus,
    LspStatuses,
};
use led_state_syntax::{SyntaxState, SyntaxStates};
use led_state_tabs::{Tab, TabId, Tabs};
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

// ── Inputs on KbdMacroState ────────────────────────────────────────────

/// Narrow projection over `KbdMacroState` for the status-bar
/// recording indicator (M22). Exposes only `recording`, so
/// pushes into `KbdMacroState.current` during a recording
/// session don't invalidate the status-bar memo on every
/// captured keystroke. Matches the EXAMPLE-ARCH guideline
/// "project only the fields the memo actually reads."
#[derive(drv::Input, Copy, Clone)]
pub struct KbdMacroRecordingInput<'a> {
    pub recording: &'a bool,
}

impl<'a> KbdMacroRecordingInput<'a> {
    pub fn new(s: &'a KbdMacroState) -> Self {
        Self {
            recording: &s.recording,
        }
    }
}

// ── Inputs on SessionState ────────────────────────────────────────────

/// Narrow projection over `SessionState` for the status-bar
/// "(secondary)" indicator. Exposes only `primary` and
/// `init_done` — `init_done` gates the indicator so we don't flash
/// "(secondary)" during the brief startup window before `Restored`
/// arrives and sets the real flock outcome. The save/restore-state
/// fields (`saved`, `last_saved`, `pending_undo`) churn and would
/// invalidate the status-bar memo for no reason.
#[derive(drv::Input, Copy, Clone)]
pub struct SessionPrimaryInput<'a> {
    pub primary: &'a bool,
    pub init_done: &'a bool,
}

impl<'a> SessionPrimaryInput<'a> {
    pub fn new(s: &'a SessionState) -> Self {
        Self {
            primary: &s.primary,
            init_done: &s.init_done,
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
    pub by_server: &'a imbl::HashMap<ServerId, LspServerStatus>,
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
        &'a imbl::HashMap<CanonPath, led_state_git::GitLineStatuses>,
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
    pub failed_dirs: &'a imbl::HashSet<CanonPath>,
}

impl<'a> FsTreeInput<'a> {
    pub fn new(fs: &'a led_state_browser::FsTree) -> Self {
        Self {
            root: &fs.root,
            dir_contents: &fs.dir_contents,
            failed_dirs: &fs.failed_dirs,
        }
    }
}

/// Workspace root only. A change to `dir_contents` (every fs-list
/// reply) must NOT invalidate memos that only care about the root
/// path — give them their own narrow projection.
#[derive(drv::Input, Copy, Clone)]
pub struct FsRootInput<'a> {
    pub root: &'a Option<CanonPath>,
}

impl<'a> FsRootInput<'a> {
    pub fn new(fs: &'a led_state_browser::FsTree) -> Self {
        Self { root: &fs.root }
    }
}

/// `<config>/notify/` resolved at startup. Not a source-backed
/// value; the runtime's `run` loop computes it once and projects
/// the local reference. Stable address across the loop lifetime
/// makes drv's pointer-eq cache hit on idle ticks.
#[derive(drv::Input, Copy, Clone)]
pub struct NotifyDirInput<'a> {
    pub notify_dir: &'a Option<CanonPath>,
}

impl<'a> NotifyDirInput<'a> {
    pub fn new(notify_dir: &'a Option<CanonPath>) -> Self {
        Self { notify_dir }
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

// ── Input on FileWatchState ────────────────────────────────────────────

/// Per-tick fan-out queue. The driver writes here during ingest;
/// the runtime drains via memos during query.
#[derive(drv::Input, Copy, Clone)]
pub struct FileWatchEventsInput<'a> {
    pub recent_events:
        &'a imbl::HashMap<WatchSeq, imbl::Vector<FileWatchEvent>>,
}

impl<'a> FileWatchEventsInput<'a> {
    pub fn new(s: &'a FileWatchState) -> Self {
        Self {
            recent_events: &s.recent_events,
        }
    }
}

/// Existing watch registrations. Read by the desired/actual diff.
#[derive(drv::Input, Copy, Clone)]
pub struct FileWatchRegistryInput<'a> {
    pub registry: &'a imbl::HashMap<WatchSeq, Registration>,
}

impl<'a> FileWatchRegistryInput<'a> {
    pub fn new(s: &'a FileWatchState) -> Self {
        Self {
            registry: &s.registry,
        }
    }
}

/// "Now" as memo input. Per EXAMPLE-ARCH "Time is a source
/// field": the runtime writes `clock.now = Instant::now()` once
/// per ingest tick, and time-dependent memos take this input so
/// their cache invalidates whenever a tick's clock advances.
#[derive(drv::Input, Copy, Clone)]
pub struct ClockInput<'a> {
    pub now: &'a std::time::Instant,
}

impl<'a> ClockInput<'a> {
    pub fn new(c: &'a crate::Clock) -> Self {
        Self { now: &c.now }
    }
}

/// Narrow projection of [`AlertState`] for deadline memos. The
/// painter's `AlertsInput` deliberately excludes
/// `info_expires_at` (it churns by the millisecond and would
/// invalidate the status-bar cache); deadlines need it.
#[derive(drv::Input, Copy, Clone)]
pub struct AlertExpiryInput<'a> {
    pub info_expires_at: &'a Option<std::time::Instant>,
}

impl<'a> AlertExpiryInput<'a> {
    pub fn new(a: &'a AlertState) -> Self {
        Self {
            info_expires_at: &a.info_expires_at,
        }
    }
}

/// Per-buffer undo-flush debounce timestamps. The deadline memo
/// folds them into the earliest pending flush time so the runner
/// wakes when one expires.
#[derive(drv::Input, Copy, Clone)]
pub struct UndoFlushDebounceInput<'a> {
    pub entries: &'a imbl::HashMap<CanonPath, crate::UndoFlushDebounce>,
}

impl<'a> UndoFlushDebounceInput<'a> {
    pub fn new(
        m: &'a imbl::HashMap<CanonPath, crate::UndoFlushDebounce>,
    ) -> Self {
        Self { entries: m }
    }
}

/// Narrow projection of [`LspExtrasState`] for the inlay-hints
/// memo — just the toggle bit. A change to other extras (rename,
/// code-actions) doesn't need to invalidate the request memo.
#[derive(drv::Input, Copy, Clone)]
pub struct LspInlayHintsEnabledInput<'a> {
    pub enabled: &'a bool,
}

impl<'a> LspInlayHintsEnabledInput<'a> {
    pub fn new(s: &'a led_state_lsp::LspExtrasState) -> Self {
        Self {
            enabled: &s.inlay_hints_enabled,
        }
    }
}

/// Narrow projection of [`LspPending`] for the inlay-hints memo —
/// just the requested-set. Mutations on other LspPending fields
/// (rename outboxes etc.) shouldn't invalidate the inlay-hints
/// memo.
#[derive(drv::Input, Copy, Clone)]
pub struct LspInlayHintsRequestedInput<'a> {
    pub by_path: &'a imbl::HashMap<CanonPath, BufferVersion>,
}

impl<'a> LspInlayHintsRequestedInput<'a> {
    pub fn new(s: &'a led_state_lsp::LspPending) -> Self {
        Self {
            by_path: &s.inlay_hints_requested,
        }
    }
}

// ── Input on LspWatchedGlobs ──────────────────────────────────────────

/// Per-server registered glob projection. Read by the
/// `lsp_watched_file_notifications` memo to fan watch events out
/// to language servers.
#[derive(drv::Input, Copy, Clone)]
pub struct LspWatchedGlobsInput<'a> {
    pub by_server: &'a imbl::HashMap<
        ServerId,
        imbl::HashMap<String, Arc<Vec<led_driver_lsp_core::RegistrationGlob>>>,
    >,
}

impl<'a> LspWatchedGlobsInput<'a> {
    pub fn new(g: &'a LspWatchedGlobs) -> Self {
        Self {
            by_server: &g.by_server,
        }
    }
}

/// Per-buffer record of the last `(version, saved_version)` pair
/// pushed to the LSP driver. Read by the LSP buffer-changed memo
/// to decide what's stale.
#[derive(drv::Input, Copy, Clone)]
pub struct LspNotifiedInput<'a> {
    pub by_path: &'a imbl::HashMap<CanonPath, crate::LspNotified>,
}

impl<'a> LspNotifiedInput<'a> {
    pub fn new(m: &'a imbl::HashMap<CanonPath, crate::LspNotified>) -> Self {
        Self { by_path: m }
    }
}

/// Projection over `undo_persistence` so the sync-check memo can
/// read trackers without depending on the full HashMap.
#[derive(drv::Input, Copy, Clone)]
pub struct UndoPersistenceInput<'a> {
    pub by_path:
        &'a imbl::HashMap<CanonPath, crate::UndoPersistTracker>,
}

impl<'a> UndoPersistenceInput<'a> {
    pub fn new(
        m: &'a imbl::HashMap<CanonPath, crate::UndoPersistTracker>,
    ) -> Self {
        Self { by_path: m }
    }
}

/// Wraps a `notify_hash_index` result so it can pass back into
/// another memo as a `drv::Input`. Without this the consumer
/// would need to project an `Arc<HashMap>` directly, which the
/// projection trait doesn't grant pointer-eq on.
#[derive(drv::Input, Copy, Clone)]
pub struct HashIndexInput<'a> {
    pub by_path: &'a std::collections::HashMap<String, CanonPath>,
}

impl<'a> HashIndexInput<'a> {
    pub fn new(
        m: &'a std::collections::HashMap<String, CanonPath>,
    ) -> Self {
        Self { by_path: m }
    }
}
