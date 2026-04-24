//! M18 LSP-extras state — pending-request outboxes + overlay /
//! hint stores that the non-diagnostic non-completion LSP flows
//! need.
//!
//! Shape mirrors `state-completions`:
//!
//! - `seq_gen: u64` allocates request ids.
//! - `pending_*: Vec<_>` outboxes the runtime drains into
//!   `LspCmd::*` during the execute phase.
//! - `latest_*_seq: Option<u64>` lets the ingest side reject
//!   stale responses without needing per-request tracking on
//!   the driver side.
//!
//! The crate grows one piece per stage:
//!
//! - Stage 2 ships `LspExtrasState` with the goto-definition
//!   fields.
//! - Stage 3 adds the rename overlay.
//! - Stage 4 adds the code-action picker.
//! - Stage 5 adds per-buffer inlay hints + the toggle flag.
//! - Stage 6 adds the pending-format seq and the
//!   save-after-format gate.

use std::sync::Arc;

use led_core::{CanonPath, TextInput};
use led_driver_lsp_core::{CodeActionSummary, InlayHint};

/// Root atom. Every M18 feature folds its own concern onto this
/// struct; for stages 2–3 only goto-definition + rename are
/// present.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LspExtrasState {
    /// Monotonic sequence id used by every outbound M18 RPC so
    /// responses can be matched up and stale replies dropped.
    /// Shared across concerns to keep the id space flat and so
    /// round-trip tests don't have to juggle per-concern counters.
    pub seq_gen: u64,

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
    pub latest_goto_seq: Option<u64>,

    // ── Rename overlay ─────────────────────────────────────────

    /// Active rename overlay (None when no rename is in flight).
    /// Mutually exclusive with completions / find-file /
    /// code-actions — at most one overlay owns editor input at
    /// any time.
    pub rename: Option<RenameState>,
    /// Pending `textDocument/rename` requests queued by
    /// dispatch on Enter-commit. Drained by the execute phase.
    pub pending_rename: Vec<PendingRename>,
    /// Latest allocated rename seq. `LspEvent::Edits {
    /// origin: Rename, seq }` is dropped when `seq` doesn't
    /// match — the user has since aborted or kicked off another
    /// rename and applying the stale edit batch would be confusing.
    pub latest_rename_seq: Option<u64>,

    // ── Code action picker ────────────────────────────────────

    /// Active code-action picker overlay. Populated when an
    /// `LspEvent::CodeActions` delivery arrives with at least
    /// one item; cleared on commit / abort / stale-seq drop.
    pub code_actions: Option<CodeActionPickerState>,
    /// Pending `textDocument/codeAction` requests.
    pub pending_code_action: Vec<PendingCodeActionRequest>,
    /// Latest request seq; ingest drops any
    /// `LspEvent::CodeActions` whose seq doesn't match. Not the
    /// same as `latest_code_action_select_seq` — picker install
    /// and commit are distinct round trips.
    pub latest_code_action_seq: Option<u64>,
    /// Pending `codeAction/resolve + apply` selects. Populated
    /// on Enter-commit from the picker overlay; drained by the
    /// execute phase into `LspCmd::SelectCodeAction`.
    pub pending_code_action_select: Vec<PendingCodeActionSelect>,
    /// Latest commit seq; ingest drops any
    /// `LspEvent::Edits { origin: CodeAction }` whose seq
    /// doesn't match. A fresh Alt-i session invalidates any
    /// in-flight commit — legacy parity.
    pub latest_code_action_select_seq: Option<u64>,

    // ── Inlay hints ────────────────────────────────────────────

    /// `Ctrl-t` toggle. When `false`, no inlay-hint requests
    /// fire and the painter doesn't draw hints even when the
    /// cache holds some (users expect the toggle to actually
    /// turn them off visually).
    pub inlay_hints_enabled: bool,
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
    pub inlay_hints_requested: imbl::HashSet<(CanonPath, u64)>,

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
    pub latest_format_seq: imbl::HashMap<CanonPath, u64>,
}

