use std::fs::File;
use std::path::Path;

/// Initialize logging to a file. Uses `RUST_LOG` env var for filter level,
/// defaulting to `trace` if not set.
pub fn init_file_logger(path: &Path) {
    let file = File::create(path).expect("failed to create log file");
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("trace"))
        .format_timestamp_millis()
        .target(env_logger::Target::Pipe(Box::new(file)))
        .init();
}
