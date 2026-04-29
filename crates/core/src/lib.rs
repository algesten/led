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

pub mod command;
pub mod content_hash;
pub mod git;
pub mod grapheme;
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
pub use ids::{
    BufferStateSum, BufferVersion, ChainId, EditSeq, LspRequestSeq, SavedVersion, ServerId,
    UndoDbSeq, WatchSeq,
};
pub use grapheme::{
    TAB_STOP, char_to_grapheme_col, display_col_to_grapheme, grapheme_col_to_char,
    grapheme_col_to_utf16_units, grapheme_display_width, line_grapheme_len,
    prefix_display_width, utf16_units_to_grapheme_col,
};
pub use issue::{
    CategoryInfo, IssueCategory, StatusDisplay, directory_categories, resolve_display,
};
pub use notify::Notifier;
pub use paths::{CanonPath, PathChain, UserPath};
pub use text_input::TextInput;
pub use wrap::{
    SubLine, SubLineRange, col_to_sub_line, is_continued, line_layout,
    sub_line_cells_to_grapheme_col, sub_line_count, sub_line_range,
};

// `id_newtype!` is `#[macro_export]` so it's already callable as
// `led_core::id_newtype!(...)` without a re-export line.
