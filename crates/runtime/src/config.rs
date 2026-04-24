//! TOML config loader.
//!
//! Reads `<config-dir>/keys.toml`, merges its `[keys]` section into
//! the default keymap, and returns the result. Called synchronously
//! by `main.rs` before raw mode is acquired — parse errors surface
//! on a cooked terminal.
//!
//! File name and format match legacy led exactly so user configs
//! port over without change:
//!
//! ```toml
//! [keys]
//! "ctrl+q" = "quit"
//! "ctrl+w" = "next_tab"
//! ```
//!
//! Modifier separator is `+`; action names are `snake_case`. See
//! `keymap.rs` for the full vocabulary.
//!
//! Unknown key strings, unknown command strings, and malformed TOML
//! are all reported as `ConfigError` with source context.

use std::fs;
use std::path::{Path, PathBuf};

use crate::keymap::{default_keymap, parse_command, parse_key, Keymap};

/// Result of [`load_keymap`]: the merged keymap plus any
/// per-binding warnings that were non-fatal (unknown key, unknown
/// command, malformed binding shape).
///
/// Unknown bindings are deliberately non-fatal during the rewrite
/// period: a legacy user config is expected to reference dozens of
/// commands the rewrite hasn't implemented yet, and a single
/// `prev_issue` shouldn't take down the whole config. Legacy is
/// stricter — it throws out the entire file on one bad entry;
/// revisit that choice when the rewrite is complete.
#[derive(Debug, Default)]
pub struct LoadedKeymap {
    pub keymap: Keymap,
    pub warnings: Vec<String>,
}

/// Fatal config failures: bad file I/O or TOML that won't parse at
/// the top level. Per-binding errors land in `LoadedKeymap.warnings`
/// instead.
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
    /// Any non-`[keys]` top-level table type problem.
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
/// 2. Look for `config.toml` in `config_dir` (if given), then in
///    `$XDG_CONFIG_HOME/led/` (if set), otherwise `~/.config/led/`.
///    This is uniform across Linux / macOS / Windows — we skip
///    `dirs::config_dir()` because its macOS path
///    (`~/Library/Application Support`) is inappropriate for a
///    terminal-first editor.
/// 3. If a file is found, merge its `[keys]` section on top.
/// 4. If no file is found anywhere, return the defaults silently —
///    having no config is the common case.
pub fn load_keymap(config_dir: Option<&Path>) -> Result<LoadedKeymap, ConfigError> {
    let mut loaded = LoadedKeymap {
        keymap: default_keymap(),
        warnings: Vec::new(),
    };
    let Some(path) = discover_config(config_dir) else {
        return Ok(loaded);
    };
    let source = fs::read_to_string(&path).map_err(|e| ConfigError::Io {
        path: path.clone(),
        message: e.to_string(),
    })?;
    apply_toml(&mut loaded, &path, &source)?;
    Ok(loaded)
}

/// Resolve the config file path.
///
/// Deliberately CLI-tool convention, not Apple convention:
/// `$XDG_CONFIG_HOME/led/config.toml` if set, otherwise
/// `$HOME/.config/led/config.toml` on every platform. `dirs::config_dir`
/// is NOT used — on macOS it returns
/// `~/Library/Application Support`, which is surprising for a
/// terminal editor.
fn discover_config(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = explicit {
        let candidate = dir.join("keys.toml");
        return candidate.exists().then_some(candidate);
    }
    let base = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("led")
    } else {
        dirs::home_dir()?.join(".config").join("led")
    };
    let candidate = base.join("keys.toml");
    candidate.exists().then_some(candidate)
}

