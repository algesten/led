//! TOML `theme.toml` loader.
//!
//! Resolves a [`Theme`] from either an explicit path (`--theme`), a
//! file in the user's config directory, or the built-in legacy
//! default (the hard-coded chrome that shipped before M14b).
//!
//! The TOML shape uses one table per chrome region. Each region is a
//! table accepting any subset of `fg` / `bg` / `bold` / `reverse` /
//! `underline`. Colors are named (`"red"`, `"cyan"`, ...) or 24-bit
//! hex (`"#cd0000"`). Missing region → default (unstyled) slot.
//!
//! ```toml
//! [chrome.tab_active]
//! reverse = true
//!
//! [chrome.status_warn]
//! fg = "white"
//! bg = "#cd0000"
//! bold = true
//!
//! [chrome.ruler]
//! bg = "#222222"
//! ruler_column = 110
//! ```
//!
//! Unknown regions / unknown color names surface as non-fatal
//! warnings (same discipline as `keys.toml`).

use std::fs;
use std::path::{Path, PathBuf};

use led_driver_terminal_core::{Color, Style, SyntaxTheme, Theme};

/// Result of [`load_theme`]: the resolved theme plus any non-fatal
/// parse warnings. Unknown region names / unknown color names /
/// malformed style tables are dropped with a warning; the rest of
/// the theme still applies.
#[derive(Debug)]
pub struct LoadedTheme {
    pub theme: Theme,
    pub warnings: Vec<String>,
}

/// Fatal theme-load failures (I/O, top-level TOML parse errors).
/// Per-region schema problems live in `warnings` instead.
#[derive(Debug)]
pub enum ThemeError {
    Io { path: PathBuf, message: String },
    Toml { path: PathBuf, message: String },
    SchemaMismatch { path: PathBuf, message: String },
}

