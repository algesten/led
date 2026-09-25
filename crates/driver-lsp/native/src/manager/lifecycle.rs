use std::collections::HashMap;
use std::process::Command;

use led_core::ServerId;
use led_driver_lsp_core::{
    DiagnosticSource, LspEvent,
    diag_source::DiagMode,
};
use led_state_syntax::Language;
use serde_json::{Value, json};

use crate::protocol::{
    InitializeCapabilities, build_did_change_configuration_notification,
    build_initialize_request, build_initialized_notification, parse_initialize_response,
};

use super::{
    Manager, PendingRequest, ServerEntry, notifications::send_did_open, short_server_id,
};

impl Manager {
    pub(super) fn ensure_server_spawned(&mut self, language: Language) {
        if self.servers.contains_key(&language) {
            return;
        }
        if self.skipped_languages.contains(&language) {
            // We've already decided this language has no server
            // available. Don't retry on every BufferOpened.
            return;
        }
        let Some(config) = self.registry.config_for(language) else {
            // No registry entry for this language — also "no
            // server", permanently. Mark so we skip future
            // spawn calls cheaply.
            self.skipped_languages.insert(language);
            return;
        };
        // Server name = the binary command (e.g. "rust-analyzer",
        // "taplo"). Matches legacy server.rs:95 so the status-bar
        // text and trace output read the same shape regardless of
        // which editor ran the workspace.
        let name = config.command.to_string();
        let args: Vec<&str> = config.args.to_vec();
        if language == Language::Rust && config.command == "rust-analyzer" {
            let result = ensure_rust_analyzer_available(config.command, "rustup", || {
                let _ = self.lsp_event_tx.send(LspEvent::Progress {
                    server: ServerId::new(name.clone()),
                    busy: true,
                    detail: Some("Installing rust-analyzer".into()),
                });
                self.notify.notify();
            });
            match result {
                Ok(true) => {
                    let _ = self.lsp_event_tx.send(LspEvent::Progress {
                        server: ServerId::new(name.clone()),
                        busy: false,
                        detail: None,
                    });
                    self.notify.notify();
                }
                Ok(false) => {}
                Err(message) => {
                    let _ = self.lsp_event_tx.send(LspEvent::Error {
                        server: ServerId::new(name),
                        message,
                    });
                    self.notify.notify();
                    self.skipped_languages.insert(language);
                    return;
                }
            }
        }
        let server = match crate::subprocess::spawn(
            name.clone(),
            config.command,
            &args,
            self.incoming_tx.clone(),
        ) {
            Ok(s) => s,
            Err(e) => {
                // `NotFound` = binary not in `$PATH`. Legacy
                // (registry.rs + manager.rs) treats this as a
                // silent skip: the user just doesn't have this
                // LSP installed, which is the normal case for
                // most languages. No alert, no log.
                //
                // Anything else (permission denied, malformed
                // binary, etc.) IS surfaced as a warn alert so
                // the user can act on it.
                if e.kind() != std::io::ErrorKind::NotFound {
                    let _ = self.lsp_event_tx.send(LspEvent::Error {
                        server: ServerId::new(name),
                        message: format!("spawn failed: {e}"),
                    });
                    self.notify.notify();
                }
                self.skipped_languages.insert(language);
                return;
            }
        };
        let server_name = short_server_id(&server.name);
        self.trace.lsp_server_started(&server_name);

        let id = self.fresh_id();
        let root = self
            .workspace_root
            .clone()
            .unwrap_or_default();
        let body = build_initialize_request(id, &root);
        let _ = server.send_body(&body);
        self.trace
            .lsp_send_request(&server_name, "initialize", id, None);

        let mut entry = ServerEntry {
            language,
            server,
            diag: DiagnosticSource::new(),
            pending_requests: HashMap::new(),
            queued_opens: Vec::new(),
            initialized: false,
            doc_versions: HashMap::new(),
            buffer_hashes: HashMap::new(),
            deferred_init_request: false,
            last_rope_sent: HashMap::new(),
            // Completion caps default to "no support"; parsed
            // from the initialize response in `finish_initialize`.
            completion_provider: false,
            completion_trigger_chars: Vec::new(),
            completion_resolve_provider: false,
            code_action_cache: HashMap::new(),
        };
        entry.pending_requests.insert(id, PendingRequest::Initialize);
        self.servers.insert(language, entry);
    }

