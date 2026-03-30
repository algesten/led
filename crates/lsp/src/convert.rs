use std::path::{Path, PathBuf};

use led_core::Doc;
use lsp_types::{
    CodeActionOrCommand, CompletionResponse, GotoDefinitionResponse, Location, Position, TextEdit,
    Uri, WorkspaceEdit,
};

use crate::{CompletionItem, Diagnostic, DiagnosticSeverity, FileEdit, InlayHint};

// ── URI / path ──

pub(crate) fn uri_from_path(path: &Path) -> Option<Uri> {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = format!("file://{}", canonical.to_str()?);
    s.parse().ok()
}

pub(crate) fn path_from_uri(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let stripped = s.strip_prefix("file://")?;
    Some(PathBuf::from(stripped))
}

// ── UTF-16 ↔ char ──

pub(crate) fn utf16_col_to_char_col(line: &str, utf16_col: u32) -> usize {
    let mut utf16_offset = 0u32;
    for (i, ch) in line.chars().enumerate() {
        if utf16_offset >= utf16_col {
            return i;
        }
        utf16_offset += ch.len_utf16() as u32;
    }
    line.chars().count()
}

pub(crate) fn char_col_to_utf16_col(line: &str, char_col: usize) -> u32 {
    let mut utf16_offset = 0u32;
    for (i, ch) in line.chars().enumerate() {
        if i >= char_col {
            break;
        }
        utf16_offset += ch.len_utf16() as u32;
    }
    utf16_offset
}

/// Convert (row, col) to LSP Position, using a line for UTF-16 conversion.
pub(crate) fn lsp_pos(row: usize, col: usize, line: Option<&str>) -> Position {
    let utf16_col = match line {
        Some(l) => char_col_to_utf16_col(l, col),
        None => col as u32,
    };
    Position::new(row as u32, utf16_col)
}

/// Convert LSP Position to (row, col), using a line for UTF-16 conversion.
pub(crate) fn from_lsp_pos(pos: &Position, line: Option<&str>) -> (usize, usize) {
    let row = pos.line as usize;
    let col = match line {
        Some(l) => utf16_col_to_char_col(l, pos.character),
        None => pos.character as usize,
    };
    (row, col)
}

/// Get a line from a Doc, stripping trailing newline.
pub(crate) fn doc_line(doc: &dyn Doc, row: usize) -> Option<String> {
    if row >= doc.line_count() {
        return None;
    }
    let line = doc.line(led_core::Row(row));
    Some(
        line.trim_end_matches('\n')
            .trim_end_matches('\r')
            .to_string(),
    )
}

/// Get a line from a file on disk.
fn disk_line(path: &Path, row: usize) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    content.lines().nth(row).map(|l| l.to_string())
}

/// Get full text from a Doc.
pub(crate) fn doc_full_text(doc: &dyn Doc) -> String {
    let mut buf = Vec::new();
    doc.write_to(&mut buf).ok();
    String::from_utf8(buf).unwrap_or_default()
}

// ── TextEdit conversion ──

pub(crate) fn lsp_text_edit_to_domain(
    te: &TextEdit,
    line_at: &impl Fn(usize) -> Option<String>,
) -> crate::TextEdit {
    let start_row = te.range.start.line as usize;
    let end_row = te.range.end.line as usize;
    let start_line = line_at(start_row);
    let end_line = if end_row == start_row {
        start_line.clone()
    } else {
        line_at(end_row)
    };
    let (_, start_col) = from_lsp_pos(&te.range.start, start_line.as_deref());
    let (_, end_col) = from_lsp_pos(&te.range.end, end_line.as_deref());
    crate::TextEdit {
        start_row,
        start_col,
        end_row,
        end_col,
        new_text: te.new_text.clone(),
    }
}

// ── WorkspaceEdit → FileEdit ──

