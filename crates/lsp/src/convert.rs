use std::collections::HashMap;
use std::path::{Path, PathBuf};

use led_core::lsp_types::{EditorRange, EditorTextEdit};
use lsp_types::{GotoDefinitionResponse, Location, TextEdit, WorkspaceEdit};

use crate::util::{from_lsp_pos, path_from_uri, read_file_lines};

pub(crate) fn lsp_edit_to_editor(
    te: &TextEdit,
    line_at: impl Fn(usize) -> Option<String>,
) -> EditorTextEdit {
    let start_row = te.range.start.line as usize;
    let end_row = te.range.end.line as usize;
    let start_line = line_at(start_row);
    let end_line = if end_row == start_row {
        start_line.clone()
    } else {
        line_at(end_row)
    };
    let start = from_lsp_pos(&te.range.start, start_line.as_deref());
    let end = from_lsp_pos(&te.range.end, end_line.as_deref());
    EditorTextEdit {
        range: EditorRange { start, end },
        new_text: te.new_text.clone(),
        start_line: None,
        end_line: None,
    }
}

pub(crate) fn workspace_edit_to_file_edits(
    edit: &WorkspaceEdit,
) -> Vec<(PathBuf, Vec<EditorTextEdit>)> {
    let mut result: HashMap<PathBuf, Vec<EditorTextEdit>> = HashMap::new();

    if let Some(changes) = &edit.changes {
        for (uri, edits) in changes {
            if let Some(path) = path_from_uri(uri) {
                let lines = read_file_lines(&path);
                let line_at = |row: usize| lines.get(row).cloned();
                let editor_edits: Vec<EditorTextEdit> = edits
                    .iter()
                    .map(|e| lsp_edit_to_editor(e, &line_at))
                    .collect();
                result.entry(path).or_default().extend(editor_edits);
            }
        }
    }

    if let Some(document_changes) = &edit.document_changes {
        use lsp_types::DocumentChanges;
        match document_changes {
            DocumentChanges::Edits(edits) => {
                for tde in edits {
                    if let Some(path) = path_from_uri(&tde.text_document.uri) {
                        let lines = read_file_lines(&path);
                        let line_at = |row: usize| lines.get(row).cloned();
                        let editor_edits: Vec<EditorTextEdit> = tde
                            .edits
                            .iter()
                            .filter_map(|e| match e {
                                lsp_types::OneOf::Left(te) => {
                                    Some(lsp_edit_to_editor(te, &line_at))
                                }
                                lsp_types::OneOf::Right(ate) => {
                                    Some(lsp_edit_to_editor(&ate.text_edit, &line_at))
                                }
                            })
                            .collect();
                        result.entry(path).or_default().extend(editor_edits);
                    }
                }
            }
            DocumentChanges::Operations(ops) => {
                for op in ops {
                    if let lsp_types::DocumentChangeOperation::Edit(tde) = op {
                        if let Some(path) = path_from_uri(&tde.text_document.uri) {
                            let lines = read_file_lines(&path);
                            let line_at = |row: usize| lines.get(row).cloned();
                            let editor_edits: Vec<EditorTextEdit> = tde
                                .edits
                                .iter()
                                .filter_map(|e| match e {
                                    lsp_types::OneOf::Left(te) => {
                                        Some(lsp_edit_to_editor(te, &line_at))
                                    }
                                    lsp_types::OneOf::Right(ate) => {
                                        Some(lsp_edit_to_editor(&ate.text_edit, &line_at))
                                    }
                                })
                                .collect();
                            result.entry(path).or_default().extend(editor_edits);
                        }
                    }
                }
            }
        }
    }

    result.into_iter().collect()
}

pub(crate) fn apply_edits_to_disk(path: &Path, edits: &[EditorTextEdit]) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    if lines.is_empty() {
        lines.push(String::new());
    }

    // Sort edits in reverse order so we apply from bottom to top
    let mut sorted_edits: Vec<&EditorTextEdit> = edits.iter().collect();
    sorted_edits.sort_by(|a, b| {
        let row_cmp = b.range.start.row.cmp(&a.range.start.row);
        if row_cmp == std::cmp::Ordering::Equal {
            b.range.start.col.cmp(&a.range.start.col)
        } else {
            row_cmp
        }
    });

    for edit in sorted_edits {
        let start_row = edit.range.start.row.min(lines.len());
        let start_col = edit.range.start.col;
        let end_row = edit.range.end.row.min(lines.len());
        let end_col = edit.range.end.col;

        // Build the new content
        let prefix = if start_row < lines.len() {
            lines[start_row].chars().take(start_col).collect::<String>()
        } else {
            String::new()
        };
        let suffix = if end_row < lines.len() {
            lines[end_row].chars().skip(end_col).collect::<String>()
        } else {
            String::new()
        };

        let new_text = format!("{}{}{}", prefix, edit.new_text, suffix);
        let new_lines: Vec<String> = new_text.lines().map(|l| l.to_string()).collect();

        // Replace the range
        let remove_end = (end_row + 1).min(lines.len());
        lines.splice(start_row..remove_end, new_lines);
    }

    let result = lines.join("\n");
    // Preserve trailing newline if original had one
    let output = if content.ends_with('\n') && !result.ends_with('\n') {
        format!("{}\n", result)
    } else {
        result
    };
    let _ = std::fs::write(path, output);
}

pub(crate) fn language_id_for_extension(ext: &str) -> &'static str {
    match ext {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "py" => "python",
        "c" | "h" => "c",
        "cpp" | "hpp" | "cc" | "cxx" => "cpp",
        "swift" => "swift",
        "toml" => "toml",
        "json" => "json",
        "sh" | "bash" => "shellscript",
        _ => "plaintext",
    }
}

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
                let lines = read_file_lines(&path);
                let line = lines.get(link.target_selection_range.start.line as usize);
                let pos =
                    from_lsp_pos(&link.target_selection_range.start, line.map(|s| s.as_str()));
                Some((path, pos.row, pos.col))
            })
            .collect(),
    }
}

fn location_to_tuple(loc: &Location) -> Option<(PathBuf, usize, usize)> {
    let path = path_from_uri(&loc.uri)?;
    let lines = read_file_lines(&path);
    let line = lines.get(loc.range.start.line as usize);
    let pos = from_lsp_pos(&loc.range.start, line.map(|s| s.as_str()));
    Some((path, pos.row, pos.col))
}
