use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct EditorPosition {
    pub row: usize,
    pub col: usize,
}

#[derive(Debug, Clone)]
pub struct EditorRange {
    pub start: EditorPosition,
    pub end: EditorPosition,
}

#[derive(Debug, Clone)]
pub struct EditorTextEdit {
    pub range: EditorRange,
    pub new_text: String,
    /// Pre-edit line content for UTF-16 position conversion in didChange.
    /// Set by TextDoc when recording changes; None for LSP-originated edits.
    pub start_line: Option<String>,
    pub end_line: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EditorFileEdit {
    pub path: PathBuf,
    pub edits: Vec<EditorTextEdit>,
}

#[derive(Debug, Clone)]
pub struct EditorDiagnostic {
    pub range: EditorRange,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct EditorInlayHint {
    pub position: EditorPosition,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct EditorCompletionItem {
    pub label: String,
    pub detail: Option<String>,
    pub insert_text: String,
    pub text_edit: Option<EditorTextEdit>,
    pub additional_edits: Vec<EditorTextEdit>,
    pub sort_text: Option<String>,
    pub filter_text: Option<String>,
    /// Raw LSP completion item, kept for resolve requests.
    pub lsp_completion: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}
