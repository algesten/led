mod component;
mod convert;
mod manager;
mod registry;
mod server;
mod transport;
mod types;
mod util;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use led_core::{Effect, LspStatus, Waker};
use lsp_types::CodeActionOrCommand;

use crate::registry::LspRegistry;
use crate::server::LanguageServer;
use crate::types::{LspManagerEvent, ProgressState};

pub struct LspManager {
    registry: LspRegistry,
    servers: HashMap<String, Arc<LanguageServer>>,
    root: PathBuf,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<LspManagerEvent>,
    event_tx: tokio::sync::mpsc::UnboundedSender<LspManagerEvent>,
    waker: Option<Waker>,
    pending_starts: HashSet<String>,
    opened_docs: HashSet<PathBuf>,
    /// Paths that got TabActivated before the server was ready
    pending_opens: HashSet<PathBuf>,
    pending_code_actions: HashMap<PathBuf, Vec<CodeActionOrCommand>>,
    progress_tokens: HashMap<String, ProgressState>,
    quiescent: bool,
    need_diagnostics: bool,
    _file_watcher: Option<notify::RecommendedWatcher>,
    file_watcher_globs: Option<globset::GlobSet>,
}

impl LspManager {
    pub fn new(root: PathBuf, waker: Option<Waker>) -> Self {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            registry: LspRegistry::new(),
            servers: HashMap::new(),
            root,
            event_rx,
            event_tx,
            waker,
            pending_starts: HashSet::new(),
            opened_docs: HashSet::new(),
            pending_opens: HashSet::new(),
            pending_code_actions: HashMap::new(),
            progress_tokens: HashMap::new(),
            quiescent: true,
            need_diagnostics: false,
            _file_watcher: None,
            file_watcher_globs: None,
        }
    }

    fn is_busy(&self) -> bool {
        !self.quiescent || !self.progress_tokens.is_empty()
    }

    fn lsp_status_effect(&self) -> Effect {
        Effect::SetLspStatus(compute_lsp_status(
            self.servers.values().next().map(|s| &*s.name),
            &self.progress_tokens,
            self.is_busy(),
        ))
    }
}

fn progress_detail(progress_tokens: &HashMap<String, ProgressState>) -> Option<String> {
    if progress_tokens.is_empty() {
        return None;
    }
    progress_tokens.values().next().map(|p| {
        if let Some(ref msg) = p.message {
            format!("{} {}", p.title, msg)
        } else {
            p.title.clone()
        }
    })
}

fn compute_lsp_status(
    server_name: Option<&str>,
    progress_tokens: &HashMap<String, ProgressState>,
    busy: bool,
) -> LspStatus {
    LspStatus {
        server_name: server_name.unwrap_or_default().to_string(),
        busy,
        detail: progress_detail(progress_tokens),
    }
}