pub(crate) fn workspace_edit_to_file_edits(edit: &WorkspaceEdit) -> Vec<FileEdit> {
    use std::collections::HashMap;
    let mut result: HashMap<PathBuf, Vec<crate::TextEdit>> = HashMap::new();

    let collect_edits =
        |path: &Path,
         lsp_edits: &[TextEdit],
         result: &mut HashMap<PathBuf, Vec<crate::TextEdit>>| {
            let lines = read_file_lines(path);
            let line_at = |row: usize| lines.get(row).cloned();
            let edits: Vec<crate::TextEdit> = lsp_edits
                .iter()
                .map(|e| lsp_text_edit_to_domain(e, &line_at))
                .collect();
            result.entry(path.to_path_buf()).or_default().extend(edits);
        };

    if let Some(changes) = &edit.changes {
        for (uri, edits) in changes {
            if let Some(path) = path_from_uri(uri) {
                collect_edits(&path, edits, &mut result);
            }
        }
    }

    if let Some(document_changes) = &edit.document_changes {
        use lsp_types::DocumentChanges;
        match document_changes {
            DocumentChanges::Edits(edits) => {
                for tde in edits {
                    if let Some(path) = path_from_uri(&tde.text_document.uri) {
                        let raw: Vec<TextEdit> = tde
                            .edits
                            .iter()
                            .map(|e| match e {
                                lsp_types::OneOf::Left(te) => te.clone(),
                                lsp_types::OneOf::Right(ate) => ate.text_edit.clone(),
                            })
                            .collect();
                        collect_edits(&path, &raw, &mut result);
                    }
                }
            }
            DocumentChanges::Operations(ops) => {
                for op in ops {
                    if let lsp_types::DocumentChangeOperation::Edit(tde) = op {
                        if let Some(path) = path_from_uri(&tde.text_document.uri) {
                            let raw: Vec<TextEdit> = tde
                                .edits
                                .iter()
                                .map(|e| match e {
                                    lsp_types::OneOf::Left(te) => te.clone(),
                                    lsp_types::OneOf::Right(ate) => ate.text_edit.clone(),
                                })
                                .collect();
                            collect_edits(&path, &raw, &mut result);
                        }
                    }
                }
            }
        }
    }

    result
        .into_iter()
        .map(|(path, edits)| FileEdit { path, edits })
        .collect()
}

// ── Apply edits to disk ──

pub(crate) fn apply_edits_to_disk(path: &Path, edits: &[crate::TextEdit]) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    if lines.is_empty() {
        lines.push(String::new());
    }

    let mut sorted_edits: Vec<&crate::TextEdit> = edits.iter().collect();
    sorted_edits.sort_by(|a, b| {
        let row_cmp = b.start_row.cmp(&a.start_row);
        if row_cmp == std::cmp::Ordering::Equal {
            b.start_col.cmp(&a.start_col)
        } else {
            row_cmp
        }
    });

    for edit in sorted_edits {
        let start_row = edit.start_row.min(lines.len());
        let end_row = edit.end_row.min(lines.len());

        let prefix = if start_row < lines.len() {
            lines[start_row]
                .chars()
                .take(edit.start_col)
                .collect::<String>()
        } else {
            String::new()
        };
        let suffix = if end_row < lines.len() {
            lines[end_row]
                .chars()
                .skip(edit.end_col)
                .collect::<String>()
        } else {
            String::new()
        };

        let new_text = format!("{}{}{}", prefix, edit.new_text, suffix);
        let new_lines: Vec<String> = new_text.lines().map(|l| l.to_string()).collect();

        let remove_end = (end_row + 1).min(lines.len());
        lines.splice(start_row..remove_end, new_lines);
    }

    let result = lines.join("\n");
    let output = if content.ends_with('\n') && !result.ends_with('\n') {
        format!("{}\n", result)
    } else {
        result
    };
    let _ = std::fs::write(path, output);
}

// ── Goto definition response ──

pub(crate) fn definition_response_to_locations(
    resp: GotoDefinitionResponse,
) -> Vec<(PathBuf, usize, usize)> {
    match resp {
        GotoDefinitionResponse::Scalar(loc) => location_to_tuple(&loc).into_iter().collect(),
        GotoDefinitionResponse::Array(locs) => locs.iter().filter_map(location_to_tuple).collect(),
        GotoDefinitionResponse::Link(links) => links
            .iter()
            .filter_map(|link| {
                let path = path_from_uri(&link.target_uri)?;
                let line = disk_line(&path, link.target_selection_range.start.line as usize);
                let (row, col) = from_lsp_pos(&link.target_selection_range.start, line.as_deref());
                Some((path, row, col))
            })
            .collect(),
    }
}

fn location_to_tuple(loc: &Location) -> Option<(PathBuf, usize, usize)> {
    let path = path_from_uri(&loc.uri)?;
    let line = disk_line(&path, loc.range.start.line as usize);
    let (row, col) = from_lsp_pos(&loc.range.start, line.as_deref());
    Some((path, row, col))
}

// ── Diagnostics ──

pub(crate) fn convert_diagnostics(
    lsp_diags: &[lsp_types::Diagnostic],
    line_at: &impl Fn(usize) -> Option<String>,
) -> Vec<Diagnostic> {
    lsp_diags
        .iter()
        .map(|d| {
            let start_line = line_at(d.range.start.line as usize);
            let end_line = if d.range.end.line == d.range.start.line {
                start_line.clone()
            } else {
                line_at(d.range.end.line as usize)
            };
            let (start_row, start_col) = from_lsp_pos(&d.range.start, start_line.as_deref());
            let (end_row, end_col) = from_lsp_pos(&d.range.end, end_line.as_deref());
            let severity = match d.severity {
                Some(lsp_types::DiagnosticSeverity::ERROR) => DiagnosticSeverity::Error,
                Some(lsp_types::DiagnosticSeverity::WARNING) => DiagnosticSeverity::Warning,
                Some(lsp_types::DiagnosticSeverity::INFORMATION) => DiagnosticSeverity::Info,
                Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
                _ => DiagnosticSeverity::Error,
            };
            Diagnostic {
                start_row,
                start_col,
                end_row,
                end_col,
                severity,
                message: d.message.clone(),
                source: d.source.clone(),
            }
        })
        .collect()
}

