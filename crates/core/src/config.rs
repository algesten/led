use std::path::PathBuf;
use std::sync::Arc;

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
    pub arg_paths: Vec<PathBuf>,

    /// Directory derived from the command line, or the directory
    /// where the binary started.
    pub start_dir: Arc<PathBuf>,

    /// Config directory (e.g. ~/.config/led).
    pub config_dir: PathBuf,
}
