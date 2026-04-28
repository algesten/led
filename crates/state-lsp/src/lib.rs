//! M18 LSP-extras state — split into two sources per arch
//! guideline 1 ("external facts and user decisions go in
//! separate sources"):
//!
//! - [`LspExtrasState`] — *user decisions*: which overlay is
//!   open (rename / code-actions), the inlay-hints toggle.
//!   Mutated by dispatch from key events; survives reconnects.
//! - [`LspPending`] — *driver bookkeeping*: outboxes the
//!   runtime drains into `LspCmd::*` plus the per-request
//!   `latest_*_seq` gates and the per-buffer inlay-hint cache.
//!   Mutated by ingest from `LspEvent::*` and by dispatch
//!   `queue_*` helpers; rebuilt on reconnect.
//!
//! Splitting them keeps memos that read overlay state from
//! recomputing on every queued LSP request, and lets tests
//! poke pending vectors without instantiating overlay state.

use std::sync::Arc;

use led_core::{BufferVersion, CanonPath, LspRequestSeq, ServerId, TextInput};
use led_driver_lsp_core::{CodeActionSummary, InlayHint, RegistrationGlob};

// ── User-decision source ──────────────────────────────────────

/// Overlay state + the inlay-hints user toggle. Plain user
/// decisions; nothing here gets mutated by ingest.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LspExtrasState {
    /// Active rename overlay (None when no rename is in flight).
    /// Mutually exclusive with completions / find-file /
    /// code-actions — at most one overlay owns editor input at
    /// any time.
    pub rename: Option<RenameState>,

    /// Active code-action picker overlay. Populated when an
    /// `LspEvent::CodeActions` delivery arrives with at least
    /// one item; cleared on commit / abort / stale-seq drop.
    pub code_actions: Option<CodeActionPickerState>,

    /// `Ctrl-t` toggle. When `false`, no inlay-hint requests
    /// fire and the painter doesn't draw hints even when the
    /// cache holds some (users expect the toggle to actually
    /// turn them off visually).
    pub inlay_hints_enabled: bool,
}

impl LspExtrasState {
    /// Dismiss the rename overlay. Idempotent. Does NOT clear
    /// the latest-rename seq on the pending source — an
    /// in-flight request may still deliver edits the user would
    /// want applied if they aborted out of curiosity; the
    /// runtime gates acceptance by the rename overlay being
    /// `None`, not by the seq.
    pub fn dismiss_rename(&mut self) {
        self.rename = None;
    }

    /// Dismiss the code-action picker. Idempotent.
    pub fn dismiss_code_actions(&mut self) {
        self.code_actions = None;
    }

    /// Flip the inlay-hints toggle. Returns the new value so
    /// the caller can pair this with
    /// `LspPending::clear_inlay_hint_cache` on toggle-off (the
    /// cache lives on the bookkeeping source, so the dispatch
    /// helper composes both calls).
    pub fn toggle_inlay_hints(&mut self) -> bool {
        self.inlay_hints_enabled = !self.inlay_hints_enabled;
        self.inlay_hints_enabled
    }
}

// ── Driver-bookkeeping source ────────────────────────────────

