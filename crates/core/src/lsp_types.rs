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
pub struct EditorCodeAction {
    pub title: String,
    pub index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}
