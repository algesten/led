use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{CanonPath, Col, Doc, EditOp, PersistedContentHash, Row};

mod convert;
mod manager;
mod registry;
mod server;
mod transport;

// ── Domain types (public, no lsp-types leak) ──

#[derive(Debug, Clone)]
pub struct TextEdit {
    pub start_row: Row,
    pub start_col: Col,
    pub end_row: Row,
    pub end_col: Col,
    pub new_text: String,
}

#[derive(Debug, Clone)]
pub struct FileEdit {
    pub path: CanonPath,
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
    pub row: Row,
    pub col: Col,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub start_row: Row,
    pub start_col: Col,
    pub end_row: Row,
    pub end_col: Col,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub source: Option<String>,
    /// Diagnostic code from the server (e.g. "E0277"). Used to match
    /// the same diagnostic between push and pull results.
    pub code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

// ── LspOut (derived → driver) ──

#[derive(Debug, Clone)]
pub enum LspOut {
    // Lifecycle
    Init {
        root: CanonPath,
    },
    Shutdown,

    // Document sync
    BufferOpened {
        path: CanonPath,
        doc: Arc<dyn Doc>,
    },
    BufferChanged {
        path: CanonPath,
        doc: Arc<dyn Doc>,
        edit_ops: Vec<EditOp>,
        /// True when this change should also send didSave (local save or
        /// external file change). The content matches what's on disk.
        do_save: bool,
    },
    BufferClosed {
        path: CanonPath,
    },

    /// Request a diagnostic cycle. The manager freezes its input queue,
    /// snapshots content hashes for all open docs, and pulls diagnostics
    /// for every opened file.
    RequestDiagnostics,

    // User-initiated requests
    GotoDefinition {
        path: CanonPath,
        row: Row,
        col: Col,
    },
    Complete {
        path: CanonPath,
        row: Row,
        col: Col,
    },
    CompleteAccept {
        index: usize,
    },
    Rename {
        path: CanonPath,
        row: Row,
        col: Col,
        new_name: String,
    },
    CodeAction {
        path: CanonPath,
        start_row: Row,
        start_col: Col,
        end_row: Row,
        end_col: Col,
    },
    CodeActionSelect {
        index: usize,
    },
    Format {
        path: CanonPath,
    },
    InlayHints {
        path: CanonPath,
        start_row: Row,
        end_row: Row,
    },
}

// ── LspIn (driver → model) ──

#[derive(Clone)]
pub enum LspIn {
    // Navigation
    Navigate {
        path: CanonPath,
        row: Row,
        col: Col,
    },

    // Edits
    Edits {
        edits: Vec<FileEdit>,
    },

    // Completion popup
    Completion {
        items: Vec<CompletionItem>,
        prefix_start_col: Col,
    },

    // Code action picker
    CodeActions {
        actions: Vec<String>,
    },

    // Annotations
    Diagnostics {
        path: CanonPath,
        diagnostics: Vec<Diagnostic>,
        content_hash: PersistedContentHash,
    },
    InlayHints {
        path: CanonPath,
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
