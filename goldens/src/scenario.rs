//! Declarative golden scenarios: `setup.toml` + `script.txt` per directory.
//!
//! A scenario directory contains:
//!   setup.toml       — initial conditions (files, terminal size, flags)
//!   script.txt       — sequence of input commands
//!   frame.snap       — captured rendered terminal grid (generated)
//!   dispatched.snap  — captured normalized trace (generated)
//!
//! See `docs/rewrite/GOLDENS-PLAN.md`.

use std::path::Path;

use serde::Deserialize;

use crate::{GoldenRunner, GoldenRunnerBuilder};

#[derive(Debug, Deserialize, Default)]
pub struct Setup {
    #[serde(default)]
    pub terminal: TerminalSetup,
    #[serde(default, rename = "file")]
    pub files: Vec<FileSpec>,
    #[serde(default)]
    pub no_workspace: bool,
    /// Initialize a `.git/` dir in the workspace so led detects it as a
    /// project root (required for LSP, git, and session features).
    #[serde(default)]
    pub git_init: bool,
    /// Inline TOML representation of `.fake-lsp.json`. Re-serialized as
    /// JSON for the fake-lsp binary. When present, led runs with
    /// `--test-lsp-server <fake-lsp-path>`.
    #[serde(default)]
    pub fake_lsp: Option<toml::Value>,
    /// Inline TOML representation of `.fake-gh.json`.
    #[serde(default)]
    pub fake_gh: Option<toml::Value>,
    /// User-config files to seed in the isolated config dir before led
    /// starts. Lets axis-4 (per-config-key) goldens override defaults.
    /// NOTE: led replaces defaults wholesale (no merge), so a custom
    /// `keys.toml` must include every binding the scenario relies on.
    #[serde(default)]
    pub config: ConfigSetup,
}

#[derive(Debug, Deserialize, Default)]
pub struct ConfigSetup {
    /// Literal TOML written to `<config_dir>/keys.toml`.
    #[serde(default)]
    pub keys: Option<String>,
    /// Literal TOML written to `<config_dir>/theme.toml`.
    #[serde(default)]
    pub theme: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TerminalSetup {
    pub cols: u16,
    pub rows: u16,
}

impl Default for TerminalSetup {
    fn default() -> Self {
        // Match the user's typical terminal so goldens reflect real-use
        // layout (sidebar widths, gutter alignment, status bar wrapping).
        Self { cols: 120, rows: 40 }
    }
}

#[derive(Debug, Deserialize)]
pub struct FileSpec {
    pub path: String,
    pub contents: String,
}

#[derive(Debug)]
pub enum ScriptStep {
    /// One or more chords (length > 1 is an Emacs-style chord prefix).
    Press(Vec<String>),
    /// Literal text (rest of the line after `type `).
    Type(String),
    /// Wall-clock pause in milliseconds. Used for async-driver scenarios
    /// (LSP, git, gh) where the dispatch happens silently after a delay
    /// and settle's quiescence detection would return prematurely. Will
    /// become virtual when --test-clock lands.
    Wait(std::time::Duration),
    /// Write `contents` to `path` (workspace-relative) mid-scenario.
    /// Triggers external-change reactions when watchers are on.
    /// Backslash escapes in `contents`: `\n`, `\t`, `\\`.
    FsWrite { path: String, contents: String },
    /// Delete `path` (workspace-relative) mid-scenario. Triggers
    /// external-remove reactions when watchers are on.
    FsDelete { path: String },
}

pub fn parse_script(src: &str) -> Vec<ScriptStep> {
    let mut steps = Vec::new();
    for (lineno, raw) in src.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("press ") {
            let chords: Vec<String> = rest.split_whitespace().map(str::to_string).collect();
            if chords.is_empty() {
                panic!("script line {}: 'press' with no chord", lineno + 1);
            }
            steps.push(ScriptStep::Press(chords));
        } else if let Some(rest) = line.strip_prefix("type ") {
            steps.push(ScriptStep::Type(rest.to_string()));
        } else if line == "type" {
            steps.push(ScriptStep::Type(String::new()));
        } else if let Some(rest) = line.strip_prefix("wait ") {
            let trimmed = rest.trim();
            let ms: u64 = if let Some(n) = trimmed.strip_suffix("ms") {
                n.trim().parse().unwrap_or_else(|e| {
                    panic!("script line {}: parse wait {trimmed:?}: {e}", lineno + 1)
                })
            } else if let Some(n) = trimmed.strip_suffix("s") {
                let secs: u64 = n.trim().parse().unwrap_or_else(|e| {
                    panic!("script line {}: parse wait {trimmed:?}: {e}", lineno + 1)
                });
                secs * 1000
            } else {
                panic!(
                    "script line {}: wait needs unit ms or s (e.g. 'wait 500ms')",
                    lineno + 1
                );
            };
            steps.push(ScriptStep::Wait(std::time::Duration::from_millis(ms)));
        } else if let Some(rest) = line.strip_prefix("fs_write ") {
            let (path, contents) = rest.split_once(char::is_whitespace).unwrap_or_else(|| {
                panic!(
                    "script line {}: fs_write needs <path> <text>",
                    lineno + 1
                )
            });
            steps.push(ScriptStep::FsWrite {
                path: path.to_string(),
                contents: unescape(contents.trim_start()),
            });
        } else if let Some(rest) = line.strip_prefix("fs_delete ") {
            steps.push(ScriptStep::FsDelete {
                path: rest.trim().to_string(),
            });
        } else {
            panic!(
                "script line {}: unrecognized command {line:?}. Expected 'press <chord...>', 'type <text>', 'wait <N>ms', 'fs_write <path> <text>', or 'fs_delete <path>'.",
                lineno + 1
            );
        }
    }
    steps
}