/// One queued goto-definition request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingGoto {
    pub path: CanonPath,
    pub seq: u64,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

led_core::impl_identity_to_static!(RenameState);

/// Queued rename request — the `new_name` is whatever the user
/// had typed into `RenameState.input` at Enter time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRename {
    pub path: CanonPath,
    pub seq: u64,
    pub line: u32,
    pub col: u32,
    pub new_name: Arc<str>,
}

/// Picker overlay for `textDocument/codeAction`. At most one
/// picker is open at a time; `items` is ref-counted so paint +
/// dispatch share without cloning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeActionPickerState {
    /// Origin buffer — selection commits route back here on
    /// `SelectCodeAction` so the manager knows which server to
    /// ask.
    pub path: CanonPath,
    /// Seq of the `LspEvent::CodeActions` delivery that
    /// populated this picker. A later delivery with a higher
    /// seq replaces the whole picker wholesale.
    pub seq: u64,
    pub items: Arc<Vec<CodeActionSummary>>,
    pub selected: usize,
    pub scroll: usize,
}

led_core::impl_identity_to_static!(CodeActionPickerState);

/// Queued `textDocument/codeAction` request. Range covers
/// mark..cursor when a selection is active, else cursor..cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCodeActionRequest {
    pub path: CanonPath,
    pub seq: u64,
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
    pub seq: u64,
    pub action: CodeActionSummary,
}

/// Queued inlay-hint request for `path` over the visible
/// `(start_line..end_line)` viewport. The response stamps its
/// reply with `version`, so stale arrivals drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInlayHintRequest {
    pub path: CanonPath,
    pub seq: u64,
    pub version: u64,
    pub start_line: u32,
    pub end_line: u32,
}

/// Per-buffer inlay-hint cache. `version` is the buffer
/// version the hints were computed against; the painter
/// displays them only when `version == buffer.version` (no
/// smear on stale data).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BufferInlayHints {
    pub version: u64,
    pub hints: Arc<Vec<InlayHint>>,
}

/// Queued `textDocument/formatting` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFormat {
    pub path: CanonPath,
    pub seq: u64,
}

