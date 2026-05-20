//! Shared foundational types for the led rewrite.
//!
//! Everything here is cross-cutting and source-free — no `drv::Input` derive,
//! no drivers, no app logic. Currently:
//!
//! - [`id_newtype!`] / [`string_newtype!`] — macros for strongly-typed
//!   primitive and string newtypes (`TabId`, `BufferVersion`,
//!   `LspRequestSeq`, `ChainId`, `ServerId`, …)
//! - [`UserPath`] / [`CanonPath`] — path newtypes mirroring legacy led's
//!   user-vs-canonical split

pub mod content_hash;
pub mod git;
pub mod ids;
pub mod issue;
pub mod notify;
pub mod paths;
pub mod sub_line;

/// Re-export of the `drv` crate so the `id_newtype!` macro can
/// reference the `drv::Input` derive via `$crate::drv::Input`
/// from any call site, without every consumer crate needing
/// `drv` as a direct dependency (they already get it transitively
/// through `led-core`).
#[doc(hidden)]
pub use drv;

pub use content_hash::{EphemeralContentHash, PersistedContentHash};
pub use ids::{
    BufferStateSum, BufferVersion, ChainId, EditSeq, LspRequestSeq, SavedVersion, ServerId,
    UndoDbSeq, WatchSeq,
};
pub use issue::{CategoryInfo, IssueCategory};
pub use notify::Notifier;
pub use paths::{CanonPath, PathChain, UserPath};
pub use sub_line::SubLine;

// `id_newtype!` is `#[macro_export]` so it's already callable as
// `led_core::id_newtype!(...)` without a re-export line.
