use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Startup {
    /// Run headless, used in tests.
    pub headless: bool,

    /// Files to open on startup.
    pub arg_paths: Vec<PathBuf>,

    /// Directory derived from the command line, or the directory
    /// where the binary started.
    pub start_dir: Arc<PathBuf>,

    /// Config directory (e.g. ~/.config/led).
    pub config_dir: PathBuf,
}
