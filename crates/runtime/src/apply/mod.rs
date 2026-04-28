//! Free helpers extracted from the runtime's main loop.
//!
//! Carved out of `lib.rs` purely to keep the crate root focused on
//! types + the `run()` tick loop. Every item in this module is a
//! verbatim move from `lib.rs` — no logic changes — and each
//! function is `pub(crate)` so the tick loop can call it just like
//! before.

pub(crate) mod edit;
pub(crate) mod fs;
pub(crate) mod lsp;
pub(crate) mod session;
