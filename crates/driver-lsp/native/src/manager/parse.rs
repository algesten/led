use std::sync::Arc;

use led_state_diagnostics::{Diagnostic, DiagnosticSeverity};
use serde_json::Value;

use crate::protocol::path_from_uri;

/// Parse an `InlayHint[]` response into the compact wire
/// shape led's painter consumes. Hints without a recognisable
/// `position` + `label` are dropped. `label` may be either a
/// bare string or an array of `InlayHintLabelPart` objects;
/// we concatenate the parts' `value` fields.
pub(super) fn parse_inlay_hints(
    items: &[Value],
) -> Vec<led_driver_lsp_core::InlayHint> {
    items
        .iter()
        .filter_map(|v| {
            let pos = v.get("position")?;
            let line = pos.get("line")?.as_u64()? as u32;
            let col = pos.get("character")?.as_u64()? as u32;
            let label_value = v.get("label")?;
            let label = match label_value {
                Value::String(s) => s.clone(),
                Value::Array(parts) => {
                    let mut acc = String::new();
                    for p in parts {
                        if let Some(s) = p.get("value").and_then(|vv| vv.as_str()) {
                            acc.push_str(s);
                        }
                    }
                    acc
                }
                _ => return None,
            };
            let padding_left = v
                .get("paddingLeft")
                .and_then(|p| p.as_bool())
                .unwrap_or(false);
            let padding_right = v
                .get("paddingRight")
                .and_then(|p| p.as_bool())
                .unwrap_or(false);
            Some(led_driver_lsp_core::InlayHint {
                line,
                col,
                label: Arc::<str>::from(label),
                padding_left,
                padding_right,
            })
        })
        .collect()
}

/// Parse a `WorkspaceEdit` response from `textDocument/rename`
/// or a resolved code action into a flat `Vec<FileEdit>`. LSP
/// has two shapes:
///
/// - `changes`: `{ uri: [TextEdit] }` — the legacy form.
/// - `documentChanges`: `[{ textDocument: {uri,version},
///   edits: [TextEdit] }, ...]` — the versioned form.
///
/// We flatten either shape into one `FileEdit` per distinct
/// uri. Unknown shapes (pure-null, pure-errors) return an
/// empty vec which the runtime treats as "no-op rename" —
/// still surfaces the alert and dismisses.
pub(super) fn parse_workspace_edit(
    result: &Value,
) -> Vec<led_driver_lsp_core::FileEdit> {
    let mut out: Vec<led_driver_lsp_core::FileEdit> = Vec::new();
    if let Some(changes) = result.get("changes").and_then(|c| c.as_object()) {
        for (uri, edits_json) in changes {
            let Some(path) =
                path_from_uri(uri).map(|p| led_core::UserPath::new(p).canonicalize())
            else {
                continue;
            };
            let Some(arr) = edits_json.as_array() else { continue };
            let edits = parse_text_edit_list(arr);
            if !edits.is_empty() {
                out.push(led_driver_lsp_core::FileEdit { path, edits });
            }
        }
    }
    if let Some(doc_changes) =
        result.get("documentChanges").and_then(|d| d.as_array())
    {
        for change in doc_changes {
            let Some(uri) = change
                .pointer("/textDocument/uri")
                .and_then(|v| v.as_str())
                .and_then(path_from_uri)
                .map(|p| led_core::UserPath::new(p).canonicalize())
            else {
                continue;
            };
            let Some(arr) = change.get("edits").and_then(|e| e.as_array()) else {
                continue;
            };
            let edits = parse_text_edit_list(arr);
            if !edits.is_empty() {
                out.push(led_driver_lsp_core::FileEdit { path: uri, edits });
            }
        }
    }
    out
}

pub(super) fn parse_text_edit_list(arr: &[Value]) -> Vec<led_driver_lsp_core::TextEditOp> {
    arr.iter().filter_map(parse_text_edit).collect()
}

