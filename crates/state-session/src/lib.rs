//! Session persistence state atom (M21).
//!
//! The session is the cross-process editor state that survives a
//! quit / restart cycle: which tabs were open, where each cursor
//! sat, what the active tab was. The actual SQLite I/O lives in
//! the session driver; this crate is the data shape the runtime
//! folds the driver's events into and re-projects on quit.
//!
//! Three pieces:
//!
//! - [`SessionState`] — the live atom on `Atoms.session`.
//! - [`SessionData`] — the serialisable session payload.
//!   Crosses the driver ABI in both directions (Save command +
//!   Restored event).
//! - [`SessionTab`] — one entry inside a `SessionData`, the
//!   per-buffer slice (path + cursor + scroll).

use led_core::CanonPath;
use led_core::PersistedContentHash;
use led_state_tabs::{Cursor, Scroll};

/// Live session state on `Atoms.session`. Driven by the
/// session driver:
///
/// - `Init` outbound → `Restored` inbound flips `primary` to
///   the flock outcome and stamps `last_saved` with whatever
///   the DB held.
/// - `Save` outbound → `Saved` inbound sets `saved = true`.
/// - The Quit gate consults `(saved || !primary)` to decide
///   whether the loop can break.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionState {
    /// `true` when the most recent Save round-trip completed,
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
    /// Save). Used by the Quit-side derived to diff against the
    /// current state and skip the `Save` dispatch when nothing
    /// has changed.
    pub last_saved: Option<SessionData>,
    /// Per-path buffer state stashed on `Restored` ingest,
    /// keyed by canonical path. The load-completion hook reads
    /// this when a buffer materialises: if the disk's hash
    /// matches the snapshot's `disk_anchor_hash`, restore
    /// `eb.rope` from `rope_content` and decode the history;
    /// otherwise drop silently. The entry is always removed
    /// after a load completion regardless of match.
    pub pending_buffer_state: imbl::HashMap<CanonPath, BufferStateSnapshot>,
}

/// One persisted snapshot of the editor's session for one
/// workspace root. Crosses the session driver's ABI so it lives
/// in `state-session` (the driver core depends on us, not the
/// other way around).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionData {
    /// Index into `tabs` of the tab that was active when the
    /// session was saved. `None` when no tabs were open.
    pub active_tab_idx: Option<usize>,
    /// Whether the file browser was visible when the session
    /// was saved — restored as-is on next launch.
    pub show_side_panel: bool,
    /// Open tabs in display order. Position-sensitive: the same
    /// order is restored.
    pub tabs: Vec<SessionTab>,
    /// Per-buffer state snapshots: dirty rope content + undo
    /// history, stamped with the disk anchor hash. The runtime
    /// only restores a snapshot when its `disk_anchor_hash`
    /// matches the freshly-loaded rope's hash; mismatches drop
    /// silently because the file changed externally between
    /// sessions and the persisted state references different
    /// bytes.
    pub buffer_states: Vec<BufferStateSnapshot>,
}

/// One persisted buffer state. `rope_content` is the in-memory
/// rope at quit time (which may carry unsaved edits relative to
/// `disk_anchor_hash`); `history_blob` is opaque, owned by
/// `state-buffer-edits::History::{encode,decode}`.
///
/// On restore: read the disk file, hash it. If the hash equals
/// `disk_anchor_hash` (no external modification), install the
/// snapshot — `eb.rope` becomes `rope_content` and `eb.history`
/// becomes `decode(history_blob)`. Otherwise drop silently and
/// fall back to the disk content with empty history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferStateSnapshot {
    pub path: CanonPath,
    /// Hash of the file's *disk* content at the moment we
    /// persisted this snapshot. NOT the in-memory rope's hash —
    /// `rope_content` may differ from disk by the user's
    /// unsaved edits.
    pub disk_anchor_hash: PersistedContentHash,
    /// The in-memory rope at quit time. Restored verbatim into
    /// `eb.rope` when the disk anchor matches.
    pub rope_content: String,
    /// Encoded `History`. Empty Vec when the buffer had no
    /// recorded history (just stash the rope to survive quit).
    pub history_blob: Vec<u8>,
}

/// One persisted tab entry. The path is the canonical form;
/// re-mapping to the user-typed form (for symlinked dotfiles
/// etc.) is a future refinement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTab {
    pub path: CanonPath,
    pub cursor: Cursor,
    pub scroll: Scroll,
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
            active_tab_idx: Some(1),
            show_side_panel: true,
            buffer_states: Vec::new(),
            tabs: vec![
                SessionTab {
                    path: canon("/p/a.rs"),
                    cursor: Cursor {
                        line: 4,
                        col: 2,
                        preferred_col: 2,
                    },
                    scroll: Scroll::default(),
                },
                SessionTab {
                    path: canon("/p/b.rs"),
                    cursor: Cursor::default(),
                    scroll: Scroll::default(),
                },
            ],
        };
        let cloned = d.clone();
        assert_eq!(d, cloned);
    }
}