impl std::fmt::Display for ThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThemeError::Io { path, message } => write!(f, "read {}: {message}", path.display()),
            ThemeError::Toml { path, message } => write!(f, "parse {}: {message}", path.display()),
            ThemeError::SchemaMismatch { path, message } => {
                write!(f, "{}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for ThemeError {}

/// Build the runtime theme.
///
/// Resolution:
/// 1. If `explicit_path` is `Some`, load it (error when missing).
/// 2. Otherwise look in `config_dir/theme.toml`, then
///    `$XDG_CONFIG_HOME/led/theme.toml` (or `~/.config/led/` when
///    the env var is unset). Missing file → `Theme::default`.
pub fn load_theme(
    config_dir: Option<&Path>,
    explicit_path: Option<&Path>,
) -> Result<LoadedTheme, ThemeError> {
    let mut loaded = LoadedTheme {
        theme: Theme::default(),
        warnings: Vec::new(),
    };
    let path = match explicit_path {
        Some(p) => Some(p.to_path_buf()),
        None => discover_theme(config_dir),
    };
    let Some(path) = path else {
        return Ok(loaded);
    };
    let source = fs::read_to_string(&path).map_err(|e| ThemeError::Io {
        path: path.clone(),
        message: e.to_string(),
    })?;
    apply_toml(&mut loaded, &path, &source)?;
    Ok(loaded)
}

fn discover_theme(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = explicit {
        let candidate = dir.join("theme.toml");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let base = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("led")
    } else {
        dirs::home_dir()?.join(".config").join("led")
    };
    let candidate = base.join("theme.toml");
    candidate.exists().then_some(candidate)
}

fn apply_toml(
    loaded: &mut LoadedTheme,
    path: &Path,
    source: &str,
) -> Result<(), ThemeError> {
    let value: toml::Value = source.parse().map_err(|e: toml::de::Error| ThemeError::Toml {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let root = match value {
        toml::Value::Table(t) => t,
        _ => {
            return Err(ThemeError::SchemaMismatch {
                path: path.to_path_buf(),
                message: "top level must be a TOML table".into(),
            })
        }
    };
    if let Some(syntax_value) = root.get("syntax") {
        apply_syntax(loaded, path, syntax_value)?;
    }

    let Some(chrome) = root.get("chrome") else {
        return Ok(());
    };
    let chrome = match chrome {
        toml::Value::Table(t) => t,
        _ => {
            return Err(ThemeError::SchemaMismatch {
                path: path.to_path_buf(),
                message: "`chrome` must be a table".into(),
            })
        }
    };

    // `ruler_column` is a flat integer under [chrome], not a region
    // table. Pull it out first so the region loop doesn't trip on
    // "unknown region" for this key.
    if let Some(v) = chrome.get("ruler_column") {
        match v {
            toml::Value::Integer(n) if *n >= 0 => {
                loaded.theme.ruler_column = Some(*n as u16);
            }
            _ => loaded
                .warnings
                .push("[chrome] `ruler_column` must be a non-negative integer (skipped)".into()),
        }
    }

    for (region, style_value) in chrome {
        if region == "ruler_column" {
            continue;
        }
        let style_table = match style_value {
            toml::Value::Table(t) => t,
            _ => {
                loaded.warnings.push(format!(
                    "[chrome] `{region}`: value must be a table (skipped)"
                ));
                continue;
            }
        };
        let style = match parse_style(style_table, "chrome", region, &mut loaded.warnings) {
            Some(s) => s,
            None => continue,
        };
        if !assign_region(&mut loaded.theme, region, style) {
            loaded
                .warnings
                .push(format!("[chrome] `{region}`: unknown region (skipped)"));
        }
    }

    Ok(())
}

/// Ingest the `[syntax]` TOML table, one sub-table per
/// [`TokenKind`]. Same field grammar as `[chrome.<region>]` —
/// `fg` / `bg` / `bold` / `reverse` / `underline`. Unknown kinds
/// produce a warning and are dropped; malformed styles warn and
/// leave the default slot intact.
fn apply_syntax(
    loaded: &mut LoadedTheme,
    path: &Path,
    syntax: &toml::Value,
) -> Result<(), ThemeError> {
    let table = match syntax {
        toml::Value::Table(t) => t,
        _ => {
            return Err(ThemeError::SchemaMismatch {
                path: path.to_path_buf(),
                message: "`syntax` must be a table".into(),
            });
        }
    };
    for (kind, style_value) in table {
        let style_table = match style_value {
            toml::Value::Table(t) => t,
            _ => {
                loaded
                    .warnings
                    .push(format!("[syntax] `{kind}`: value must be a table (skipped)"));
                continue;
            }
        };
        let style = match parse_style(style_table, "syntax", kind, &mut loaded.warnings) {
            Some(s) => s,
            None => continue,
        };
        if !assign_syntax_kind(&mut loaded.theme.syntax, kind, style) {
            loaded
                .warnings
                .push(format!("[syntax] `{kind}`: unknown token kind (skipped)"));
        }
    }
    Ok(())
}

fn assign_syntax_kind(syntax: &mut SyntaxTheme, kind: &str, style: Style) -> bool {
    // `type` is a reserved word in Rust, so the struct field is
    // `type_`; accept both the natural `type` spelling and the
    // underscore variant from TOML for consistency.
    match kind {
        "keyword" => syntax.keyword = style,
        "type" | "type_" => syntax.type_ = style,
        "function" => syntax.function = style,
        "string" => syntax.string = style,
        "number" => syntax.number = style,
        "boolean" => syntax.boolean = style,
        "comment" => syntax.comment = style,
        "operator" => syntax.operator = style,
        "punctuation" => syntax.punctuation = style,
        "variable" => syntax.variable = style,
        "property" => syntax.property = style,
        "attribute" => syntax.attribute = style,
        "tag" => syntax.tag = style,
        "label" => syntax.label = style,
        "constant" => syntax.constant = style,
        "escape" => syntax.escape = style,
        "default" => syntax.default = style,
        _ => return false,
    }
    true
}

fn parse_style(
    table: &toml::map::Map<String, toml::Value>,
    section: &str,
    region: &str,
    warnings: &mut Vec<String>,
) -> Option<Style> {
    let mut style = Style::default();
    for (k, v) in table {
        match k.as_str() {
            "fg" => match parse_color(v) {
                Some(c) => style.fg = Some(c),
                None => warnings.push(format!(
                    "[{section}.{region}] `fg`: unknown color (skipped this field)"
                )),
            },
            "bg" => match parse_color(v) {
                Some(c) => style.bg = Some(c),
                None => warnings.push(format!(
                    "[{section}.{region}] `bg`: unknown color (skipped this field)"
                )),
            },
            "bold" => match v.as_bool() {
                Some(b) => style.attrs.bold = b,
                None => warnings.push(format!(
                    "[{section}.{region}] `bold`: expected boolean (skipped)"
                )),
            },
            "reverse" => match v.as_bool() {
                Some(b) => style.attrs.reverse = b,
                None => warnings.push(format!(
                    "[{section}.{region}] `reverse`: expected boolean (skipped)"
                )),
            },
            "underline" => match v.as_bool() {
                Some(b) => style.attrs.underline = b,
                None => warnings.push(format!(
                    "[{section}.{region}] `underline`: expected boolean (skipped)"
                )),
            },
            other => warnings.push(format!(
                "[{section}.{region}] `{other}`: unknown field (skipped)"
            )),
        }
    }
    Some(style)
}

fn parse_color(v: &toml::Value) -> Option<Color> {
    let s = v.as_str()?;
    // `xNNN` — xterm 256-color palette index (e.g. `x216` = peach).
    // Matches legacy `default_theme.toml`'s syntax.
    if let Some(digits) = s.strip_prefix('x') {
        if let Ok(n) = digits.parse::<u16>()
            && n <= 255
        {
            return Some(Color::Indexed(n as u8));
        }
        return None;
    }
    // Hex `#rrggbb` — 24-bit RGB. Only reliable on truecolor
    // terminals; indexed forms above are the safer default.
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    parse_named_color(s)
}

fn parse_hex_color(hex: &str) -> Option<Color> {
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::rgb(r, g, b))
}

fn parse_named_color(name: &str) -> Option<Color> {
    match name.to_ascii_lowercase().as_str() {
        "black" => Some(Color::BLACK),
        "red" => Some(Color::RED),
        "green" => Some(Color::GREEN),
        "yellow" => Some(Color::YELLOW),
        "blue" => Some(Color::BLUE),
        "magenta" => Some(Color::MAGENTA),
        "cyan" => Some(Color::CYAN),
        "white" => Some(Color::WHITE),
        "grey" | "gray" => Some(Color::GREY),
        "dark_grey" | "dark_gray" | "darkgrey" | "darkgray" => Some(Color::DARK_GREY),
        "bright_red" => Some(Color::BRIGHT_RED),
        "bright_green" => Some(Color::BRIGHT_GREEN),
        "bright_yellow" => Some(Color::BRIGHT_YELLOW),
        "bright_blue" => Some(Color::BRIGHT_BLUE),
        "bright_magenta" => Some(Color::BRIGHT_MAGENTA),
        "bright_cyan" => Some(Color::BRIGHT_CYAN),
        "bright_white" => Some(Color::BRIGHT_WHITE),
        _ => None,
    }
}

fn assign_region(theme: &mut Theme, region: &str, style: Style) -> bool {
    match region {
        "tab_active" => theme.tab_active = style,
        "tab_inactive" => theme.tab_inactive = style,
        "tab_preview" => theme.tab_preview = style,
        "tab_dirty_marker" => theme.tab_dirty_marker = style,
        "status_normal" => theme.status_normal = style,
        "status_warn" => theme.status_warn = style,
        "browser_selected_focused" => theme.browser_selected_focused = style,
        "browser_selected_unfocused" => theme.browser_selected_unfocused = style,
        "browser_chevron" => theme.browser_chevron = style,
        "browser_border" => theme.browser_border = style,
        "search_toggle_on" => theme.search_toggle_on = style,
        "search_match" => theme.search_match = style,
        "search_hit_replaced" => theme.search_hit_replaced = style,
        "cursor_line" => theme.cursor_line = style,
        "ruler" => theme.ruler = style,
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempDir(PathBuf);
    fn tempdir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir();
        let unique = format!("led-theme-test-{}-{}", std::process::id(), n);
        let p = base.join(unique);
        std::fs::create_dir_all(&p).expect("tempdir create");
        TempDir(p)
    }

    /// Guard that points `XDG_CONFIG_HOME` at an empty tempdir so
    /// `discover_theme` doesn't fall through to the developer's real
    /// `~/.config/led/theme.toml`. Every test in this module that
    /// calls `load_theme` without an explicit path must hold one,
    /// otherwise the dev's config leaks in and flakes assertions.
    /// Env vars are process-global, so serialise via a mutex.
    struct XdgGuard {
        prev: Option<std::ffi::OsString>,
        _tmp: TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn xdg_guard() -> XdgGuard {
        use std::sync::{Mutex, OnceLock};
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        let tmp = tempdir();
        // SAFETY: tests hold `lock` while mutating the process
        // environment, and `XdgGuard::drop` restores it.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        }
        XdgGuard {
            prev,
            _tmp: tmp,
            _lock: lock,
        }
    }

    impl Drop for XdgGuard {
        fn drop(&mut self) {
            // SAFETY: `_lock` still held — restore under the same
            // mutex we acquired in `xdg_guard`.
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
        }
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

    fn write_theme(dir: &TempDir, body: &str) -> PathBuf {
        let p = dir.path().join("theme.toml");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn no_file_returns_built_in_default() {
        // Isolate `XDG_CONFIG_HOME` so the developer's real
        // `~/.config/led/theme.toml` can't leak into the test.
        let _xdg = xdg_guard();
        let tmp = tempdir();
        let loaded = load_theme(Some(tmp.path()), None).unwrap();
        assert_eq!(loaded.theme, Theme::default());
        assert!(loaded.warnings.is_empty(), "warnings: {:?}", loaded.warnings);
    }

    #[test]
    fn named_and_hex_colors_both_parse() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[chrome.status_warn]
fg = "white"
bg = "#cd0000"
bold = true
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.status_warn.fg, Some(Color::WHITE));
        assert_eq!(loaded.theme.status_warn.bg, Some(Color::rgb(0xcd, 0, 0)));
        assert!(loaded.theme.status_warn.attrs.bold);
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn unknown_region_is_warned_not_fatal() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r#"
[chrome.goldenrod_tabs]
fg = "yellow"

[chrome.tab_active]
reverse = true
"#,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("goldenrod_tabs"));
        assert!(loaded.theme.tab_active.attrs.reverse);
    }

    #[test]
    fn unknown_color_name_leaves_field_unset_and_warns() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[chrome.status_warn]
fg = "puce"
bg = "#cd0000"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.status_warn.fg, None);
        assert_eq!(loaded.theme.status_warn.bg, Some(Color::rgb(0xcd, 0, 0)));
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("fg"));
    }

    #[test]
    fn ruler_column_is_an_integer_under_chrome() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r#"
[chrome]
ruler_column = 110
"#,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.ruler_column, Some(110));
    }

    #[test]
    fn short_hex_is_rejected() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[chrome.status_warn]
bg = "#abc"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.status_warn.bg, None);
        assert_eq!(loaded.warnings.len(), 1);
    }

    #[test]
    fn xterm_index_syntax_resolves_to_indexed_color() {
        // Legacy `default_theme.toml` used `"$x024"` (via an alias
        // table); theme.toml accepts `"x024"` directly.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r#"
[chrome.status_normal]
fg = "x223"
bg = "x024"
"#,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.status_normal.fg, Some(Color::Indexed(223)));
        assert_eq!(loaded.theme.status_normal.bg, Some(Color::Indexed(24)));
    }

    #[test]
    fn named_color_resolves_to_ansi_palette_index() {
        // Built-in `"red"` → Indexed(1), not RGB. Terminals honour
        // the user's configured palette for the 0-15 range.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r#"
[chrome.status_warn]
fg = "white"
bg = "red"
"#,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.status_warn.fg, Some(Color::Indexed(7)));
        assert_eq!(loaded.theme.status_warn.bg, Some(Color::Indexed(1)));
    }

    #[test]
    fn syntax_section_populates_token_kind_slots() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r##"