pub(super) fn parse_text_edit(v: &Value) -> Option<led_driver_lsp_core::TextEditOp> {
    let range = v.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    let start_line = start.get("line")?.as_u64()? as u32;
    let start_col = start.get("character")?.as_u64()? as u32;
    let end_line = end.get("line")?.as_u64()? as u32;
    let end_col = end.get("character")?.as_u64()? as u32;
    let new_text = v.get("newText").and_then(|t| t.as_str()).unwrap_or("");
    Some(led_driver_lsp_core::TextEditOp {
        start_line,
        start_col,
        end_line,
        end_col,
        new_text: Arc::<str>::from(new_text),
    })
}

/// Parse a `textDocument/definition` response. The LSP shape
/// is one of: `null`, a single `Location`, an array of
/// `Location`, or an array of `LocationLink`. We only use the
/// first entry and flatten to [`led_driver_lsp_core::Location`].
///
/// Returns `None` when the server has no answer, or when every
/// entry is malformed.
pub(super) fn parse_definition_location(
    result: Value,
) -> Option<led_driver_lsp_core::Location> {
    let entry = match result {
        Value::Null => return None,
        Value::Array(arr) => arr.into_iter().next()?,
        v @ Value::Object(_) => v,
        _ => return None,
    };
    // `LocationLink` uses `targetUri` + `targetSelectionRange`;
    // `Location` uses `uri` + `range`. Try both.
    let uri = entry
        .get("uri")
        .or_else(|| entry.get("targetUri"))
        .and_then(|u| u.as_str())?;
    let range = entry
        .get("range")
        .or_else(|| entry.get("targetSelectionRange"))
        .or_else(|| entry.get("targetRange"))?;
    let start = range.get("start")?;
    let line = start.get("line").and_then(|v| v.as_u64())? as u32;
    let col = start.get("character").and_then(|v| v.as_u64())? as u32;
    let path = path_from_uri(uri)?;
    Some(led_driver_lsp_core::Location {
        path: led_core::UserPath::new(path).canonicalize(),
        line,
        col,
    })
}

/// Parse a `textDocument/diagnostic` pull-response body. LSP
/// documents two shapes: Full (report) and Unchanged. We only
/// care about Full here; Unchanged yields an empty list.
pub(super) fn parse_diagnostic_result(result: &Value) -> Vec<Diagnostic> {
    let kind = result.get("kind").and_then(|k| k.as_str()).unwrap_or("full");
    if kind != "full" {
        return Vec::new();
    }
    result
        .get("items")
        .and_then(|i| i.as_array())
        .map(|arr| arr.iter().filter_map(parse_diagnostic_entry).collect())
        .unwrap_or_default()
}

/// Parse one LSP `Diagnostic` object into our
/// [`led_state_diagnostics::Diagnostic`]. Positions are
/// forwarded verbatim (LSP uses UTF-16 by default — we'll convert
/// in stage 5 when we have the rope snapshot at accept time). For
/// now, interpret positions as char offsets; incorrect for
/// non-ASCII but not worse than legacy's first cut.
pub(super) fn parse_diagnostic_entry(entry: &Value) -> Option<Diagnostic> {
    let range = entry.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    let start_line = start.get("line")?.as_u64()? as usize;
    let start_col = start.get("character")?.as_u64()? as usize;
    let end_line = end.get("line")?.as_u64()? as usize;
    let end_col = end.get("character")?.as_u64()? as usize;
    let severity = match entry.get("severity").and_then(|s| s.as_u64()) {
        Some(1) => DiagnosticSeverity::Error,
        Some(2) => DiagnosticSeverity::Warning,
        Some(3) => DiagnosticSeverity::Info,
        Some(4) => DiagnosticSeverity::Hint,
        _ => DiagnosticSeverity::Error,
    };
    let message = entry
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let source = entry
        .get("source")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());
    let code = entry.get("code").and_then(|c| {
        c.as_str().map(|s| s.to_string()).or_else(|| {
            c.as_i64().map(|n| n.to_string())
        })
    });
    Some(Diagnostic {
        start_line,
        start_col,
        end_line,
        end_col,
        severity,
        message,
        source,
        code,
    })
}
