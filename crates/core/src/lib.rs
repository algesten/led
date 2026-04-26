//! Shared foundational types for the led rewrite.
//!
//! Everything here is cross-cutting and atom-free — no `drv::atom`,
//! no drivers, no app logic. Currently:
//!
//! - [`id_newtype!`] — macro for strongly-typed u64 identifier newtypes
//! - [`UserPath`] / [`CanonPath`] — path newtypes mirroring legacy led's
//!   user-vs-canonical split

pub mod command;
pub mod content_hash;
pub mod git;
pub mod ids;
pub mod issue;
pub mod notify;
pub mod paths;
pub mod text_input;
pub mod wrap;

/// Re-export of the `drv` crate so the `id_newtype!` macro can
/// reference the `drv::Input` derive via `$crate::drv::Input`
/// from any call site, without every consumer crate needing
/// `drv` as a direct dependency (they already get it transitively
/// through `led-core`).
#[doc(hidden)]
pub use drv;

pub use command::{Command, parse_command};
pub use content_hash::{EphemeralContentHash, PersistedContentHash};
pub use issue::{
    CategoryInfo, IssueCategory, StatusDisplay, directory_categories, resolve_display,
};
pub use notify::Notifier;
pub use paths::{CanonPath, PathChain, UserPath};
pub use text_input::TextInput;
pub use wrap::{
    SubLine, col_to_sub_line, is_continued, sub_line_col_to_line_col, sub_line_count,
    sub_line_range,
};

// `id_newtype!` is `#[macro_export]` so it's already callable as
// `led_core::id_newtype!(...)` without a re-export line.
