//! Per-phase free functions extracted from the runtime's main `run()` loop.
//!
//! Each phase function takes `&mut Atoms` + (optionally) a borrowed
//! `TickEnv` of read-only loop-invariants, then destructures `Atoms`
//! once at its top to obtain the disjoint `&mut` field references it
//! needs. This sidesteps the borrow-check problems that arose from
//! one giant top-level destructure threaded through every phase.
//!
//! Translation discipline: the bodies are verbatim moves from the
//! original `run()` body. The only edits are the function-boundary
//! plumbing (signature, the `let Atoms { … } = atoms;` destructure,
//! and minor scoping fixes for locals that crossed phase boundaries).

use led_core::CanonPath;
use led_driver_terminal_core::Theme;

use crate::keymap::Keymap;
use crate::trace::SharedTrace;
use crate::{Drivers, Wake};

pub(crate) mod dispatch_phase;
pub(crate) mod execute_phase;
pub(crate) mod file_watch_dispatch;
pub(crate) mod git_dispatch;
pub(crate) mod ingest;
pub(crate) mod lsp_dispatch;
pub(crate) mod query_phase;
pub(crate) mod render_phase;
pub(crate) mod session_dispatch;
pub(crate) mod wait_phase;

/// Read-only loop-invariants threaded through phases. Built once
/// before the loop; `&TickEnv<'_>` is passed to phases that need
/// it. `stdout` is held separately because it's mutably borrowed.
pub(crate) struct TickEnv<'a> {
    pub drivers: &'a Drivers,
    pub keymap: &'a Keymap,
    pub theme: &'a Theme,
    pub wake: &'a Wake,
    pub trace: &'a SharedTrace,
    pub no_workspace: bool,
    pub resolved_config_dir: &'a Option<CanonPath>,
    pub resolved_notify_dir: &'a Option<CanonPath>,
}
