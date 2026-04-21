//! Shared foundational types for the led rewrite.
//!
//! Everything here is cross-cutting and atom-free — no `drv::atom`,
//! no drivers, no app logic. Currently:
//!
//! - [`id_newtype!`] — macro for strongly-typed u64 identifier newtypes
//! - [`UserPath`] / [`CanonPath`] — path newtypes mirroring legacy led's
//!   user-vs-canonical split

pub mod ids;
pub mod notify;
pub mod paths;
pub mod text_input;

pub use notify::Notifier;
pub use paths::{CanonPath, UserPath};
pub use text_input::TextInput;

// `id_newtype!` is `#[macro_export]` so it's already callable as
// `led_core::id_newtype!(...)` without a re-export line.
