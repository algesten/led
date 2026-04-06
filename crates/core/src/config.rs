use std::sync::Arc;

use crate::path::{CanonPath, UserPath};

#[derive(Debug, Default, Clone, PartialEq)]
/// Immutable configuration captured at startup. Do not mutate after construction —
/// runtime file opens go through `AppState::pending_opens` instead.
pub struct Startup {
    /// Run headless, used in tests.
    pub headless: bool,

    /// Enable file system watchers (docstore + workspace).
    /// Always true in production. Tests that don't need external-change
    /// or cross-instance-sync detection leave this false to avoid
    /// saturating macOS FSEvents under parallel test load.
    pub enable_watchers: bool,

    /// Files to open on startup (from CLI args). Immutable after construction.
    pub arg_paths: Vec<CanonPath>,

    /// Directory derived from the command line, or the directory
    /// where the binary started.
    pub start_dir: Arc<CanonPath>,

    /// The original user-provided start directory (before canonicalization).
    /// Used to derive user-facing paths that preserve symlink names.
    pub user_start_dir: UserPath,

    /// Directory to reveal in the file browser (from CLI `led <dir>` invocation).
    /// When set, the browser focuses this directory on startup instead of
    /// opening files.
    pub arg_dir: Option<CanonPath>,

    /// Config directory (e.g. ~/.config/led).
    pub config_dir: UserPath,

    /// Override the LSP server command for all languages (testing only).
    pub test_lsp_server: Option<UserPath>,
}