fn apply_toml(
    loaded: &mut LoadedKeymap,
    path: &Path,
    source: &str,
) -> Result<(), ConfigError> {
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

    for (key_str, value) in keys_table {
        let prefix_key = match parse_key(key_str) {
            Ok(k) => k,
            Err(message) => {
                loaded
                    .warnings
                    .push(format!("[keys] `{key_str}`: {message} (skipped)"));
                continue;
            }
        };

        match value {
            // Direct: "ctrl+q" = "quit"
            toml::Value::String(cmd_str) => match parse_command(cmd_str) {
                Ok(cmd) => loaded.keymap.insert_direct(prefix_key, cmd),
                Err(message) => loaded.warnings.push(format!(
                    "[keys] `{key_str}` = `{cmd_str}`: {message} (skipped)"
                )),
            },
            // Chord: [keys."ctrl+x"] "ctrl+s" = "save"
            toml::Value::Table(chord_table) => {
                for (second_str, second_value) in chord_table {
                    let cmd_str = match second_value.as_str() {
                        Some(s) => s,
                        None => {
                            loaded.warnings.push(format!(
                                "[keys.\"{key_str}\"] `{second_str}`: value must be a string (skipped)"
                            ));
                            continue;
                        }
                    };
                    let second = match parse_key(second_str) {
                        Ok(k) => k,
                        Err(message) => {
                            loaded.warnings.push(format!(
                                "[keys.\"{key_str}\"] `{second_str}`: {message} (skipped)"
                            ));
                            continue;
                        }
                    };
                    match parse_command(cmd_str) {
                        Ok(cmd) => loaded.keymap.insert_chord(prefix_key, second, cmd),
                        Err(message) => loaded.warnings.push(format!(
                            "[keys.\"{key_str}\"] `{second_str}` = `{cmd_str}`: {message} (skipped)"
                        )),
                    }
                }
            }
            _ => loaded.warnings.push(format!(
                "[keys] `{key_str}`: value must be a string (direct) or a table (chord) (skipped)"
            )),
        }
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
        let p = dir.path().join("keys.toml");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn missing_file_returns_defaults() {
        let tmp = tempdir();
        // No keys.toml inside.
        let keymap = load_keymap(Some(tmp.path())).unwrap().keymap;
        // A default direct binding is still there.
        assert_eq!(
            keymap.lookup_direct(&parse_key("up").unwrap()),
            Some(Command::CursorUp)
        );
        // And the default chord.
        assert_eq!(
            keymap.lookup_chord(
                &parse_key("ctrl+x").unwrap(),
                &parse_key("ctrl+c").unwrap(),
            ),
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
"ctrl+q" = "quit"
"ctrl+w" = "next_tab"
"#,
        );
        let keymap = load_keymap(Some(tmp.path())).unwrap().keymap;
        assert_eq!(
            keymap.lookup_direct(&parse_key("ctrl+q").unwrap()),
            Some(Command::Quit)
        );
        assert_eq!(
            keymap.lookup_direct(&parse_key("ctrl+w").unwrap()),
            Some(Command::TabNext)
        );
        // Default direct binding still present.
        assert_eq!(
            keymap.lookup_direct(&parse_key("up").unwrap()),
            Some(Command::CursorUp)
        );
        // Default chord still present.
        assert_eq!(
            keymap.lookup_chord(
                &parse_key("ctrl+x").unwrap(),
                &parse_key("ctrl+s").unwrap(),
            ),
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
"esc" = "save"
"#,
        );
        let keymap = load_keymap(Some(tmp.path())).unwrap().keymap;
        assert_eq!(
            keymap.lookup_direct(&parse_key("esc").unwrap()),
            Some(Command::Save)
        );
    }

    #[test]
    fn malformed_toml_errors() {
        let tmp = tempdir();
        write_config(&tmp, "[keys\n\"ctrl+q\" = \"quit\"\n");
        let e = load_keymap(Some(tmp.path())).unwrap_err();
        assert!(matches!(e, ConfigError::Toml { .. }), "got {e:?}");
    }

    #[test]
    fn unknown_key_is_a_warning_not_a_fatal() {
        let tmp = tempdir();
        write_config(
            &tmp,
            r#"
[keys]
"ctrl+nope+bogus" = "quit"
"ctrl+q" = "quit"
"#,
        );
        let loaded = load_keymap(Some(tmp.path())).unwrap();
        // Bad line surfaced as a warning.
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains("ctrl+nope+bogus"),
            "{:?}",
            loaded.warnings
        );
        // Good line still applied.
        assert_eq!(
            loaded
                .keymap
                .lookup_direct(&parse_key("ctrl+q").unwrap()),
            Some(Command::Quit)
        );
    }

    #[test]
    fn unknown_command_is_a_warning_not_a_fatal() {
        let tmp = tempdir();
        write_config(
            &tmp,
            r#"
[keys]
"ctrl+q" = "explode"
"ctrl+w" = "next_tab"
"#,
        );
        let loaded = load_keymap(Some(tmp.path())).unwrap();
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains("explode"),
            "{:?}",
            loaded.warnings
        );
        assert_eq!(
            loaded
                .keymap
                .lookup_direct(&parse_key("ctrl+w").unwrap()),
            Some(Command::TabNext)
        );
        // Bogus binding not applied.
        assert_eq!(
            loaded
                .keymap
                .lookup_direct(&parse_key("ctrl+q").unwrap()),
            None
        );
    }

    #[test]
    fn non_string_binding_is_a_warning_not_a_fatal() {
        let tmp = tempdir();
        write_config(
            &tmp,
            r#"
[keys]
"ctrl+q" = 42
"#,
        );
        let loaded = load_keymap(Some(tmp.path())).unwrap();
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("string"));
    }

    #[test]
    fn legacy_config_loads_with_many_warnings() {
        // A real-world scenario: user still has the legacy keys.toml
        // with commands the rewrite doesn't know yet. The rewrite's
        // own defaults stay in effect, plus whatever the user wrote
        // that we *do* understand. All unknowns are warnings.
        //
        // `next_issue` / `prev_issue` became known commands in M20a;
        // `open_file_search` in M14. None of the entries in this
        // fixture are still unknown, so warnings = 0.
        let tmp = tempdir();
        write_config(
            &tmp,
            r#"
[keys]
"alt+," = "prev_issue"
"alt+." = "next_issue"
"ctrl+f" = "open_file_search"
"up" = "move_up"
"#,
        );
        let loaded = load_keymap(Some(tmp.path())).unwrap();
        assert_eq!(loaded.warnings.len(), 0);
        // Known one applied.
        assert_eq!(
            loaded
                .keymap
                .lookup_direct(&parse_key("up").unwrap()),
            Some(Command::CursorUp)
        );
    }

    #[test]
    fn xdg_config_home_env_is_honoured() {
        // Point XDG_CONFIG_HOME at our tempdir, drop keys.toml into
        // `<tmp>/led/`, and confirm discover_config finds it without
        // a CLI --config-dir hint.
        let tmp = tempdir();
        let led_dir = tmp.path().join("led");
        std::fs::create_dir_all(&led_dir).unwrap();
        let file = led_dir.join("keys.toml");
        let mut f = std::fs::File::create(&file).unwrap();
        f.write_all(br#"[keys]
"ctrl+q" = "quit"
"#)
            .unwrap();

        // SAFETY: test is single-threaded within its process slice
        // thanks to the env guard — but env mutation is inherently
        // process-global. We save + restore to be polite to other
        // tests in the same binary.
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: env mutation in test; see note above.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };

        let keymap = load_keymap(None).unwrap().keymap;
        assert_eq!(
            keymap.lookup(&parse_key("ctrl+q").unwrap()),
            Some(Command::Quit)
        );

        // Restore.
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
    }

    #[test]
    fn empty_file_is_fine() {
        let tmp = tempdir();
        write_config(&tmp, "");
        let keymap = load_keymap(Some(tmp.path())).unwrap().keymap;
        // Defaults still intact — ctrl+x ctrl+c quits in M6.
        assert_eq!(
            keymap
                .lookup_chord(
                    &parse_key("ctrl+x").unwrap(),
                    &parse_key("ctrl+c").unwrap(),
                ),
            Some(Command::Quit)
        );
    }

    // ── M6: nested chord parsing ────────────────────────────────────────

    #[test]
    fn nested_chord_sub_table_parses() {
        let tmp = tempdir();
        write_config(
            &tmp,
            r#"
[keys."ctrl+x"]
"ctrl+s" = "save"
"k" = "kill_buffer"
"#,
        );
        let keymap = load_keymap(Some(tmp.path())).unwrap().keymap;
        assert_eq!(
            keymap.lookup_chord(
                &parse_key("ctrl+x").unwrap(),
                &parse_key("ctrl+s").unwrap(),
            ),
            Some(Command::Save)
        );
        assert_eq!(
            keymap.lookup_chord(
                &parse_key("ctrl+x").unwrap(),
                &parse_key("k").unwrap(),
            ),
            Some(Command::KillBuffer)
        );
    }

    #[test]
    fn unknown_chord_key_is_a_warning() {
        let tmp = tempdir();
        write_config(
            &tmp,
            r#"
[keys."ctrl+x"]
"ctrl+garbage+key" = "quit"
"ctrl+s" = "save"
"#,
        );
        let loaded = load_keymap(Some(tmp.path())).unwrap();
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains("ctrl+garbage+key"),
            "{:?}",
            loaded.warnings
        );
        // Good line still applied.
        assert_eq!(
            loaded.keymap.lookup_chord(
                &parse_key("ctrl+x").unwrap(),
                &parse_key("ctrl+s").unwrap(),
            ),
            Some(Command::Save)
        );
    }

    #[test]
    fn unknown_chord_command_is_a_warning() {
        let tmp = tempdir();
        write_config(
            &tmp,
            r#"
[keys."ctrl+x"]
"ctrl+s" = "explode"
"k" = "kill_buffer"
"#,
        );
        let loaded = load_keymap(Some(tmp.path())).unwrap();
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains("explode"),
            "{:?}",
            loaded.warnings
        );
        // Good line still applied; bogus not.
        assert_eq!(
            loaded.keymap.lookup_chord(
                &parse_key("ctrl+x").unwrap(),
                &parse_key("k").unwrap(),
            ),
            Some(Command::KillBuffer)
        );
    }

    #[test]
    fn mix_of_direct_and_chord_under_keys_works() {
        let tmp = tempdir();
        write_config(
            &tmp,
            r#"
[keys]
"ctrl+q" = "quit"
"alt+a" = "save_all"

[keys."ctrl+x"]
"ctrl+s" = "save"
"#,
        );
        let keymap = load_keymap(Some(tmp.path())).unwrap().keymap;
        assert_eq!(
            keymap.lookup_direct(&parse_key("ctrl+q").unwrap()),
            Some(Command::Quit)
        );
        assert_eq!(
            keymap.lookup_direct(&parse_key("alt+a").unwrap()),
            Some(Command::SaveAll)
        );
        assert_eq!(
            keymap.lookup_chord(
                &parse_key("ctrl+x").unwrap(),
                &parse_key("ctrl+s").unwrap(),
            ),
            Some(Command::Save)
        );
    }
}