/// Pending-request outboxes + per-server / per-buffer
/// bookkeeping. Mutated by ingest as `LspEvent::*` arrive and
/// by dispatch's `queue_*` helpers; drained by the execute
/// phase.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LspPending {
    /// Monotonic sequence id used by every outbound M18 RPC so
    /// responses can be matched up and stale replies dropped.
    /// Shared across concerns to keep the id space flat and so
    /// round-trip tests don't have to juggle per-concern counters.
    pub seq_gen: LspRequestSeq,

    // ── Goto-definition ────────────────────────────────────────

    /// Pending `textDocument/definition` requests the execute
    /// phase will ship as `LspCmd::RequestGotoDefinition`.
    /// `Vec` rather than `Option` so repeated Alt-Enter presses
    /// don't get dropped — the server responds to each and the
    /// runtime gates by `latest_goto_seq`.
    pub pending_goto: Vec<PendingGoto>,
    /// Latest allocated goto-def seq. Ingest drops any
    /// `LspEvent::GotoDefinition` whose seq doesn't match — the
    /// user has since navigated elsewhere and applying the stale
    /// target would be a jarring jump.
    pub latest_goto_seq: Option<LspRequestSeq>,

    // ── Rename ─────────────────────────────────────────────────

    /// Pending `textDocument/rename` requests queued by
    /// dispatch on Enter-commit. Drained by the execute phase.
    pub pending_rename: Vec<PendingRename>,
    /// Latest allocated rename seq. `LspEvent::Edits {
    /// origin: Rename, seq }` is dropped when `seq` doesn't
    /// match — the user has since aborted or kicked off another
    /// rename and applying the stale edit batch would be confusing.
    pub latest_rename_seq: Option<LspRequestSeq>,

    // ── Code action picker ────────────────────────────────────

    /// Pending `textDocument/codeAction` requests.
    pub pending_code_action: Vec<PendingCodeActionRequest>,
    /// Latest request seq; ingest drops any
    /// `LspEvent::CodeActions` whose seq doesn't match. Not the
    /// same as `latest_code_action_select_seq` — picker install
    /// and commit are distinct round trips.
    pub latest_code_action_seq: Option<LspRequestSeq>,
    /// Pending `codeAction/resolve + apply` selects. Populated
    /// on Enter-commit from the picker overlay; drained by the
    /// execute phase into `LspCmd::SelectCodeAction`.
    pub pending_code_action_select: Vec<PendingCodeActionSelect>,
    /// Latest commit seq; ingest drops any
    /// `LspEvent::Edits { origin: CodeAction }` whose seq
    /// doesn't match. A fresh Alt-i session invalidates any
    /// in-flight commit — legacy parity.
    pub latest_code_action_select_seq: Option<LspRequestSeq>,

    // ── Inlay hints ────────────────────────────────────────────

    /// Pending `textDocument/inlayHint` requests keyed by
    /// path; the execute phase drains these into `LspCmd`.
    pub pending_inlay_hint: Vec<PendingInlayHintRequest>,
    /// Per-buffer inlay-hint cache. `version` is the buffer
    /// version the hints were computed against — a later
    /// edit invalidates the cache by not matching, and a
    /// fresh request fires on the next tick.
    pub inlay_hints_by_path: imbl::HashMap<CanonPath, BufferInlayHints>,
    /// `(path, version)` pairs we've issued requests for.
    /// The execute phase consults this so repeated ticks with
    /// the same version don't spam the server. Cleared on
    /// toggle-off or buffer version bump.
    pub inlay_hints_requested: imbl::HashSet<(CanonPath, BufferVersion)>,

    // ── Format + format-on-save ────────────────────────────────

    /// Pending `textDocument/formatting` requests.
    pub pending_format: Vec<PendingFormat>,
    /// `path`s awaiting a post-format save — when the
    /// corresponding `LspEvent::Edits { origin: Format }`
    /// arrives, we apply the edits then flip the buffer into
    /// `edits.pending_saves`. Separate from `pending_format`
    /// because a plain `LspFormat` command fires format
    /// without wanting a save to follow.
    pub pending_save_after_format: imbl::HashSet<CanonPath>,
    /// Latest format seq per path so stale format responses
    /// drop — a double Ctrl-S in quick succession issues two
    /// formats; only the latest reply unlocks the save.
    pub latest_format_seq: imbl::HashMap<CanonPath, LspRequestSeq>,
}

impl LspPending {
    /// Allocate the next request sequence id.
    pub fn next_seq(&mut self) -> LspRequestSeq {
        self.seq_gen = LspRequestSeq(self.seq_gen.0.wrapping_add(1));
        self.seq_gen
    }

    /// Queue a goto-definition request for the cursor at
    /// `(line, col)` on `path`. Returns the allocated seq so
    /// dispatch tests can assert on it.
    pub fn queue_goto_definition(
        &mut self,
        path: CanonPath,
        line: u32,
        col: u32,
    ) -> LspRequestSeq {
        let seq = self.next_seq();
        self.latest_goto_seq = Some(seq);
        self.pending_goto.push(PendingGoto {
            path,
            seq,
            line,
            col,
        });
        seq
    }

