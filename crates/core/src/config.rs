use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Default)]
pub struct Startup {
    /// Argument given on the command line, if any.
    pub arg_path: Option<PathBuf>,

    /// Directory derived from the command line, or the directory
    /// where the binary started.
    pub start_dir: Arc<PathBuf>,
}
