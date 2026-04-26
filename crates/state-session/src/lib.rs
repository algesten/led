//! Session persistence types — mirrors legacy
//! `led_workspace::{SessionData, SessionBuffer, RestoredSession,
//! UndoRestoreData}` exactly so the SQLite storage layout, save /
//! load orchestration, and undo-flush lifecycle line up with
//! legacy's design.
//!
//! The wire-format compatibility caveat: legacy's `UndoEntry` is a
//! single-op type with `direction` flags ("Emacs-style linear
//! history"). Our internal `History` uses past/future/current
//! stacks of multi-op `EditGroup`s. The schema's `undo_entries`
//! row carries an opaque BLOB; we put a serialised `EditGroup` in
//! it (not a legacy-shaped `UndoEntry`). The storage *structure*
//! is byte-for-byte legacy — separate metadata + append-only log
//! + session_kv — but the per-entry payload is ours.
//!
//! Cross-binary compatibility with legacy DBs is therefore
//! intentionally out-of-scope.

use std::collections::HashMap;

use led_core::{CanonPath, PersistedContentHash};
use led_state_buffer_edits::EditGroup;
use led_state_tabs::{Cursor, Scroll};

/// Live session state on `Atoms.session`. Driven by the
/// session driver:
///
/// - `Init` outbound → `Restored` inbound flips `primary` to
///   the flock outcome and stamps `last_saved` with whatever
///   the DB held.
/// - `SaveSession` outbound → `SessionSaved` inbound sets
///   `saved = true`.
/// - The Quit gate consults `(saved || !primary)` to decide
///   whether the loop can break.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionState {
    /// `true` when the most recent save round-trip completed,
    /// or when there was nothing to save (non-primary,
    /// standalone). The Quit gate consults this; while
    /// `phase == Exiting && !saved && primary` the main loop
    /// keeps spinning.
    pub saved: bool,
    /// `true` when this process owns the workspace's primary
    /// flock at `<config>/primary/<hash(root)>`. Secondaries
    /// are still functional but don't restore the saved
    /// session and don't write on quit.
    pub primary: bool,
    /// `true` once the session driver has answered the initial
    /// `Init` command. Until then the runtime keeps `Phase::
    /// Starting` and avoids dispatching a second `Init`.
    pub init_done: bool,
    /// What was on disk at startup (or the last successful
    /// save). Used by the Quit-side derived to diff against the
    /// current state and skip the `SaveSession` dispatch when
    /// nothing has changed.
    pub last_saved: Option<SessionData>,
    /// Per-path restored undo state, stashed on `Restored`
    /// ingest and consumed by the load-completion hook to
    /// install history into the freshly-loaded buffer.
    pub pending_undo: imbl::HashMap<CanonPath, UndoRestoreData>,
}

/// Full session payload — written wholesale on
/// `SessionCmd::SaveSession`, returned wholesale by
/// `SessionEvent::Restored`. Mirrors legacy's `SessionData`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionData {
    /// Index into `tabs` of the tab that was active when the
    /// session was saved.
    pub active_tab_order: usize,
    /// Whether the file browser was visible when the session
    /// was saved — restored as-is on next launch.
    pub show_side_panel: bool,
    /// Open buffers in display order. Position-sensitive: the
    /// same order is restored.
    pub buffers: Vec<SessionBuffer>,
    /// Free-form key/value pairs for non-buffer state (browser
    /// expanded dirs, jump list, isearch query, …). Legacy
    /// stores these in the `session_kv` table.
    pub kv: HashMap<String, String>,
}

/// One persisted buffer entry. Mirrors legacy `SessionBuffer`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionBuffer {
    pub path: CanonPath,
    pub tab_order: usize,
    pub cursor: Cursor,
    pub scroll: Scroll,
    /// Restored undo state — populated by `load_session` from
    /// the `buffer_undo_state` + `undo_entries` tables. `None`
    /// when no persisted undo exists for this buffer.
    pub undo: Option<UndoRestoreData>,
}

/// Per-buffer undo restore payload. Mirrors legacy
/// `UndoRestoreData`.
#[derive(Debug, Clone, PartialEq)]
pub struct UndoRestoreData {
    /// UUID-like identifier for this undo chain. Two sessions
    /// referring to the same chain_id can extend each other's
    /// history; a different chain_id means a fresh start.
    pub chain_id: String,
    /// Hash of the file content the chain was anchored against.
    /// On restore: only install the entries when the freshly-
    /// loaded rope's hash matches.
    pub content_hash: PersistedContentHash,
    /// Position in the past stack (== past.len()). `None`
    /// means "head of history."
    pub undo_cursor: Option<usize>,
    /// Net distance from the save point — how many groups have
    /// been applied since the last save. 0 = clean.
    pub distance_from_save: i32,
    /// All persisted EditGroups in append order. Restored as
    /// `eb.history.past` after a hash check; future stays
    /// empty (entries past undo_cursor are conceptually in
    /// future, but our internal stacks are append-only-past).
    pub entries: Vec<EditGroup>,
    /// SQLite seq of the last entry — used by sync (M21+)
    /// to fetch only entries newer than this.
    pub last_seq: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    #[test]
    fn default_state_is_unsaved_non_primary_no_last() {
        let s = SessionState::default();
        assert!(!s.saved);
        assert!(!s.primary);
        assert!(!s.init_done);
        assert!(s.last_saved.is_none());
    }

    #[test]
    fn session_data_clone_round_trips() {
        let d = SessionData {
            active_tab_order: 1,
            show_side_panel: true,
            buffers: vec![SessionBuffer {
                path: canon("/p/a.rs"),
                tab_order: 0,
                cursor: Cursor {
                    line: 4,
                    col: 2,
                    preferred_col: 2,
                },
                scroll: Scroll::default(),
                undo: None,
            }],
            kv: HashMap::from([("browser.selected".into(), "2".into())]),
        };
        assert_eq!(d.clone(), d);
    }
}