    /// Queue a rename request. Called from dispatch on Enter-
    /// commit — allocates a fresh seq that `latest_rename_seq`
    /// tracks so stale responses can be dropped.
    pub fn queue_rename(
        &mut self,
        path: CanonPath,
        line: u32,
        col: u32,
        new_name: Arc<str>,
    ) -> LspRequestSeq {
        let seq = self.next_seq();
        self.latest_rename_seq = Some(seq);
        self.pending_rename.push(PendingRename {
            path,
            seq,
            line,
            col,
            new_name,
        });
        seq
    }

    /// Queue a code-action request for the selection
    /// `(start..end)` on `path`.
    pub fn queue_code_action(
        &mut self,
        path: CanonPath,
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
    ) -> LspRequestSeq {
        let seq = self.next_seq();
        self.latest_code_action_seq = Some(seq);
        self.pending_code_action.push(PendingCodeActionRequest {
            path,
            seq,
            start_line,
            start_col,
            end_line,
            end_col,
        });
        seq
    }

    /// Queue a code-action commit. Called from the picker
    /// overlay on Enter — the summary carries the `action_id`
    /// the native driver uses to resolve the original
    /// `CodeActionOrCommand`.
    pub fn queue_code_action_select(
        &mut self,
        path: CanonPath,
        action: CodeActionSummary,
    ) -> LspRequestSeq {
        let seq = self.next_seq();
        self.latest_code_action_select_seq = Some(seq);
        self.pending_code_action_select
            .push(PendingCodeActionSelect { path, seq, action });
        seq
    }

    /// Queue a `textDocument/formatting` request. Returns the
    /// allocated seq.
    pub fn queue_format(&mut self, path: CanonPath) -> LspRequestSeq {
        let seq = self.next_seq();
        self.latest_format_seq.insert(path.clone(), seq);
        self.pending_format.push(PendingFormat { path, seq });
        seq
    }

    /// Queue a `textDocument/inlayHint` request.
    pub fn queue_inlay_hints(
        &mut self,
        path: CanonPath,
        version: BufferVersion,
        start_line: u32,
        end_line: u32,
    ) -> LspRequestSeq {
        let seq = self.next_seq();
        self.pending_inlay_hint.push(PendingInlayHintRequest {
            path: path.clone(),
            seq,
            version,
            start_line,
            end_line,
        });
        self.inlay_hints_requested.insert((path, version));
        seq
    }

    /// Toggle-off side effects on the bookkeeping side: drop
    /// the per-buffer cache + the in-flight ledger so the next
    /// toggle-on refetches fresh. Called by the dispatch helper
    /// `toggle_inlay_hints` after it flips
    /// `LspExtrasState::inlay_hints_enabled` to `false`.
    pub fn clear_inlay_hint_cache(&mut self) {
        self.inlay_hints_by_path.clear();
        self.inlay_hints_requested.clear();
    }
}

// ── Value types travelling through both sources ───────────────

/// One queued goto-definition request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingGoto {
    pub path: CanonPath,
    pub seq: LspRequestSeq,
    pub line: u32,
    pub col: u32,
}

/// Editable rename overlay. The user's new-name text lives in
/// `input`; the anchor fields capture where the rename was
/// initiated so the execute phase knows which
/// `textDocument/rename` position to send on commit.
///
/// `seed_word` is the pre-rename identifier the overlay is
/// seeded with (shown in the painter prompt as
/// "Rename: <seed>"). Matches legacy's
/// `RenameState.cursor_orig_word`.
#[derive(Debug, Clone, PartialEq, Eq, drv::Input)]
pub struct RenameState {
    pub input: TextInput,
    pub anchor_path: CanonPath,
    pub anchor_line: u32,
    pub anchor_col: u32,
    pub seed_word: Arc<str>,
}

impl RenameState {
    /// Open a fresh rename overlay. `seed_word` is typed into
    /// `input` as the initial value so Enter with no further
    /// typing is a cheap "no-op rename" (the server still
    /// responds; the runtime applies zero edits).
    pub fn open(
        anchor_path: CanonPath,
        anchor_line: u32,
        anchor_col: u32,
        seed_word: Arc<str>,
    ) -> Self {
        Self {
            input: TextInput::new(seed_word.as_ref()),
            anchor_path,
            anchor_line,
            anchor_col,
            seed_word,
        }
    }
}

