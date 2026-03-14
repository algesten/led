use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Startup {
    /// Argument given on the command line, if any.
    pub arg_path: Option<PathBuf>,

    /// Directory derived from the command line, or the directory
    /// where the binary started.
    pub start_dir: Arc<PathBuf>,

    /// Config directory (e.g. ~/.config/led).
    pub config_dir: PathBuf,
}
