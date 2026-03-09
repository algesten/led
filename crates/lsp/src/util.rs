use std::path::{Path, PathBuf};

use led_core::lsp_types::EditorPosition;
use lsp_types::{Position, Uri};

pub(crate) fn uri_from_path(path: &Path) -> Option<Uri> {
    let s = format!("file://{}", path.to_str()?);
    s.parse().ok()
}

pub(crate) fn path_from_uri(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let stripped = s.strip_prefix("file://")?;
    Some(PathBuf::from(stripped))
}

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

pub(crate) fn lsp_pos(row: usize, col: usize, lines: &[String]) -> Position {
    let utf16_col = if row < lines.len() {
        char_col_to_utf16_col(&lines[row], col)
    } else {
        col as u32
    };
    Position::new(row as u32, utf16_col)
}

pub(crate) fn from_lsp_pos(pos: &Position, lines: &[String]) -> EditorPosition {
    let row = pos.line as usize;
    let col = if row < lines.len() {
        utf16_col_to_char_col(&lines[row], pos.character)
    } else {
        pos.character as usize
    };
    EditorPosition { row, col }
}

pub(crate) fn read_file_lines(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => content.lines().map(|l| l.to_string()).collect(),
        Err(_) => vec![],
    }
}