/// Queued rename request — the `new_name` is whatever the user
/// had typed into `RenameState.input` at Enter time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRename {
    pub path: CanonPath,
    pub seq: LspRequestSeq,
    pub line: u32,
    pub col: u32,
    pub new_name: Arc<str>,
}

/// Picker overlay for `textDocument/codeAction`. At most one
/// picker is open at a time; `items` is ref-counted so paint +
/// dispatch share without cloning.
#[derive(Debug, Clone, PartialEq, Eq, drv::Input)]
pub struct CodeActionPickerState {
    /// Origin buffer — selection commits route back here on
    /// `SelectCodeAction` so the manager knows which server to
    /// ask.
    pub path: CanonPath,
    /// Seq of the `LspEvent::CodeActions` delivery that
    /// populated this picker. A later delivery with a higher
    /// seq replaces the whole picker wholesale.
    pub seq: LspRequestSeq,
    pub items: Arc<Vec<CodeActionSummary>>,
    pub selected: usize,
    pub scroll: usize,
}

/// Queued `textDocument/codeAction` request. Range covers
/// mark..cursor when a selection is active, else cursor..cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCodeActionRequest {
    pub path: CanonPath,
    pub seq: LspRequestSeq,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

/// Queued commit — the `action` carries the opaque id the
/// native driver uses to look the raw `CodeActionOrCommand`
/// value back up for resolve/apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCodeActionSelect {
    pub path: CanonPath,
    pub seq: LspRequestSeq,
    pub action: CodeActionSummary,
}

/// Queued inlay-hint request for `path` over the visible
/// `(start_line..end_line)` viewport. The response stamps its
/// reply with `version`, so stale arrivals drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInlayHintRequest {
    pub path: CanonPath,
    pub seq: LspRequestSeq,
    pub version: BufferVersion,
    pub start_line: u32,
    pub end_line: u32,
}

/// Per-buffer inlay-hint cache. `version` is the buffer
/// version the hints were computed against; the painter
/// displays them only when `version == buffer.version` (no
/// smear on stale data).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BufferInlayHints {
    pub version: BufferVersion,
    pub hints: Arc<Vec<InlayHint>>,
}

/// Queued `textDocument/formatting` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFormat {
    pub path: CanonPath,
    pub seq: LspRequestSeq,
}

// ── External-fact source: server-registered file-watch globs ──

/// Driver-owned external-fact source: the set of
/// `workspace/didChangeWatchedFiles` glob registrations every
/// language server has installed via `client/registerCapability`.
///
/// Nested map shape: `server → registration_id → Arc<Vec<glob>>`.
/// rust-analyzer typically registers a single id with several
/// globs (`**/Cargo.toml`, `**/*.rs`, …); other servers may
/// register multiple ids over the lifecycle. The runtime ingest
/// arm replaces a registration wholesale on
/// [`LspEvent::WatchedFilesRegistered`] and removes by id on
/// [`LspEvent::WatchedFilesUnregistered`].
///
/// `imbl::HashMap` for both layers keeps idle ticks pointer-equal
/// per G14: the input projection in the runtime's
/// `lsp_watched_file_notifications` memo is a cheap clone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LspWatchedGlobs {
    pub by_server:
        imbl::HashMap<ServerId, imbl::HashMap<String, Arc<Vec<RegistrationGlob>>>>,
}

impl LspWatchedGlobs {
    /// Install or replace one registration id's glob set for
    /// `server`. Same shape regardless of whether the id existed
    /// previously — registrations are immutable per LSP spec; a
    /// re-registration with the same id replaces wholesale.
    pub fn register(
        &mut self,
        server: ServerId,
        registration_id: String,
        globs: Arc<Vec<RegistrationGlob>>,
    ) {
        let entry = self.by_server.entry(server).or_default();
        entry.insert(registration_id, globs);
    }