[syntax.keyword]
fg = "x170"
bold = true

[syntax.string]
fg = "x107"

[syntax.comment]
fg = "#808080"
"##,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(
            loaded.theme.syntax.keyword.fg,
            Some(Color::Indexed(170))
        );
        assert!(loaded.theme.syntax.keyword.attrs.bold);
        assert_eq!(loaded.theme.syntax.string.fg, Some(Color::Indexed(107)));
        assert_eq!(
            loaded.theme.syntax.comment.fg,
            Some(Color::rgb(0x80, 0x80, 0x80))
        );
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn syntax_type_alias_accepts_both_spellings() {
        // TOML keys can't use a Rust reserved word naturally, but
        // either `type` or `type_` maps to `SyntaxTheme.type_`.
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r#"
[syntax.type]
fg = "x074"
"#,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.theme.syntax.type_.fg, Some(Color::Indexed(74)));
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn unknown_syntax_kind_is_warned_not_fatal() {
        let tmp = tempdir();
        let path = write_theme(
            &tmp,
            r#"
[syntax.neon]
fg = "magenta"

[syntax.keyword]
fg = "yellow"
"#,
        );
        let loaded = load_theme(None, Some(&path)).unwrap();
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("neon"));
        assert_eq!(loaded.theme.syntax.keyword.fg, Some(Color::YELLOW));
    }

    #[test]
    fn malformed_toml_is_a_hard_error() {
        let tmp = tempdir();
        let path = write_theme(&tmp, "[chrome\n");
        let err = load_theme(None, Some(&path)).unwrap_err();
        assert!(matches!(err, ThemeError::Toml { .. }), "got {err:?}");
    }
}
