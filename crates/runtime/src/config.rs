//! TOML config loader.
//!
//! Reads `<config-dir>/config.toml`, merges its `[keys]` section into
//! the default keymap, and returns the result. Called synchronously
//! by `main.rs` before raw mode is acquired — parse errors surface
//! on a cooked terminal.
//!
//! File shape (whole file optional):
//!
//! ```toml
//! [keys]
//! "ctrl-q" = "quit"
//! "ctrl-w" = "tab.next"
//! ```
//!
//! Unknown key strings, unknown command strings, and malformed TOML
//! are all reported as `ConfigError` with source context.

use std::fs;
use std::path::{Path, PathBuf};

use crate::keymap::{default_keymap, parse_command, parse_key, Keymap};

/// Failure modes surfaced back to the binary. `Display` produces
/// human-readable messages suitable for `eprintln!` — no source-chain
/// boilerplate, since the binary wants a single line.
#[derive(Debug)]
pub enum ConfigError {
    /// The file existed but could not be read (I/O error).
    Io {
        path: PathBuf,
        message: String,
    },
    /// The file existed but the top-level TOML could not be parsed.
    Toml {
        path: PathBuf,
        message: String,
    },
    /// A `[keys]` entry referenced an unknown key string.
    UnknownKey {
        path: PathBuf,
        key: String,
        message: String,
    },
    /// A `[keys]` entry referenced an unknown command string.
    UnknownCommand {
        path: PathBuf,
        key: String,
        command: String,
        message: String,
    },
    /// The `[keys]` section held a non-string value for some key.
    NonStringBinding {
        path: PathBuf,
        key: String,
    },
    /// Any other non-`[keys]` top-level table type problem.
    SchemaMismatch {
        path: PathBuf,
        message: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io { path, message } => {
                write!(f, "read {}: {message}", path.display())
            }
            ConfigError::Toml { path, message } => {
                write!(f, "parse {}: {message}", path.display())
            }
            ConfigError::UnknownKey {
                path,
                key,
                message,
            } => write!(
                f,
                "{}: [keys] entry `{key}`: {message}",
                path.display()
            ),
            ConfigError::UnknownCommand {
                path,
                key,
                command,
                message,
            } => write!(
                f,
                "{}: [keys] entry `{key}` → `{command}`: {message}",
                path.display()
            ),
            ConfigError::NonStringBinding { path, key } => write!(
                f,
                "{}: [keys] entry `{key}` must be a string command name",
                path.display()
            ),
            ConfigError::SchemaMismatch { path, message } => {
                write!(f, "{}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Build the runtime keymap.
///
/// Resolution:
/// 1. Start with [`default_keymap`].
/// 2. Look for `config.toml` in `config_dir` (if given) or in the
///    platform config dir (`~/.config/led/` on Linux/macOS).
/// 3. If a file is found, merge its `[keys]` section on top.
/// 4. If no file is found anywhere, return the defaults silently —
///    having no config is the common case.
pub fn load_keymap(config_dir: Option<&Path>) -> Result<Keymap, ConfigError> {
    let mut keymap = default_keymap();
    let Some(path) = discover_config(config_dir) else {
        return Ok(keymap);
    };
    let source = fs::read_to_string(&path).map_err(|e| ConfigError::Io {
        path: path.clone(),
        message: e.to_string(),
    })?;
    apply_toml(&mut keymap, &path, &source)?;
    Ok(keymap)
}

fn discover_config(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = explicit {
        let candidate = dir.join("config.toml");
        return candidate.exists().then_some(candidate);
    }
    let base = dirs::config_dir()?.join("led");
    let candidate = base.join("config.toml");
    candidate.exists().then_some(candidate)
}

fn apply_toml(keymap: &mut Keymap, path: &Path, source: &str) -> Result<(), ConfigError> {
    let value: toml::Value = source.parse().map_err(|e: toml::de::Error| ConfigError::Toml {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let root = match value {
        toml::Value::Table(t) => t,
        _ => {
            return Err(ConfigError::SchemaMismatch {
                path: path.to_path_buf(),
                message: "top level must be a TOML table".into(),
            })
        }
    };

    let Some(keys_value) = root.get("keys") else {
        return Ok(());
    };
    let keys_table = match keys_value {
        toml::Value::Table(t) => t,
        _ => {
            return Err(ConfigError::SchemaMismatch {
                path: path.to_path_buf(),
                message: "`keys` must be a table".into(),
            })
        }
    };

    for (key_str, cmd_value) in keys_table {
        let cmd_str = match cmd_value.as_str() {
            Some(s) => s,
            None => {
                return Err(ConfigError::NonStringBinding {
                    path: path.to_path_buf(),
                    key: key_str.to_string(),
                })
            }
        };
        let key = parse_key(key_str).map_err(|message| ConfigError::UnknownKey {
            path: path.to_path_buf(),
            key: key_str.to_string(),
            message,
        })?;
        let cmd = parse_command(cmd_str).map_err(|message| ConfigError::UnknownCommand {
            path: path.to_path_buf(),
            key: key_str.to_string(),
            command: cmd_str.to_string(),
            message,
        })?;
        keymap.insert(key, cmd);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Command;
    use std::io::Write;

    struct TempDir(PathBuf);
    fn tempdir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir();
        let unique = format!("led-runtime-config-{}-{}", std::process::id(), n);
        let p = base.join(unique);
        std::fs::create_dir_all(&p).expect("tempdir create");
        TempDir(p)
    }
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_config(dir: &TempDir, body: &str) -> PathBuf {
        let p = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn missing_file_returns_defaults() {
        let tmp = tempdir();
        // No config.toml inside.
        let keymap = load_keymap(Some(tmp.path())).unwrap();
        // A default binding is still there.
        assert_eq!(
            keymap.lookup(&parse_key("ctrl-c").unwrap()),
            Some(Command::Quit)
        );
    }

    #[test]
    fn user_overrides_merge_on_top_of_defaults() {
        let tmp = tempdir();
        write_config(
            &tmp,
            r#"
[keys]
"ctrl-q" = "quit"
"ctrl-w" = "tab.next"
"#,
        );
        let keymap = load_keymap(Some(tmp.path())).unwrap();
        assert_eq!(
            keymap.lookup(&parse_key("ctrl-q").unwrap()),
            Some(Command::Quit)
        );
        assert_eq!(
            keymap.lookup(&parse_key("ctrl-w").unwrap()),
            Some(Command::TabNext)
        );
        // Defaults still present.
        assert_eq!(
            keymap.lookup(&parse_key("ctrl-s").unwrap()),
            Some(Command::Save)
        );
    }

    #[test]
    fn user_override_replaces_default_binding() {
        let tmp = tempdir();
        write_config(
            &tmp,
            r#"
[keys]
"ctrl-c" = "save"
"#,
        );
        let keymap = load_keymap(Some(tmp.path())).unwrap();
        assert_eq!(
            keymap.lookup(&parse_key("ctrl-c").unwrap()),
            Some(Command::Save)
        );
    }

    #[test]
    fn malformed_toml_errors() {
        let tmp = tempdir();
        write_config(&tmp, "[keys\n\"ctrl-q\" = \"quit\"\n");
        let e = load_keymap(Some(tmp.path())).unwrap_err();
        assert!(matches!(e, ConfigError::Toml { .. }), "got {e:?}");
    }

    #[test]
    fn unknown_key_errors() {
        let tmp = tempdir();
        write_config(
            &tmp,
            r#"
[keys]
"ctrl-nope-bogus" = "quit"
"#,
        );
        let e = load_keymap(Some(tmp.path())).unwrap_err();
        match e {
            ConfigError::UnknownKey { key, .. } => assert_eq!(key, "ctrl-nope-bogus"),
            other => panic!("expected UnknownKey, got {other:?}"),
        }
    }

    #[test]
    fn unknown_command_errors() {
        let tmp = tempdir();
        write_config(
            &tmp,
            r#"
[keys]
"ctrl-q" = "explode"
"#,
        );
        let e = load_keymap(Some(tmp.path())).unwrap_err();
        match e {
            ConfigError::UnknownCommand { command, .. } => assert_eq!(command, "explode"),
            other => panic!("expected UnknownCommand, got {other:?}"),
        }
    }

    #[test]
    fn non_string_binding_errors() {
        let tmp = tempdir();
        write_config(
            &tmp,
            r#"
[keys]
"ctrl-q" = 42
"#,
        );
        let e = load_keymap(Some(tmp.path())).unwrap_err();
        assert!(matches!(e, ConfigError::NonStringBinding { .. }));
    }

    #[test]
    fn empty_file_is_fine() {
        let tmp = tempdir();
        write_config(&tmp, "");
        let keymap = load_keymap(Some(tmp.path())).unwrap();
        assert_eq!(
            keymap.lookup(&parse_key("ctrl-c").unwrap()),
            Some(Command::Quit)
        );
    }
}