impl LspExtrasState {
    /// Allocate the next request sequence id.
    pub fn next_seq(&mut self) -> u64 {
        self.seq_gen = self.seq_gen.wrapping_add(1);
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
    ) -> u64 {
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
    ) -> u64 {
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

    /// Dismiss the rename overlay. Idempotent. Does NOT clear
    /// `latest_rename_seq` — an in-flight request may still
    /// deliver edits the user would want applied if they
    /// aborted out of curiosity; the runtime gates acceptance
    /// by the rename overlay being `None`, not by the seq.
    pub fn dismiss_rename(&mut self) {
        self.rename = None;
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
    ) -> u64 {
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
    ) -> u64 {
        let seq = self.next_seq();
        self.latest_code_action_select_seq = Some(seq);
        self.pending_code_action_select
            .push(PendingCodeActionSelect { path, seq, action });
        seq
    }

    /// Dismiss the code-action picker. Idempotent.
    pub fn dismiss_code_actions(&mut self) {
        self.code_actions = None;
    }

    /// Flip the inlay-hints toggle. Turning off clears the
    /// per-buffer cache + the in-flight-request ledger so the
    /// next toggle-on refetches — matches legacy's "off
    /// returns to blank state" UX.
    pub fn toggle_inlay_hints(&mut self) -> bool {
        self.inlay_hints_enabled = !self.inlay_hints_enabled;
        if !self.inlay_hints_enabled {
            self.inlay_hints_by_path.clear();
            self.inlay_hints_requested.clear();
        }
        self.inlay_hints_enabled
    }

    /// Queue a `textDocument/formatting` request. Returns the
    /// allocated seq.
    pub fn queue_format(&mut self, path: CanonPath) -> u64 {
        let seq = self.next_seq();
        self.latest_format_seq.insert(path.clone(), seq);
        self.pending_format.push(PendingFormat { path, seq });
        seq
    }

    /// Queue a `textDocument/inlayHint` request.
    pub fn queue_inlay_hints(
        &mut self,
        path: CanonPath,
        version: u64,
        start_line: u32,
        end_line: u32,
    ) -> u64 {
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
        let mut s = LspExtrasState::default();
        assert_eq!(s.next_seq(), 1);
        assert_eq!(s.next_seq(), 2);
        assert_eq!(s.next_seq(), 3);
    }

    #[test]
    fn queue_goto_tracks_latest_seq_and_pushes_request() {
        let mut s = LspExtrasState::default();
        let seq = s.queue_goto_definition(canon("a.rs"), 3, 7);
        assert_eq!(seq, 1);
        assert_eq!(s.latest_goto_seq, Some(1));
        assert_eq!(s.pending_goto.len(), 1);
        let req = &s.pending_goto[0];
        assert_eq!(req.path, canon("a.rs"));
        assert_eq!(req.line, 3);
        assert_eq!(req.col, 7);
        assert_eq!(req.seq, 1);
    }

    #[test]
    fn queue_goto_advances_latest_on_second_invoke() {
        let mut s = LspExtrasState::default();
        s.queue_goto_definition(canon("a.rs"), 0, 0);
        let seq2 = s.queue_goto_definition(canon("a.rs"), 1, 1);
        assert_eq!(seq2, 2);
        assert_eq!(s.latest_goto_seq, Some(2));
        assert_eq!(s.pending_goto.len(), 2);
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
        let mut s = LspExtrasState::default();
        let seq = s.queue_rename(canon("a.rs"), 2, 4, Arc::<str>::from("bar"));
        assert_eq!(seq, 1);
        assert_eq!(s.latest_rename_seq, Some(1));
        assert_eq!(s.pending_rename.len(), 1);
        assert_eq!(s.pending_rename[0].new_name.as_ref(), "bar");
    }

    // ── Inlay hints ──────────────────────────────────────

    #[test]
    fn toggle_inlay_hints_flips_flag_and_clears_cache_on_off() {
        let mut s = LspExtrasState::default();
        assert!(!s.inlay_hints_enabled);
        assert!(s.toggle_inlay_hints());
        assert!(s.inlay_hints_enabled);
        s.inlay_hints_by_path.insert(
            canon("a.rs"),
            BufferInlayHints {
                version: 3,
                hints: Arc::new(Vec::new()),
            },
        );
        s.inlay_hints_requested.insert((canon("a.rs"), 3));
        assert!(!s.toggle_inlay_hints());
        assert!(!s.inlay_hints_enabled);
        assert!(s.inlay_hints_by_path.is_empty());
        assert!(s.inlay_hints_requested.is_empty());
    }

    #[test]
    fn queue_inlay_hints_records_requested_marker() {
        let mut s = LspExtrasState::default();
        let seq = s.queue_inlay_hints(canon("a.rs"), 5, 0, 20);
        assert_eq!(seq, 1);
        assert_eq!(s.pending_inlay_hint.len(), 1);
        assert!(s.inlay_hints_requested.contains(&(canon("a.rs"), 5)));
    }

    #[test]
    fn dismiss_rename_clears_overlay_preserves_seq() {
        let mut s = LspExtrasState::default();
        s.rename = Some(RenameState::open(
            canon("a.rs"),
            0,
            0,
            Arc::<str>::from("x"),
        ));
        // The commit path queues before dismissing the overlay.
        s.queue_rename(canon("a.rs"), 0, 0, Arc::<str>::from("y"));
        s.dismiss_rename();
        assert!(s.rename.is_none());
        // Abort still wants to see the completion so the edits
        // land; we don't clear latest_rename_seq here.
        assert_eq!(s.latest_rename_seq, Some(1));
    }
}
