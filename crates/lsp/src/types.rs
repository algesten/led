use std::path::PathBuf;
use std::sync::Arc;

use led_core::lsp_types::EditorTextEdit;
use lsp_types::CodeActionOrCommand;

use crate::server::LanguageServer;
use crate::transport::LspNotification;

#[derive(Clone, Copy)]
pub(crate) enum FileChangeKind {
    Created,
    Changed,
    Deleted,
}

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
    FileChanged(PathBuf, FileChangeKind),
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
        edits: Vec<lsp_types::TextEdit>,
    },
    Rename {
        primary_path: PathBuf,
        file_edits: Vec<(PathBuf, Vec<EditorTextEdit>)>,
    },
    CodeActions {
        path: PathBuf,
        raw: Vec<CodeActionOrCommand>,
    },
    CodeActionResolved {
        primary_path: PathBuf,
        file_edits: Vec<(PathBuf, Vec<EditorTextEdit>)>,
    },
    InlayHints {
        path: PathBuf,
        hints: Vec<lsp_types::InlayHint>,
    },
    Diagnostics {
        path: PathBuf,
        raw: Vec<lsp_types::Diagnostic>,
    },
    Completion {
        path: PathBuf,
        response: lsp_types::CompletionResponse,
        row: usize,
        col: usize,
    },
    CompletionResolved {
        path: PathBuf,
        additional_edits: Vec<EditorTextEdit>,
    },
    Error {
        message: String,
    },
    FormatDone {
        path: PathBuf,
        generation: u64,
    },
}

pub(crate) struct ProgressState {
    pub(crate) title: String,
    pub(crate) message: Option<String>,
    #[allow(dead_code)]
    pub(crate) percentage: Option<u32>,
}
