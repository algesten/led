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

    /// User-typed CLI arg paths, parallel to `arg_paths`. Preserves the
    /// original (pre-canonicalization) names so the buffer constructor
    /// can resolve their full symlink chain — needed for correct syntax
    /// and LSP language detection on symlinked dotfiles like
    /// `~/.profile -> ~/dotfiles/profile`.
    pub arg_user_paths: Vec<UserPath>,

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

    /// Override the `gh` CLI binary path (testing only).
    pub test_gh_binary: Option<UserPath>,

    /// Append a normalized one-line-per-dispatch trace to this file.
    /// Used by the goldens runner to snapshot externally-observable
    /// work led performs. Off in production. See `docs/rewrite/GOLDENS-PLAN.md`.
    pub golden_trace: Option<std::path::PathBuf>,

    /// Standalone (no-workspace) mode. Intended for `$EDITOR` use — e.g.
    /// `EDITOR="led --no-workspace"` for git commit messages and similar
    /// single-file edits where loading the surrounding project is wrong.
    ///
    /// In this mode:
    /// - No git root is detected; no workspace is loaded.
    /// - `AppState.workspace` is `WorkspaceState::Standalone` and never
    ///   transitions to `Loaded`.
    /// - No session is read or written (the DB is not opened, no flock).
    /// - No recursive watcher is registered on a project root.
    /// - Git/LSP/find-in-files stay dormant (they key on
    ///   `WorkspaceState::Loaded`).
    /// - The file browser sidebar is visible and rooted at the process
    ///   CWD (captured into `start_dir`), not the file argument's
    ///   parent. This matters for `$EDITOR` use: when git invokes
    ///   `led --no-workspace .git/COMMIT_EDITMSG` from the project
    ///   root, the browser shows the project instead of `.git/`.
    pub no_workspace: bool,
}
