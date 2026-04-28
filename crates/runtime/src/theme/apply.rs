//! Application of parsed styles to the live [`Theme`].
//!
//! Mutates `Theme` / `SyntaxTheme` / `DiagnosticsTheme` based on
//! parsed [`Style`]s coming from [`super::parse`].

use std::collections::HashSet;
use std::path::Path;

use led_driver_terminal_core::{Style, SyntaxTheme, Theme};

use super::ThemeError;
use super::parse::{StyleTable, parse_style, resolve_string_to_style};

/// Ingest the `[syntax]` TOML table. Each entry can be either a
/// sub-table (`keyword = { fg = "x032", bold = true }`) or a bare
/// string shorthand (`keyword = "$syntax_keyword"`) that legacy
/// themes use extensively — the shorthand is interpreted as an
/// `fg`-only style.
///
/// Legacy themes use dotted keys like `"type.builtin"` /
/// `"string.special"` for finer-grained tree-sitter captures. Our
/// `TokenKind` enum is coarser — we only have one `String` slot
/// covering all string-like captures — so dotted keys collapse
/// onto the base kind. When a theme defines BOTH (e.g. `string`
/// and `"string.special"`) the non-dotted key wins: we process
/// non-dotted keys first and let dotted keys only fill in bases
/// that weren't explicitly set. Otherwise iteration order
/// (alphabetical, via `toml`'s `BTreeMap`) would have
/// `"string.special"` silently overwrite `string`.
pub(super) fn apply_syntax(
    loaded: &mut super::LoadedTheme,
    path: &Path,
    syntax: &toml::Value,
    styles: &StyleTable,
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
    // Two-pass write so a non-dotted entry wins over a dotted
    // sibling that resolves to the same `TokenKind` slot.
    //
    // Pass 1 — authoritative assignments for non-dotted keys
    // (`string`, `keyword`, …). Each one's resolved
    // `TokenKind` is captured so Pass 2 can skip dotted entries
    // that target the same slot.
    //
    // Pass 2 — dotted keys (`string.regex`, `text.title`, …).
    // Both passes route through
    // `led_state_syntax::capture_name_to_kind` — the same
    // dispatch the syntax driver uses — so a theme entry lands
    // in the slot the painter actually reads. Dotted captures
    // that map to a slot already explicitly set in Pass 1 are
    // skipped (legacy parity: `string` wins over `string.regex`).
    let mut explicit_kinds: HashSet<led_state_syntax::TokenKind> = HashSet::new();
    for (kind, style_value) in table {
        if kind.contains('.') {
            continue;
        }
        let Some(style) = resolve_syntax_style(kind, style_value, styles, &mut loaded.warnings)
        else {
            continue;
        };
        if assign_syntax_kind(&mut loaded.theme.syntax, kind, style) {
            if let Some(tk) = led_state_syntax::capture_name_to_kind(kind) {
                explicit_kinds.insert(tk);
            }
        } else {
            loaded
                .warnings
                .push(format!("[syntax] `{kind}`: unknown token kind (skipped)"));
        }
    }
    for (kind, style_value) in table {
        if !kind.contains('.') {
            continue;
        }
        let Some(target_kind) = led_state_syntax::capture_name_to_kind(kind) else {
            loaded
                .warnings
                .push(format!("[syntax] `{kind}`: unknown token kind (skipped)"));
            continue;
        };
        if explicit_kinds.contains(&target_kind) {
            continue;
        }
        let Some(style) = resolve_syntax_style(kind, style_value, styles, &mut loaded.warnings)
        else {
            continue;
        };
        *loaded.theme.syntax.style_mut(target_kind) = style;
    }
    Ok(())
}

/// Resolve one `[syntax]` entry to a `Style`. Handles both the
/// table form (`{ fg = "...", bold = true }`) and the bare-string
/// shorthand (`"$syntax_keyword"` → `{ fg = "$syntax_keyword" }`).
/// Emits warnings for malformed values and returns `None` for them.
fn resolve_syntax_style(
    kind: &str,
    style_value: &toml::Value,
    styles: &StyleTable,
    warnings: &mut Vec<String>,
) -> Option<Style> {
    match style_value {
        toml::Value::Table(t) => parse_style(t, "syntax", kind, styles, warnings),
        toml::Value::String(s) => {
            let Some(style) = resolve_string_to_style(s, styles, &mut HashSet::new()) else {
                warnings.push(format!(
                    "[syntax] `{kind}`: unknown style `{s}` (skipped)",
                ));
                return None;
            };
            Some(style)
        }
        _ => {
            warnings.push(format!(
                "[syntax] `{kind}`: expected table or style string (skipped)"
            ));
            None
        }
    }
}

/// Ingest the `[diagnostics]` table. Four fixed severity keys —
/// `error`, `warning`, `info`, `hint`. Each accepts either the
/// table form (`{ fg = "x196", bold = true }`) or the bare-color
/// shorthand (`"$error"`), matching `[syntax]`. Unknown keys
/// warn and skip.
pub(super) fn apply_diagnostics(
    loaded: &mut super::LoadedTheme,
    path: &Path,
    diag: &toml::Value,
    styles: &StyleTable,
) -> Result<(), ThemeError> {
    let table = match diag {
        toml::Value::Table(t) => t,
        _ => {
            return Err(ThemeError::SchemaMismatch {
                path: path.to_path_buf(),
                message: "`diagnostics` must be a table".into(),
            });
        }
    };
    for (key, value) in table {
        let Some(style) = resolve_syntax_style(key, value, styles, &mut loaded.warnings)
        else {
            continue;
        };
        match key.as_str() {
            "error" => loaded.theme.diagnostics.error = style,
            "warning" => loaded.theme.diagnostics.warning = style,
            "info" => loaded.theme.diagnostics.info = style,
            "hint" => loaded.theme.diagnostics.hint = style,
            other => {
                loaded.warnings.push(format!(
                    "[diagnostics] `{other}`: unknown severity (skipped)"
                ));
            }
        }
    }
    Ok(())
}

/// Route a TOML `[syntax].<kind>` entry into the matching
/// [`SyntaxTheme`] slot. Routes via
/// `led_state_syntax::capture_name_to_kind` — the same dispatch
/// the syntax driver uses to bucket tree-sitter captures into
/// [`TokenKind`]s — so a theme entry lights the slot the painter
/// actually reads.
///
/// `type_` is also accepted as a spelling of `type` because Rust
/// reserves `type`; some users carry the underscore form from
/// experimenting in code-shaped editors.
fn assign_syntax_kind(syntax: &mut SyntaxTheme, kind: &str, style: Style) -> bool {
    let resolved = if kind == "type_" {
        Some(led_state_syntax::TokenKind::Type)
    } else {
        led_state_syntax::capture_name_to_kind(kind)
    };
    let Some(token_kind) = resolved else {
        return false;
    };
    *syntax.style_mut(token_kind) = style;
    true
}

pub(super) fn assign_region(theme: &mut Theme, region: &str, style: Style) -> bool {
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
