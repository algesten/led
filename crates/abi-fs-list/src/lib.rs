//! Shared ABI types at the FS-list driver boundary.
//!
//! [`DirEntry`] and [`DirEntryKind`] are the wire shape the FS-list
//! driver emits and the file-browser state stores. Both
//! `driver-fs-list/core` (producer) and `state-browser` (consumer)
//! depend on this leaf crate so the tier-rule from
//! `EXAMPLE-ARCH.md` § "Cross-driver composition lives in a runtime
//! crate" stays clean: `state-*` never reaches into `driver-*`.

use led_core::CanonPath;

/// Whether a child of a listed directory is itself a directory or a
/// regular file. Symlinks, sockets, FIFOs, etc. are collapsed onto
/// these two categories by the native worker (symlink → kind of the
/// target, everything else → `File`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirEntryKind {
    File,
    Directory,
}

/// A single child entry from a directory listing.
///
/// `name` is the leaf name only (no trailing `/` even for directories
/// — the renderer adds it). `path` is the canonicalized full path so
/// downstream consumers can use it as a stable key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub path: CanonPath,
    pub kind: DirEntryKind,
}
