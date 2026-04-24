//! Content-hash newtypes for diagnostic version matching.
//!
//! Legacy led identifies "buffer state for LSP purposes" by a hash of
//! the rope's content, not by a monotonic version counter. Content
//! hashes are stable across undo/redo: typing a character and then
//! deleting it restores the original hash, which lets the runtime
//! accept a late diagnostic push for the earlier state and replay it
//! forward through any remaining edits (legacy's
//! `offer_diagnostics` + `replay_diagnostics`).
//!
//! Two flavours, mirroring legacy exactly:
//!
//! - [`EphemeralContentHash`] is what the buffer computes **right now**
//!   from its current rope. Cheap to recompute per tick; changes on
//!   every edit. Never travels across the LSP pipeline on its own.
//! - [`PersistedContentHash`] is a hash that has been **anchored** —
//!   either because the buffer was saved (the runtime records one on
//!   the history as a save-point marker at that moment) or because
//!   the LSP driver snapshotted it when opening a diagnostic window.
//!   Diagnostics travelling in/out of the pipeline carry one of
//!   these; the runtime matches them against the current
//!   ephemeral hash (fast path) or against the history's recorded
//!   save-points (replay path).
//!
//! The two newtypes are distinct so `&mut` borrows and API surfaces
//! can't silently conflate "what the buffer is right now" with "what
//! a diagnostic was computed against." Promoting ephemeral → persisted
//! is an explicit [`EphemeralContentHash::persist`] call; the reverse
//! is never needed.
//!
//! The u64 is a FxHash of the rope's bytes — collision-safe enough
//! for this use (compare against a small set of save-point markers,
//! never cryptographic) and fast enough to recompute on every edit
//! without showing up in allocation profiles.

/// Hash of the buffer's rope **as it is right now**. Recomputed on
/// every edit; cheap. Compare against a
/// [`PersistedContentHash`] via [`Self::matches`] — never mixed
/// directly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct EphemeralContentHash(pub u64);

impl EphemeralContentHash {
    /// Hash a ropey `Rope`'s bytes in chunk order. Ropey iterates
    /// chunks in document order so the hash is deterministic for a
    /// given logical content.
    pub fn of_rope(rope: &ropey::Rope) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for chunk in rope.chunks() {
            chunk.as_bytes().hash(&mut hasher);
        }
        Self(hasher.finish())
    }

    /// Promote this ephemeral hash to a [`PersistedContentHash`].
    /// Call at save time (the current buffer content is now what was
    /// written to disk) or when the LSP driver opens a window and
    /// needs a persisted anchor to stamp diagnostics with.
    pub fn persist(self) -> PersistedContentHash {
        PersistedContentHash(self.0)
    }

    /// Returns `true` when this ephemeral hash and a diagnostic's
    /// persisted hash identify the same byte content. Used for the
    /// fast path in diagnostic ingestion.
    pub fn matches(self, persisted: PersistedContentHash) -> bool {
        self.0 == persisted.0
    }
}

/// Hash that has been **anchored** into some persistent record —
/// either the history's save-point markers (inserted on save) or
/// a diagnostic window's snapshot. Travels on LSP commands,
/// events, and `BufferDiagnostics`. Never computed directly;
/// obtained via [`EphemeralContentHash::persist`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PersistedContentHash(pub u64);
