//! Shared ABI type at the find-file driver boundary.
//!
//! [`FindFileEntry`] is the wire shape the find-file driver emits and
//! the overlay state stores. Both `driver-find-file/core` (producer)
//! and `state-find-file` (consumer) depend on this leaf crate so the
//! tier-rule from `EXAMPLE-ARCH.md` § "Cross-driver composition lives
//! in a runtime crate" stays clean: `state-*` never reaches into
//! `driver-*`.

use led_core::CanonPath;

/// One completion-list entry. `name` has a trailing `/` for
/// directories so the renderer doesn't need to inspect `is_dir`;
/// `full` is the canonicalized target for open / save requests.
#[derive(Debug, Clone, PartialEq, Eq, drv::Input)]
pub struct FindFileEntry {
    pub name: String,
    pub full: CanonPath,
    pub is_dir: bool,
}
