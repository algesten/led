use std::fs::{self, File, OpenOptions};
use std::hash::DefaultHasher;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use led_core::AStream;
use tokio_stream::StreamExt;

const CONFIG_DIR: &str = ".config";
const LED_DIR: &str = "led";
const GIT_DIR: &str = ".git";
const PRIMARY_DIR: &str = "primary";

pub struct StartDir(pub Arc<PathBuf>);

#[derive(Clone, Default, Debug)]
pub struct Workspace {
    /// Workspace root. The project that is open.
    pub root: PathBuf,

    /// Path to directory holding config.
    pub config: PathBuf,

    /// Whether this is the primary editor (persisting workspace changes etc),
    /// or secondary that just edits files.
    pub primary: bool,
}

pub fn driver(input: impl AStream<StartDir>) -> impl AStream<Workspace> {
    input.map(|dir| {
        let dir = fs::canonicalize(&*dir.0).unwrap_or_else(|_| dir.0.as_ref().clone());

        let root = find_git_root(&dir);

        let config = dirs::home_dir()
            .unwrap_or_default()
            .join(CONFIG_DIR)
            .join(LED_DIR);

        let editor = match try_become_primary(&config, &root) {
            Some(lock_file) => {
                // Keep the lock alive for the process lifetime.
                std::mem::forget(lock_file);
                true
            }
            None => false,
        };

        Workspace {
            root,
            config,
            primary: editor,
        }
    })
}

/// The closest git root, which is also the workspace
fn find_git_root(start: &Path) -> PathBuf {
    let mut dir = start.to_path_buf();
    let mut root = None;
    loop {
        let git = dir.join(GIT_DIR);
        if git.exists() && git.is_dir() {
            root = Some(dir.clone());
        }
        if !dir.pop() {
            break;
        }
    }
    root.unwrap_or_else(|| start.to_path_buf())
}

/// Try to acquire the primary-editor lock for this workspace.
///
/// Returns `Some(File)` if we became primary (the caller must keep the File
/// alive for the whole process lifetime — dropping it releases the lock).
/// Returns `None` if another editor already holds the lock.
fn try_become_primary(config: &Path, root: &Path) -> Option<File> {
    use std::hash::{Hash, Hasher};
    use std::os::unix::io::AsRawFd;

    let lock_dir = config.join(PRIMARY_DIR);
    std::fs::create_dir_all(&lock_dir).ok()?;

    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(lock_dir.join(&hash))
        .ok()?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 { Some(file) } else { None }
}