// ── Inlay hints ──

pub(crate) fn convert_inlay_hints(
    hints: Vec<lsp_types::InlayHint>,
    line_at: &impl Fn(usize) -> Option<String>,
) -> Vec<InlayHint> {
    hints
        .into_iter()
        .map(|h| {
            let line = line_at(h.position.line as usize);
            let (row, col) = from_lsp_pos(&h.position, line.as_deref());
            let label = match h.label {
                lsp_types::InlayHintLabel::String(s) => s,
                lsp_types::InlayHintLabel::LabelParts(parts) => {
                    parts.into_iter().map(|p| p.value).collect::<String>()
                }
            };
            InlayHint { row, col, label }
        })
        .collect()
}

// ── Completion ──

pub(crate) fn convert_completion_response(
    resp: CompletionResponse,
    row: usize,
    col: usize,
    line_at: &impl Fn(usize) -> Option<String>,
) -> (Vec<CompletionItem>, usize) {
    let lsp_items = match resp {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    };

    let prefix_start_col = lsp_items
        .iter()
        .find_map(|item| {
            if let Some(lsp_types::CompletionTextEdit::Edit(ref te)) = item.text_edit {
                let line = line_at(te.range.start.line as usize);
                let (_, col) = from_lsp_pos(&te.range.start, line.as_deref());
                Some(col)
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            if let Some(line_str) = line_at(row) {
                let line: Vec<char> = line_str.chars().collect();
                let mut start = col;
                while start > 0 && start <= line.len() {
                    let ch = line[start - 1];
                    if ch.is_alphanumeric() || ch == '_' {
                        start -= 1;
                    } else {
                        break;
                    }
                }
                start
            } else {
                col
            }
        });

    let mut items: Vec<CompletionItem> = lsp_items
        .into_iter()
        .map(|item| {
            let label = item.label.clone();
            let detail = item.detail.clone();
            let kind = item.kind.map(|k| format!("{:?}", k));

            let (insert_text, text_edit) = match item.text_edit {
                Some(lsp_types::CompletionTextEdit::Edit(ref te)) => (
                    te.new_text.clone(),
                    Some(lsp_text_edit_to_domain(te, line_at)),
                ),
                Some(lsp_types::CompletionTextEdit::InsertAndReplace(ref te)) => {
                    let start_line = line_at(te.insert.start.line as usize);
                    let end_line = line_at(te.insert.end.line as usize);
                    let (sr, sc) = from_lsp_pos(&te.insert.start, start_line.as_deref());
                    let (er, ec) = from_lsp_pos(&te.insert.end, end_line.as_deref());
                    (
                        te.new_text.clone(),
                        Some(crate::TextEdit {
                            start_row: sr,
                            start_col: sc,
                            end_row: er,
                            end_col: ec,
                            new_text: te.new_text.clone(),
                        }),
                    )
                }
                None => {
                    let text = item
                        .insert_text
                        .as_deref()
                        .unwrap_or(&item.label)
                        .to_string();
                    (text, None)
                }
            };

            let additional_edits = item
                .additional_text_edits
                .as_ref()
                .map(|edits| {
                    edits
                        .iter()
                        .map(|e| lsp_text_edit_to_domain(e, line_at))
                        .collect()
                })
                .unwrap_or_default();

            let filter_text = item.filter_text.clone();
            let sort_text = item.sort_text.clone();

            CompletionItem {
                label,
                detail,
                kind,
                insert_text,
                filter_text,
                sort_text,
                text_edit,
                additional_edits,
            }
        })
        .collect();

    // Sort by sort_text (falling back to label) — matches VSCode behavior
    items.sort_by(|a, b| {
        let a_key = a.sort_text.as_deref().unwrap_or(&a.label);
        let b_key = b.sort_text.as_deref().unwrap_or(&b.label);
        a_key.cmp(b_key)
    });

    (items, prefix_start_col)
}

// ── Code actions ──

pub(crate) fn code_action_titles(actions: &[CodeActionOrCommand]) -> Vec<String> {
    actions
        .iter()
        .map(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.clone(),
            CodeActionOrCommand::Command(cmd) => cmd.title.clone(),
        })
        .collect()
}

// ── Helpers ──

fn read_file_lines(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => content.lines().map(|l| l.to_string()).collect(),
        Err(_) => vec![],
    }
}