    /// Drop one registration id from `server`. If the server's
    /// last registration disappears, the per-server entry is
    /// pruned too — keeps `by_server.is_empty()` meaningful.
    pub fn unregister(&mut self, server: &ServerId, registration_id: &str) {
        let now_empty = if let Some(entry) = self.by_server.get_mut(server) {
            entry.remove(registration_id);
            entry.is_empty()
        } else {
            false
        };
        if now_empty {
            self.by_server.remove(server);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    #[test]
    fn next_seq_is_monotonic() {
        let mut p = LspPending::default();
        assert_eq!(p.next_seq(), LspRequestSeq(1));
        assert_eq!(p.next_seq(), LspRequestSeq(2));
        assert_eq!(p.next_seq(), LspRequestSeq(3));
    }

    #[test]
    fn queue_goto_tracks_latest_seq_and_pushes_request() {
        let mut p = LspPending::default();
        let seq = p.queue_goto_definition(canon("a.rs"), 3, 7);
        assert_eq!(seq, LspRequestSeq(1));
        assert_eq!(p.latest_goto_seq, Some(LspRequestSeq(1)));
        assert_eq!(p.pending_goto.len(), 1);
        let req = &p.pending_goto[0];
        assert_eq!(req.path, canon("a.rs"));
        assert_eq!(req.line, 3);
        assert_eq!(req.col, 7);
        assert_eq!(req.seq, LspRequestSeq(1));
    }

    #[test]
    fn queue_goto_advances_latest_on_second_invoke() {
        let mut p = LspPending::default();
        p.queue_goto_definition(canon("a.rs"), 0, 0);
        let seq2 = p.queue_goto_definition(canon("a.rs"), 1, 1);
        assert_eq!(seq2, LspRequestSeq(2));
        assert_eq!(p.latest_goto_seq, Some(LspRequestSeq(2)));
        assert_eq!(p.pending_goto.len(), 2);
    }

    // ── Rename ───────────────────────────────────────────

    #[test]
    fn rename_state_open_seeds_input_with_word() {
        let s = RenameState::open(canon("a.rs"), 3, 7, Arc::<str>::from("foo"));
        assert_eq!(s.input.text, "foo");
        // Cursor parked at end so typing appends (matches legacy
        // "seed + extend" UX — Backspace walks back from the end).
        assert_eq!(s.input.cursor, 3);
        assert_eq!(s.anchor_line, 3);
        assert_eq!(s.anchor_col, 7);
    }

    #[test]
    fn queue_rename_tracks_latest_seq_and_pushes_request() {
        let mut p = LspPending::default();
        let seq = p.queue_rename(canon("a.rs"), 2, 4, Arc::<str>::from("bar"));
        assert_eq!(seq, LspRequestSeq(1));
        assert_eq!(p.latest_rename_seq, Some(LspRequestSeq(1)));
        assert_eq!(p.pending_rename.len(), 1);
        assert_eq!(p.pending_rename[0].new_name.as_ref(), "bar");
    }

    // ── Inlay hints ──────────────────────────────────────

    #[test]
    fn clear_inlay_hint_cache_drops_cache_and_requested() {
        let mut p = LspPending::default();
        p.inlay_hints_by_path.insert(
            canon("a.rs"),
            BufferInlayHints {
                version: BufferVersion(3),
                hints: Arc::new(Vec::new()),
            },
        );
        p.inlay_hints_requested
            .insert((canon("a.rs"), BufferVersion(3)));
        p.clear_inlay_hint_cache();
        assert!(p.inlay_hints_by_path.is_empty());
        assert!(p.inlay_hints_requested.is_empty());
    }

    #[test]
    fn queue_inlay_hints_records_requested_marker() {
        let mut p = LspPending::default();
        let seq = p.queue_inlay_hints(canon("a.rs"), BufferVersion(5), 0, 20);
        assert_eq!(seq, LspRequestSeq(1));
        assert_eq!(p.pending_inlay_hint.len(), 1);
        assert!(
            p.inlay_hints_requested
                .contains(&(canon("a.rs"), BufferVersion(5)))
        );
    }

    #[test]
    fn dismiss_rename_clears_overlay_preserves_seq() {
        let mut s = LspExtrasState::default();
        let mut p = LspPending::default();
        s.rename = Some(RenameState::open(
            canon("a.rs"),
            0,
            0,
            Arc::<str>::from("x"),
        ));
        // The commit path queues before dismissing the overlay.
        p.queue_rename(canon("a.rs"), 0, 0, Arc::<str>::from("y"));
        s.dismiss_rename();
        assert!(s.rename.is_none());
        // Abort still wants to see the completion so the edits
        // land; we don't clear latest_rename_seq here.
        assert_eq!(p.latest_rename_seq, Some(LspRequestSeq(1)));
    }
}
