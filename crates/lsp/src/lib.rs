use std::path::PathBuf;
use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{Doc, EditOp};

mod convert;
mod manager;
mod registry;
mod server;
mod transport;

// ── Domain types (public, no lsp-types leak) ──

#[derive(Debug, Clone)]
pub struct TextEdit {
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
    pub new_text: String,
}

#[derive(Debug, Clone)]
pub struct FileEdit {
    pub path: PathBuf,
    pub edits: Vec<TextEdit>,
}

#[derive(Debug, Clone)]
pub struct CompletionItem {
    pub label: String,
    pub detail: Option<String>,
    pub kind: Option<String>,
    pub insert_text: String,
    pub filter_text: Option<String>,
    pub sort_text: Option<String>,
    pub text_edit: Option<TextEdit>,
    pub additional_edits: Vec<TextEdit>,
}

#[derive(Debug, Clone)]
pub struct InlayHint {
    pub row: usize,
    pub col: usize,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

// ── LspOut (derived → driver) ──

#[derive(Clone)]
pub enum LspOut {
    // Lifecycle
    Init {
        root: PathBuf,
    },
    Shutdown,

    // Document sync
    BufferOpened {
        path: PathBuf,
        doc: Arc<dyn Doc>,
    },
    BufferChanged {
        path: PathBuf,
        doc: Arc<dyn Doc>,
        edit_ops: Vec<EditOp>,
        /// True when the change originated from disk (e.g. external `git checkout`).
        /// The file is already saved, so the LSP should also receive didSave.
        external: bool,
    },
    BufferSaved {
        path: PathBuf,
        content_hash: u64,
    },
    BufferClosed {
        path: PathBuf,
    },

    // User-initiated requests
    GotoDefinition {
        path: PathBuf,
        row: usize,
        col: usize,
    },
    Complete {
        path: PathBuf,
        row: usize,
        col: usize,
    },
    CompleteAccept {
        index: usize,
    },
    Rename {
        path: PathBuf,
        row: usize,
        col: usize,
        new_name: String,
    },
    CodeAction {
        path: PathBuf,
        start_row: usize,
        start_col: usize,
        end_row: usize,
        end_col: usize,
    },
    CodeActionSelect {
        index: usize,
    },
    Format {
        path: PathBuf,
    },
    InlayHints {
        path: PathBuf,
        start_row: usize,
        end_row: usize,
    },
}

// ── LspIn (driver → model) ──

#[derive(Clone)]
pub enum LspIn {
    // Navigation
    Navigate {
        path: PathBuf,
        row: usize,
        col: usize,
    },

    // Edits
    Edits {
        edits: Vec<FileEdit>,
    },

    // Completion popup
    Completion {
        items: Vec<CompletionItem>,
        prefix_start_col: usize,
    },

    // Code action picker
    CodeActions {
        actions: Vec<String>,
    },

    // Annotations
    Diagnostics {
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
        content_hash: u64,
    },
    InlayHints {
        path: PathBuf,
        hints: Vec<InlayHint>,
    },

    // Trigger characters reported by server capabilities
    TriggerChars {
        extensions: Vec<String>,
        triggers: Vec<String>,
    },

    // Status — two indicators for the status bar
    Progress {
        server_name: String,
        busy: bool,
        detail: Option<String>,
    },
    Error {
        message: String,
    },
}

// ── Driver ──

pub fn driver(out: Stream<LspOut>, server_override: Option<String>) -> Stream<LspIn> {
    let stream: Stream<LspIn> = Stream::new();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<LspOut>(64);
    let (result_tx, mut result_rx) = tokio::sync::mpsc::channel::<LspIn>(64);

    // Bridge: rx::Stream → mpsc channel
    out.on(move |opt: Option<&LspOut>| {
        if let Some(cmd) = opt {
            cmd_tx.try_send(cmd.clone()).ok();
        }
    });

    // Async manager task
    tokio::spawn(async move {
        manager::run(cmd_rx, result_tx, server_override).await;
    });

    // Bridge: mpsc channel → rx::Stream
    let s = stream.clone();
    tokio::task::spawn_local(async move {
        while let Some(v) = result_rx.recv().await {
            s.push(v);
            tokio::task::yield_now().await;
        }
    });

    stream
}
