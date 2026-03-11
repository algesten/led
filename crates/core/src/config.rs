use std::path::PathBuf;

#[derive(Default)]
pub struct Config {
    pub arg_path: Option<PathBuf>,
    pub start_dir: PathBuf,
}