    pub(super) fn finish_initialize(
        &mut self,
        language: Language,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let entry = self.servers.get_mut(&language).unwrap();
        match payload {
            Ok(result) => {
                let caps: InitializeCapabilities = parse_initialize_response(&result);
                // Default mode is Pull (per audit Theme L Target C
                // — "Pull diagnostics only"). Downgrade to Push
                // only when the server does NOT advertise
                // `diagnosticProvider`; those servers (e.g. legacy
                // typescript-language-server builds) deliver
                // everything via `publishDiagnostics`.
                if !caps.diagnostic_provider {
                    entry.diag.set_mode(DiagMode::Push);
                }
                entry.completion_provider = caps.completion_provider;
                entry.completion_trigger_chars = caps.completion_trigger_chars.clone();
                entry.completion_resolve_provider = caps.completion_resolve_provider;
                // Quiescence is NOT latched from the initialize
                // response. Some servers advertise
                // `serverStatusNotification` capability but never
                // emit the notification; others emit it without
                // advertising. Legacy detects at runtime on the
                // first notification — see the handler for
                // `experimental/serverStatus` below. The `caps.has_quiescence`
                // bit is retained for logs only.
                let _ = caps.has_quiescence;
                let server_name = short_server_id(&entry.server.name);
                let _ = entry.server.send_body(&build_initialized_notification());
                self.trace.lsp_send_notification(
                    &server_name,
                    "initialized",
                    None,
                    None,
                );
                // rust-analyzer waits for this before starting its cold-index
                // phase. Empty settings is the right payload — we don't override
                // any defaults. See docs/rewrite/lsp-patterns.md §2.5.
                let _ = entry
                    .server
                    .send_body(&build_did_change_configuration_notification());
                self.trace.lsp_send_notification(
                    &server_name,
                    "workspace/didChangeConfiguration",
                    None,
                    None,
                );
                entry.initialized = true;
                let queued = std::mem::take(&mut entry.queued_opens);
                for open in queued {
                    send_did_open(entry, &open.path, &open.rope, self.trace.as_ref());
                    entry.buffer_hashes.insert(open.path.clone(), open.hash);
                }
            }
            Err(err) => {
                let server_name = ServerId::new(entry.server.name.clone());
                let _ = self.lsp_event_tx.send(LspEvent::Error {
                    server: server_name,
                    message: format!(
                        "initialize failed (code {}): {}",
                        err.code, err.message
                    ),
                });
                self.notify.notify();
            }
        }
    }

    pub(super) fn shutdown_all(&mut self) {
        // Simplified for now: send shutdown + exit, drop servers.
        // A proper implementation would await the shutdown reply
        // before sending exit; the drop semantics clean up
        // regardless.
        let languages: Vec<Language> = self.servers.keys().copied().collect();
        for lang in languages {
            let id = self.fresh_id();
            let entry = self.servers.get_mut(&lang).unwrap();
            let server_name = short_server_id(&entry.server.name);
            let shutdown_body = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "shutdown",
                "params": Value::Null,
            });
            let _ = entry
                .server
                .send_body(&serde_json::to_vec(&shutdown_body).unwrap());
            self.trace
                .lsp_send_request(&server_name, "shutdown", id, None);
            let entry = self.servers.get_mut(&lang).unwrap();
            entry.pending_requests.insert(id, PendingRequest::Shutdown);
            let exit_body = json!({
                "jsonrpc": "2.0",
                "method": "exit",
                "params": Value::Null,
            });
            let _ = entry
                .server
                .send_body(&serde_json::to_vec(&exit_body).unwrap());
            self.trace
                .lsp_send_notification(&server_name, "exit", None, None);
        }
        self.servers.clear();
    }
}

