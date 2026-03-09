use std::path::PathBuf;
use std::sync::Arc;

use led_core::lsp_types::{
    EditorCodeAction, EditorCompletionItem, EditorDiagnostic, EditorInlayHint, EditorTextEdit,
};
use lsp_types::CodeActionOrCommand;

use crate::server::LanguageServer;
use crate::transport::LspNotification;

pub(crate) enum LspManagerEvent {
    ServerStarted {
        language_id: String,
        server: Arc<LanguageServer>,
    },
    ServerError {
        error: String,
    },
    Notification(LspNotification),
    RequestResult(RequestResult),
    FileChanged(PathBuf),
}

pub(crate) enum RequestResult {
    GotoDefinition {
        locations: Vec<(PathBuf, usize, usize)>,
        origin_path: PathBuf,
        origin_row: usize,
        origin_col: usize,
    },
    Format {
        path: PathBuf,
        edits: Vec<EditorTextEdit>,
    },
    Rename {
        primary_path: PathBuf,
        file_edits: Vec<(PathBuf, Vec<EditorTextEdit>)>,
    },
    CodeActions {
        path: PathBuf,
        actions: Vec<EditorCodeAction>,
        raw: Vec<CodeActionOrCommand>,
    },
    CodeActionResolved {
        primary_path: PathBuf,
        file_edits: Vec<(PathBuf, Vec<EditorTextEdit>)>,
    },
    InlayHints {
        path: PathBuf,
        hints: Vec<EditorInlayHint>,
    },
    Diagnostics {
        path: PathBuf,
        diagnostics: Vec<EditorDiagnostic>,
    },
    Completion {
        path: PathBuf,
        items: Vec<EditorCompletionItem>,
        prefix_start_col: usize,
    },
    Error {
        message: String,
    },
    FormatDone {
        path: PathBuf,
    },
}

pub(crate) struct ProgressState {
    pub(crate) title: String,
    pub(crate) message: Option<String>,
    #[allow(dead_code)]
    pub(crate) percentage: Option<u32>,
}
