//! Shared foundational types for the led rewrite.
//!
//! Everything here is cross-cutting and atom-free — no `drv::atom`,
//! no drivers, no app logic. Currently:
//!
//! - [`id_newtype!`] — macro for strongly-typed u64 identifier newtypes
//! - [`UserPath`] / [`CanonPath`] — path newtypes mirroring legacy led's
//!   user-vs-canonical split

pub mod content_hash;
pub mod ids;
pub mod issue;
pub mod notify;
pub mod paths;
pub mod text_input;
pub mod wrap;

/// Re-export of the `drv` crate so the `impl_identity_to_static!`
/// macro below can reference it via `$crate::drv` without every
/// consumer crate needing `drv` as a direct dependency.
#[doc(hidden)]
pub use drv;

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

/// Impl [`drv::ToStatic`] for a type that's cheaply clonable and
/// fully-owned (no borrows). Use this for tuple structs and enums
/// that need to appear as fields of a `#[derive(drv::Input)]`
/// projection — the derive requires named-field structs, so
/// tuple structs and enums can't use it and need a manual impl.
///
/// The impl is the "identity" shape: `Static = Self`, `to_static`
/// clones, `eq_static` compares. Equivalent to what drv ships
/// internally for primitives, tightened to also require `'static`
/// (which all app-level types are).
///
/// ```ignore
/// led_core::impl_identity_to_static!(CanonPath);
/// led_core::impl_identity_to_static!(Focus);
/// ```
#[macro_export]
macro_rules! impl_identity_to_static {
    ($t:ty) => {
        impl $crate::drv::ToStatic for $t {
            type Static = $t;
            fn to_static(&self) -> Self::Static {
                ::core::clone::Clone::clone(self)
            }
            fn eq_static(&self, other: &Self::Static) -> bool {
                ::core::cmp::PartialEq::eq(self, other)
            }
        }
    };
}