/// A rustup proxy exists on PATH even when its component is missing.
/// Probe that proxy before opening the LSP stream, install only for
/// rustup's specific missing-component diagnostic, then verify the
/// installed binary actually runs. Returns true when it installed.
fn ensure_rust_analyzer_available(
    analyzer: &str,
    rustup: &str,
    on_install: impl FnOnce(),
) -> Result<bool, String> {
    let probe = Command::new(analyzer)
        .arg("--version")
        .output()
        .map_err(|e| format!("cannot run rust-analyzer: {e}"))?;
    if probe.status.success() {
        return Ok(false);
    }
    let stderr = String::from_utf8_lossy(&probe.stderr);
    let missing_component = stderr.contains("Unknown binary 'rust-analyzer'")
        || stderr.contains("'rust-analyzer' is not installed for the toolchain");
    if !missing_component {
        return Err(format!(
            "rust-analyzer --version failed: {}",
            error_detail(&probe)
        ));
    }

    on_install();
    let install = Command::new(rustup)
        .args(["component", "add", "rust-analyzer"])
        .output()
        .map_err(|e| format!("cannot install rust-analyzer with rustup: {e}"))?;
    if !install.status.success() {
        return Err(format!(
            "rustup component add rust-analyzer failed: {}",
            error_detail(&install)
        ));
    }
    let verify = Command::new(analyzer)
        .arg("--version")
        .output()
        .map_err(|e| format!("cannot run rust-analyzer after installation: {e}"))?;
    if !verify.status.success() {
        return Err(format!(
            "rust-analyzer still fails after installation: {}",
            error_detail(&verify)
        ));
    }
    Ok(true)
}

fn error_detail(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        output.status.to_string()
    } else {
        detail.to_string()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(path: &std::path::Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn missing_rustup_component_is_installed_and_reprobed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("installed");
        let analyzer = dir.path().join("rust-analyzer");
        let rustup = dir.path().join("rustup");
        script(
            &analyzer,
            &format!(
                "if [ -f '{}' ]; then echo 'rust-analyzer test'; else echo \"error: Unknown binary 'rust-analyzer' in official toolchain 'test'\" >&2; exit 1; fi",
                marker.display()
            ),
        );
        script(
            &rustup,
            &format!(
                "[ \"$*\" = 'component add rust-analyzer' ] || exit 2\ntouch '{}'",
                marker.display()
            ),
        );

        let mut install_calls = 0;
        assert_eq!(
            ensure_rust_analyzer_available(
                analyzer.to_str().unwrap(),
                rustup.to_str().unwrap(),
                || install_calls += 1,
            ),
            Ok(true),
        );
        assert_eq!(install_calls, 1);
        assert!(marker.exists());
        assert_eq!(
            ensure_rust_analyzer_available(
                analyzer.to_str().unwrap(),
                rustup.to_str().unwrap(),
                || install_calls += 1,
            ),
            Ok(false),
        );
        assert_eq!(install_calls, 1);
    }

    #[test]
    fn unrelated_analyzer_failure_does_not_install() {
        let dir = tempfile::tempdir().unwrap();
        let analyzer = dir.path().join("rust-analyzer");
        let rustup = dir.path().join("rustup");
        let marker = dir.path().join("installed");
        script(&analyzer, "echo 'broken analyzer' >&2; exit 1");
        script(&rustup, &format!("touch '{}'", marker.display()));

        let result = ensure_rust_analyzer_available(
            analyzer.to_str().unwrap(),
            rustup.to_str().unwrap(),
            || panic!("should not install"),
        );
        assert!(result.unwrap_err().contains("broken analyzer"));
        assert!(!marker.exists());
    }

    #[test]
    fn failed_component_install_reports_rustup_error() {
        let dir = tempfile::tempdir().unwrap();
        let analyzer = dir.path().join("rust-analyzer");
        let rustup = dir.path().join("rustup");
        script(
            &analyzer,
            "echo \"error: Unknown binary 'rust-analyzer' in official toolchain 'test'\" >&2; exit 1",
        );
        script(&rustup, "echo 'download failed' >&2; exit 1");

        let mut attempted = false;
        let result = ensure_rust_analyzer_available(
            analyzer.to_str().unwrap(),
            rustup.to_str().unwrap(),
            || attempted = true,
        );
        assert!(attempted);
        assert!(result.unwrap_err().contains("download failed"));
    }
}