/// Build a runner from `setup.toml`, drive it with `script.txt`, then
/// assert the captured frame and trace against `frame.snap` and
/// `dispatched.snap` in the same directory.
pub fn run(scenario_dir: &Path) {
    let setup_path = scenario_dir.join("setup.toml");
    let setup_src = std::fs::read_to_string(&setup_path).unwrap_or_else(|e| {
        panic!(
            "read {} ({e}). Every scenario must have a setup.toml.",
            setup_path.display()
        )
    });
    let setup: Setup = toml::from_str(&setup_src).unwrap_or_else(|e| {
        panic!("parse {} ({e})", setup_path.display())
    });

    let script_src =
        std::fs::read_to_string(scenario_dir.join("script.txt")).unwrap_or_default();
    let steps = parse_script(&script_src);

    let mut builder = GoldenRunnerBuilder::new().with_viewport(
        setup.terminal.cols,
        setup.terminal.rows,
    );
    if setup.no_workspace {
        builder = builder.with_no_workspace();
    }
    if setup.git_init {
        builder = builder.with_git_init();
    }
    if let Some(v) = &setup.fake_lsp {
        builder = builder.with_fake_lsp_json(toml_to_json(v));
    }
    if let Some(v) = &setup.fake_gh {
        builder = builder.with_fake_gh_json(toml_to_json(v));
    }
    if let Some(s) = &setup.config.keys {
        builder = builder.with_config_keys(s.clone());
    }
    if let Some(s) = &setup.config.theme {
        builder = builder.with_config_theme(s.clone());
    }
    for f in &setup.files {
        builder = builder.with_file(&f.path, &f.contents);
    }
    let mut runner = builder.spawn();

    drive(&mut runner, &steps);

    runner.assert_frame(scenario_dir);
    runner.assert_trace(scenario_dir);
}

/// Interpret backslash escapes `\n`, `\t`, `\\` in script text payloads.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn toml_to_json(v: &toml::Value) -> String {
    // toml::Value → serde_json::Value via serialize/deserialize.
    let json: serde_json::Value =
        serde_json::to_value(v).expect("toml→json roundtrip");
    serde_json::to_string(&json).expect("serialize json")
}

fn drive(runner: &mut GoldenRunner, steps: &[ScriptStep]) {
    for step in steps {
        match step {
            ScriptStep::Press(chords) => {
                let refs: Vec<&str> = chords.iter().map(String::as_str).collect();
                if refs.len() == 1 {
                    runner.press(refs[0]);
                } else {
                    runner.press_seq(&refs);
                }
            }
            ScriptStep::Type(text) => runner.type_text(text),
            ScriptStep::Wait(d) => runner.wait_then_settle(*d),
            ScriptStep::FsWrite { path, contents } => {
                runner.fs_write(path, contents);
            }
            ScriptStep::FsDelete { path } => {
                runner.fs_delete(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_script_basic() {
        let s = "press Ctrl-s\ntype hello world\npress Ctrl-x Ctrl-s\n";
        let steps = parse_script(s);
        assert_eq!(steps.len(), 3);
        match &steps[0] {
            ScriptStep::Press(c) => assert_eq!(c, &["Ctrl-s"]),
            _ => panic!(),
        }
        match &steps[1] {
            ScriptStep::Type(t) => assert_eq!(t, "hello world"),
            _ => panic!(),
        }
        match &steps[2] {
            ScriptStep::Press(c) => assert_eq!(c, &["Ctrl-x", "Ctrl-s"]),
            _ => panic!(),
        }
    }

    #[test]
    fn parse_script_skips_blank_and_comments() {
        let s = "\n# a comment\npress Down\n   \n# another\n";
        let steps = parse_script(s);
        assert_eq!(steps.len(), 1);
    }

    #[test]
    fn parse_setup_minimal() {
        let s = r#"
            no_workspace = true
            [[file]]
            path = "hello.txt"
            contents = "hi\n"
        "#;
        let setup: Setup = toml::from_str(s).unwrap();
        assert!(setup.no_workspace);
        assert_eq!(setup.terminal.cols, 120);
        assert_eq!(setup.terminal.rows, 40);
        assert_eq!(setup.files.len(), 1);
        assert_eq!(setup.files[0].path, "hello.txt");
    }
}
