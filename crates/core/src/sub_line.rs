//! `SubLine` newtype.
//!
//! 0-based index of a sub-line within its enclosing logical line.
//! Lives in core/ rather than `led-text-layout` because `Scroll`,
//! per-tab state, the session driver, and the jumps stack all
//! carry one — none of which want a transitive dep on the rope-
//! coupled layout machinery.

/// 0-based index of a sub-line within its enclosing logical line.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, drv::Input,
    serde::Serialize, serde::Deserialize,
)]
pub struct SubLine(pub usize);
