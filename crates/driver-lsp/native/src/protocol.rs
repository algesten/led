//! LSP wire-protocol helpers.
//!
//! Pure JSON construction + parsing. No subprocess, no threads.
//! Every function takes plain data and returns either the frame
//! body (bytes suitable for [`crate::framing::encode_frame`]) or
//! a typed parse of a server response.
//!
//! The rewrite doesn't pull in the `lsp-types` crate — we only
//! need a narrow subset of the protocol (initialize, did*,
//! textDocument/diagnostic) and hand-rolling the JSON keeps the
//! dep graph lean. Field names match the spec exactly; deviations
//! are documented at each call site.

use std::sync::Arc;

use led_core::CanonPath;
use led_driver_lsp_core::{
    CompletionItem, CompletionTextEdit, FileEvent, FileEventKind, RegistrationGlob,
};
use serde_json::{Value, json};

// ── URI encoding ────────────────────────────────────────────────

/// Build an RFC-3986 `file://` URI from a canonical path. Encodes
/// only the bytes LSP servers choke on — spaces, `#`, `?`, and
/// non-ASCII — leaving `/` and alphanumerics verbatim. Matches
/// what `url::Url::from_file_path` produces; rolled by hand to
/// avoid the `url` dep for one call site.
pub fn uri_from_path(path: &CanonPath) -> String {
    let s = path.as_path().to_string_lossy();
    let mut out = String::with_capacity(s.len() + 7);
    out.push_str("file://");
    // On Windows `to_string_lossy` can produce `C:\...`; LSP
    // wants `/C:/...`. We're macOS / Linux only per the rewrite
    // scope, so paths already start with `/`.
    for ch in s.chars() {
        match ch {
            // Reserved chars that survive in URI paths unescaped.
            'A'..='Z' | 'a'..='z' | '0'..='9' | '/' | '-' | '_' | '.' | '~' => out.push(ch),
            _ => {
                for byte in ch.to_string().as_bytes() {
                    out.push('%');
                    out.push_str(&format!("{:02X}", byte));
                }
            }
        }
    }
    out
}

/// Inverse: parse a `file://` URI back to a path. Returns `None`
/// on non-`file` schemes or malformed percent-encoding.
pub fn path_from_uri(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let mut out = Vec::with_capacity(rest.len());
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            let b = u8::from_str_radix(hex, 16).ok()?;
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let s = String::from_utf8(out).ok()?;
    Some(std::path::PathBuf::from(s))
}

// ── Language ID mapping ────────────────────────────────────────

/// LSP's `languageId` strings. Kept separate from our internal
/// `Language` enum because this is a wire concept — what the
/// server expects on `textDocument/didOpen`. Legacy had a table
/// in convert.rs; we inline it here.
pub fn language_id(lang: led_state_syntax::Language) -> &'static str {
    use led_state_syntax::Language::*;
    match lang {
        Rust => "rust",
        TypeScript => "typescript",
        JavaScript => "javascript",
        Python => "python",
        Bash => "shellscript",
        Markdown => "markdown",
        Json => "json",
        Toml => "toml",
        C => "c",
        Cpp => "cpp",
        Ruby => "ruby",
        Swift => "swift",
        Make => "makefile",
    }
}

// ── Handshake ──────────────────────────────────────────────────

/// Build the `initialize` request body.
///
/// `id` is the request id we'll correlate the response against;
/// `root` is the workspace root. The capabilities we declare are
/// the narrow subset legacy led declared + a few we need for
/// M16: `textDocument.diagnostic` so pull-capable servers enable
/// it, `textDocument.publishDiagnostics` so push-capable ones do
/// too, and `workspace.configuration` so servers that ask for
/// config find us willing to answer.
pub fn build_initialize_request(id: i64, root: &CanonPath) -> Vec<u8> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "processId": Value::Null,
            "rootUri": uri_from_path(root),
            "capabilities": {
                "workspace": {
                    "configuration": true,
                    "didChangeConfiguration": { "dynamicRegistration": true },
                    "didChangeWatchedFiles": { "dynamicRegistration": true },
                },
                "textDocument": {
                    "synchronization": {
                        "didSave": true,
                        "willSave": false,
                        "willSaveWaitUntil": false,
                    },
                    "publishDiagnostics": {
                        "relatedInformation": false,
                        "versionSupport": false,
                        "codeDescriptionSupport": false,
                    },
                    "diagnostic": {
                        "dynamicRegistration": false,
                        "relatedDocumentSupport": false,
                    },
                },
                "window": {
                    // Opt into `$/progress` — rust-analyzer (and
                    // most LSP servers) gate progress emission on
                    // this capability. Without it the server
                    // never sends `$/progress` notifications, so
                    // the status bar has no detail to display
                    // during indexing / building phases.
                    "workDoneProgress": true,
                },
                "experimental": {
                    // rust-analyzer's non-spec quiescence extension.
                    // Other servers ignore unknown experimental keys.
                    "serverStatusNotification": true,
                },
            },
        },
    });
    serde_json::to_vec(&body).expect("serialize initialize")
}

/// Build the `initialized` notification body — sent right after
/// the server's initialize response comes back.
pub fn build_initialized_notification() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {},
    }))
    .expect("serialize initialized")
}

/// Build a `workspace/didChangeConfiguration` notification with an
/// empty settings object. rust-analyzer blocks its cold-index
/// phase waiting for client configuration; sending an empty
/// payload immediately after `initialized` releases it. Other
/// servers tolerate the empty object.
pub fn build_did_change_configuration_notification() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "method": "workspace/didChangeConfiguration",
        "params": { "settings": {} },
    }))
    .expect("serialize didChangeConfiguration")
}

/// Build a `workspace/didChangeWatchedFiles` notification body.
/// `changes` is the per-event payload the runtime memo already
/// matched against the server's registered globs. LSP encodes
/// `kind` as `1=Created | 2=Changed | 3=Deleted` (see spec
/// `FileChangeType`); we serialise the enum's discriminant
/// verbatim. `path` is percent-encoded into a `file://` URI on
/// the wire — the runtime hands us a `CanonPath` so the URI
/// rendering stays here, alongside every other LSP wire helper.
pub fn build_did_change_watched_files_notification(changes: &[FileEvent]) -> Vec<u8> {
    let arr: Vec<Value> = changes
        .iter()
        .map(|c| {
            let kind = match c.kind {
                FileEventKind::Created => 1,
                FileEventKind::Changed => 2,
                FileEventKind::Deleted => 3,
            };
            json!({ "uri": uri_from_path(&c.path), "type": kind })
        })
        .collect();
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "method": "workspace/didChangeWatchedFiles",
        "params": { "changes": arr },
    }))
    .expect("serialize didChangeWatchedFiles")
}

// ── client/registerCapability parsing ────────────────────────────

/// One parsed `Registration` entry from a
/// `client/registerCapability` payload, narrowed to the
/// `workspace/didChangeWatchedFiles` cases the runtime cares
/// about. Other registration methods (completion trigger char
/// updates, `textDocument/formatting`, …) are ignored — the
/// runtime has nothing to do with them today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedFilesRegistration {
    pub registration_id: String,
    pub globs: Vec<RegistrationGlob>,
}

/// Parse a `client/registerCapability` params object into the
/// subset of registrations the manager needs. Tolerant of
/// missing optional fields per LSP spec; malformed entries
/// drop silently. Multiple registrations in one payload are
/// rare but spec-legal — the manager emits one
/// `LspEvent::WatchedFilesRegistered` per id.
pub fn parse_register_capability_watched_files(
    params: &Value,
) -> Vec<WatchedFilesRegistration> {
    let Some(arr) = params.get("registrations").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in arr {
        let method = entry.get("method").and_then(|v| v.as_str()).unwrap_or("");
        if method != "workspace/didChangeWatchedFiles" {
            continue;
        }
        let id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let watchers = entry
            .pointer("/registerOptions/watchers")
            .and_then(|v| v.as_array());
        let Some(watchers) = watchers else { continue };
        let mut globs = Vec::with_capacity(watchers.len());
        for w in watchers {
            let pattern = match parse_watcher_glob_pattern(w) {
                Some(p) => p,
                None => continue,
            };
            // LSP `WatchKind` defaults to 7 (all three) when
            // omitted. The bit positions are spec-stable
            // (`Create=1 | Change=2 | Delete=4`) and match
            // `driver-file-watch`'s `ChangeKinds` so the
            // runtime memo can `&` them directly.
            let kinds = w
                .get("kind")
                .and_then(|v| v.as_u64())
                .map(|n| n as u8 & 0b111)
                .unwrap_or(0b111);
            let Ok(glob) = globset::Glob::new(&pattern) else {
                continue;
            };
            globs.push(RegistrationGlob {
                pattern,
                matcher: glob.compile_matcher(),
                kinds,
            });
        }
        if globs.is_empty() {
            continue;
        }
        out.push(WatchedFilesRegistration {
            registration_id: id,
            globs,
        });
    }
    out
}

/// LSP's `globPattern` is either a plain string (relative or
/// absolute glob) or a `RelativePattern { baseUri, pattern }`
/// object. We extract just the pattern string here; the
/// `globset` matcher we feed it into already handles relative
/// matches against absolute paths via the `**/` prefix
/// servers conventionally use.
fn parse_watcher_glob_pattern(watcher: &Value) -> Option<String> {
    let pat = watcher.get("globPattern")?;
    if let Some(s) = pat.as_str() {
        return Some(s.to_string());
    }
    pat.get("pattern")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// One parsed `Unregistration` entry from a
/// `client/unregisterCapability` payload. Same narrowing as
/// `parse_register_capability_watched_files`: only entries
/// whose method is `workspace/didChangeWatchedFiles` make it
/// out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedFilesUnregistration {
    pub registration_id: String,
}

pub fn parse_unregister_capability_watched_files(
    params: &Value,
) -> Vec<WatchedFilesUnregistration> {
    let Some(arr) = params.get("unregisterations").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in arr {
        let method = entry.get("method").and_then(|v| v.as_str()).unwrap_or("");
        if method != "workspace/didChangeWatchedFiles" {
            continue;
        }
        let id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(WatchedFilesUnregistration {
            registration_id: id,
        });
    }
    out
}

/// What the initialize response tells us about delivery mode,
/// quiescence, and completion support. Fed directly into
/// `DiagnosticSource::set_mode` / `set_has_quiescence`; the
/// completion fields drive whether the manager honours
/// `LspCmd::RequestCompletion` for a given server at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InitializeCapabilities {
    /// Server advertised `capabilities.diagnosticProvider` — we
    /// enter pull mode. `false` keeps the default push mode.
    pub diagnostic_provider: bool,
    /// Server advertised `capabilities.experimental.serverStatusNotification`
    /// (rust-analyzer's quiescence extension). Until the first
    /// `experimental/serverStatus quiescent=true` arrives, pull
    /// requests should be deferred.
    pub has_quiescence: bool,
    /// Server advertised `capabilities.completionProvider` —
    /// we can send `textDocument/completion` requests. Without
    /// this, completion commands are dropped.
    pub completion_provider: bool,
    /// `capabilities.completionProvider.triggerCharacters`.
    /// When the user's last-typed char matches one of these,
    /// the dispatcher fires a fresh completion request; in
    /// every other case a request only flies on explicit
    /// invocation. Empty vec = no trigger chars (identifier-
    /// only auto-trigger still applies).
    pub completion_trigger_chars: Vec<char>,
    /// Server advertised `capabilities.completionProvider.resolveProvider`.
    /// Controls whether the runtime fires `completionItem/resolve`
    /// on commit to fetch `additionalTextEdits`.
    pub completion_resolve_provider: bool,
}

/// Parse the `result` body of an `initialize` response into the
/// fields `DiagnosticSource` cares about. Tolerant of missing
/// sub-objects — a server that returns `{"capabilities":{}}` is
/// valid and means push-only, no quiescence.
pub fn parse_initialize_response(result: &Value) -> InitializeCapabilities {
    let caps = result.get("capabilities").unwrap_or(&Value::Null);
    let completion = caps.get("completionProvider");
    let completion_provider =
        completion.is_some() && !completion.is_some_and(|v| v.is_null());
    let completion_trigger_chars = completion
        .and_then(|c| c.get("triggerCharacters"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter_map(|s| s.chars().next())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let completion_resolve_provider = completion
        .and_then(|c| c.get("resolveProvider"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    InitializeCapabilities {
        diagnostic_provider: caps.get("diagnosticProvider").is_some()
            && !caps.get("diagnosticProvider").is_some_and(|v| v.is_null()),
        has_quiescence: caps
            .pointer("/experimental/serverStatusNotification")
            .is_some_and(|v| v.as_bool().unwrap_or(false)),
        completion_provider,
        completion_trigger_chars,
        completion_resolve_provider,
    }
}

/// Completion response parsed into the runtime's wire shape.
/// `prefix_start_col` is the char col where the user's typed
/// prefix begins — extracted from the first item's `textEdit`
/// when present, otherwise `None` (caller falls back to
/// identifier backtracking against the rope).
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub items: Vec<CompletionItem>,
    pub prefix_start_col: Option<u32>,
}

/// Parse either `{"items": [...]}` (a `CompletionList`) or a raw
/// `[...]` (a `CompletionItem[]`). Drops items without a `label`
/// silently — they can't display anyway. `cursor_line` is the
/// row the request was issued against; kept in the signature so
/// future refinement (e.g. "use cursor line as textEdit.line
/// default") doesn't need a breaking change. The current
/// implementation relies on the caller (the manager) to thread
/// the line through the `LspEvent::Completion.prefix_line`
/// field directly.
pub fn parse_completion_response(result: &Value, _cursor_line: u32) -> CompletionResponse {
    let raw_items: &[Value] = match result {
        Value::Array(arr) => arr.as_slice(),
        Value::Object(_) => result
            .get("items")
            .and_then(|v| v.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[]),
        _ => &[],
    };

    let mut items: Vec<CompletionItem> = Vec::with_capacity(raw_items.len());
    let mut prefix_start_col: Option<u32> = None;

    for raw in raw_items {
        let Some(label) = raw.get("label").and_then(|v| v.as_str()) else {
            continue;
        };
        let detail = raw
            .get("detail")
            .and_then(|v| v.as_str())
            .map(Arc::<str>::from);
        let sort_text = raw
            .get("sortText")
            .and_then(|v| v.as_str())
            .map(Arc::<str>::from);
        let insert_text = raw
            .get("insertText")
            .and_then(|v| v.as_str())
            .map(Arc::<str>::from);
        let kind = raw.get("kind").and_then(|v| v.as_u64()).map(|n| n as u8);
        let text_edit = raw.get("textEdit").and_then(parse_completion_text_edit);
        if prefix_start_col.is_none()
            && let Some(te) = text_edit.as_ref() {
                prefix_start_col = Some(te.col_start);
            }
        // Resolve flag: true when the server advertises
        // completionProvider.resolveProvider AND the item
        // doesn't already carry its additional edits. We err
        // on the side of "ask" — the legacy driver does the
        // same, and servers quick-reply with empty edits when
        // there's nothing to add.
        let has_additional = raw
            .get("additionalTextEdits")
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty());
        let resolve_needed = !has_additional;
        let resolve_data = raw
            .get("data")
            .map(|v| Arc::<str>::from(v.to_string()));
        items.push(CompletionItem {
            label: Arc::<str>::from(label),
            detail,
            sort_text,
            insert_text,
            text_edit,
            kind,
            resolve_needed,
            resolve_data,
        });
    }

    CompletionResponse {
        items,
        prefix_start_col,
    }
}

/// Parse one LSP `TextEdit` (within a `CompletionItem`) into
/// our wire type. Returns `None` when the shape is malformed.
fn parse_completion_text_edit(v: &Value) -> Option<CompletionTextEdit> {
    let range = v.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    let line = start.get("line").and_then(|v| v.as_u64())? as u32;
    let col_start = start.get("character").and_then(|v| v.as_u64())? as u32;
    // LSP allows multi-line edit ranges, but we collapse to one
    // line — legacy's `convert_completion_response` does the
    // same. If a server wants multi-line replacement on commit
    // it can do so via additionalTextEdits.
    let col_end = end.get("character").and_then(|v| v.as_u64())? as u32;
    let new_text = v.get("newText").and_then(|v| v.as_str()).unwrap_or("");
    Some(CompletionTextEdit {
        line,
        col_start,
        col_end,
        new_text: Arc::<str>::from(new_text),
    })
}

/// Extract `additionalTextEdits` from a `completionItem/resolve`
/// response. Returns an empty `Vec` if the server omits them
/// (common when there's nothing extra to apply).
pub fn parse_resolve_additional_edits(result: &Value) -> Vec<CompletionTextEdit> {
    let Some(arr) = result.get("additionalTextEdits").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter().filter_map(parse_completion_text_edit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;
    use serde_json::json;
    use std::path::Path;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    // ── URI encoding ────────────────────────────────────────

    #[test]
    fn uri_from_plain_ascii_path() {
        let p = canon("/tmp/foo.rs");
        assert!(uri_from_path(&p).starts_with("file://"));
        assert!(uri_from_path(&p).ends_with("foo.rs"));
    }

    #[test]
    fn uri_percent_encodes_spaces() {
        let raw = Path::new("/tmp/my project/main.rs");
        // Short-circuit `canonicalize` for an unreal path by
        // constructing a CanonPath via UserPath::canonicalize —
        // the fallback returns the path verbatim when missing.
        let p = UserPath::new(raw).canonicalize();
        let uri = uri_from_path(&p);
        assert!(uri.contains("my%20project"), "{uri}");
    }

    #[test]
    fn uri_preserves_slashes_and_alphanumerics() {
        let raw = Path::new("/a/b/c_file-1.ts");
        let p = UserPath::new(raw).canonicalize();
        let uri = uri_from_path(&p);
        assert!(uri.contains("/a/b/c_file-1.ts"), "{uri}");
    }

    #[test]
    fn uri_round_trip_for_ascii_path() {
        let raw = std::path::PathBuf::from("/tmp/a-b_c.rs");
        let p = UserPath::new(&raw).canonicalize();
        let uri = uri_from_path(&p);
        let back = path_from_uri(&uri).unwrap();
        assert_eq!(back, p.as_path());
    }

    #[test]
    fn uri_round_trip_decodes_percent() {
        let uri = "file:///tmp/my%20project/main.rs";
        let back = path_from_uri(uri).unwrap();
        assert_eq!(back.to_string_lossy(), "/tmp/my project/main.rs");
    }

    #[test]
    fn path_from_uri_rejects_non_file_scheme() {
        assert!(path_from_uri("http://example.com/foo").is_none());
    }

    // ── Language IDs ────────────────────────────────────────

    #[test]
    fn language_ids_match_lsp_spec_canonical_names() {
        use led_state_syntax::Language;
        assert_eq!(language_id(Language::Rust), "rust");
        assert_eq!(language_id(Language::TypeScript), "typescript");
        assert_eq!(language_id(Language::JavaScript), "javascript");
        // Bash → "shellscript" per LSP spec, NOT "bash".
        assert_eq!(language_id(Language::Bash), "shellscript");
        // Make → "makefile".
        assert_eq!(language_id(Language::Make), "makefile");
    }

    // ── Initialize request ──────────────────────────────────

    fn parse_body(body: &[u8]) -> Value {
        serde_json::from_slice(body).expect("valid JSON")
    }

    // ── client/registerCapability parsing ────────────────────

    #[test]
    fn parse_register_capability_extracts_globs() {
        let params = json!({
            "registrations": [{
                "id": "watched-files-1",
                "method": "workspace/didChangeWatchedFiles",
                "registerOptions": {
                    "watchers": [
                        { "globPattern": "**/Cargo.toml" },
                        { "globPattern": "**/*.rs", "kind": 7 }
                    ]
                }
            }]
        });
        let regs = parse_register_capability_watched_files(&params);
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].registration_id, "watched-files-1");
        assert_eq!(regs[0].globs.len(), 2);
        assert_eq!(regs[0].globs[0].pattern, "**/Cargo.toml");
        // Kind defaults to 7 when omitted.
        assert_eq!(regs[0].globs[0].kinds, 0b111);
        assert_eq!(regs[0].globs[1].kinds, 0b111);
    }

    #[test]
    fn parse_register_capability_skips_unrelated_methods() {
        let params = json!({
            "registrations": [{
                "id": "x",
                "method": "textDocument/completion",
                "registerOptions": {}
            }]
        });
        assert!(parse_register_capability_watched_files(&params).is_empty());
    }

    #[test]
    fn parse_unregister_capability_extracts_ids() {
        let params = json!({
            "unregisterations": [{
                "id": "watched-files-1",
                "method": "workspace/didChangeWatchedFiles"
            }, {
                "id": "trigger-chars",
                "method": "textDocument/completion"
            }]
        });
        let unregs = parse_unregister_capability_watched_files(&params);
        assert_eq!(unregs.len(), 1);
        assert_eq!(unregs[0].registration_id, "watched-files-1");
    }

    #[test]
    fn registration_glob_matches_absolute_path() {
        let params = json!({
            "registrations": [{
                "id": "w1",
                "method": "workspace/didChangeWatchedFiles",
                "registerOptions": {
                    "watchers": [{ "globPattern": "**/*.toml" }]
                }
            }]
        });
        let regs = parse_register_capability_watched_files(&params);
        let g = &regs[0].globs[0];
        assert!(g.matcher.is_match("/private/tmp/x/Cargo.toml"));
        assert!(g.matcher.is_match("Cargo.toml"));
        assert!(!g.matcher.is_match("/private/tmp/x/main.rs"));
    }

    // ── didChangeWatchedFiles outbound notification ──────────

    #[test]
    fn build_did_change_watched_files_renders_uri_and_kind() {
        let p = canon("/tmp/x/Cargo.toml");
        let body = build_did_change_watched_files_notification(&[FileEvent {
            path: p.clone(),
            kind: FileEventKind::Changed,
        }]);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["method"], "workspace/didChangeWatchedFiles");
        let changes = v["params"]["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["type"], 2);
        let uri = changes[0]["uri"].as_str().unwrap();
        assert!(uri.starts_with("file://"));
        assert!(uri.ends_with("Cargo.toml"));
    }

    #[test]
    fn initialize_request_carries_id_root_and_method() {
        let body = build_initialize_request(1, &canon("/workspace"));
        let v = parse_body(&body);
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["method"], "initialize");
        let uri = v["params"]["rootUri"].as_str().unwrap();
        assert!(uri.starts_with("file://"));
        assert!(uri.ends_with("/workspace"));
    }

    #[test]
    fn initialize_request_advertises_diagnostic_capability() {
        let body = build_initialize_request(1, &canon("/w"));
        let v = parse_body(&body);
        // Pull-capable servers only turn on `diagnosticProvider`
        // if we advertise `textDocument.diagnostic`.
        assert!(v["params"]["capabilities"]["textDocument"]["diagnostic"].is_object());
        // Push servers also need the pair advertised.
        assert!(
            v["params"]["capabilities"]["textDocument"]["publishDiagnostics"]
                .is_object()
        );
    }

    #[test]
    fn initialize_request_advertises_work_done_progress() {
        // rust-analyzer gates `$/progress` emission on this flag.
        // Without it, the status bar shows only the server name
        // (no indexing/building detail) during cold-start.
        let body = build_initialize_request(1, &canon("/w"));
        let v = parse_body(&body);
        assert_eq!(
            v["params"]["capabilities"]["window"]["workDoneProgress"],
            true
        );
    }

    #[test]
    fn initialize_request_advertises_quiescence_extension() {
        // rust-analyzer enables its serverStatus emission only when
        // the client opts into the experimental capability.
        let body = build_initialize_request(1, &canon("/w"));
        let v = parse_body(&body);
        assert_eq!(
            v["params"]["capabilities"]["experimental"]["serverStatusNotification"],
            true
        );
    }

    #[test]
    fn initialize_request_advertises_workspace_configuration() {
        // Servers ask `workspace/configuration` at startup and
        // stall if we didn't advertise support.
        let body = build_initialize_request(1, &canon("/w"));
        let v = parse_body(&body);
        assert_eq!(
            v["params"]["capabilities"]["workspace"]["configuration"],
            true
        );
    }

    #[test]
    fn initialized_notification_is_empty_and_correct_method() {
        let body = build_initialized_notification();
        let v = parse_body(&body);
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "initialized");
        assert!(v.get("id").is_none(), "notifications have no id");
        assert_eq!(v["params"], json!({}));
    }

    #[test]
    fn did_change_configuration_uses_empty_settings_object() {
        let body = build_did_change_configuration_notification();
        let v = parse_body(&body);
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "workspace/didChangeConfiguration");
        assert!(v.get("id").is_none());
        assert_eq!(v["params"]["settings"], json!({}));
    }

    // ── Initialize response parsing ────────────────────────

    #[test]
    fn parse_response_with_empty_capabilities_is_push_no_quiescence() {
        let c = parse_initialize_response(&json!({"capabilities": {}}));
        assert!(!c.diagnostic_provider);
        assert!(!c.has_quiescence);
    }

    #[test]
    fn parse_response_with_diagnostic_provider_sets_flag() {
        let c = parse_initialize_response(&json!({
            "capabilities": {"diagnosticProvider": {"identifier": "x"}}
        }));
        assert!(c.diagnostic_provider);
    }

    #[test]
    fn parse_response_with_diagnostic_provider_null_does_not_set_flag() {
        // LSP allows `diagnosticProvider: null` to mean "not
        // supported"; treat that as push mode.
        let c = parse_initialize_response(&json!({
            "capabilities": {"diagnosticProvider": null}
        }));
        assert!(!c.diagnostic_provider);
    }

    #[test]
    fn parse_response_recognises_server_status_notification() {
        let c = parse_initialize_response(&json!({
            "capabilities": {"experimental": {"serverStatusNotification": true}}
        }));
        assert!(c.has_quiescence);
    }

    #[test]
    fn parse_response_ignores_server_status_notification_false() {
        let c = parse_initialize_response(&json!({
            "capabilities": {"experimental": {"serverStatusNotification": false}}
        }));
        assert!(!c.has_quiescence);
    }

    #[test]
    fn parse_response_tolerates_missing_capabilities_key() {
        let c = parse_initialize_response(&json!({}));
        assert!(!c.diagnostic_provider);
        assert!(!c.has_quiescence);
    }

    #[test]
    fn parse_response_extracts_completion_capabilities() {
        let c = parse_initialize_response(&json!({
            "capabilities": {
                "completionProvider": {
                    "triggerCharacters": [".", ":", "("],
                    "resolveProvider": true,
                }
            }
        }));
        assert!(c.completion_provider);
        assert_eq!(c.completion_trigger_chars, vec!['.', ':', '(']);
        assert!(c.completion_resolve_provider);
    }

    #[test]
    fn parse_response_defaults_completion_when_provider_absent() {
        let c = parse_initialize_response(&json!({"capabilities": {}}));
        assert!(!c.completion_provider);
        assert!(c.completion_trigger_chars.is_empty());
        assert!(!c.completion_resolve_provider);
    }

    #[test]
    fn completion_response_accepts_list_and_array_forms() {
        // LSP allows either `{"items": [...]}` (incomplete list)
        // or a raw array; we must handle both.
        let list = json!({
            "isIncomplete": false,
            "items": [
                { "label": "foo", "sortText": "0foo" },
                { "label": "bar" },
            ]
        });
        let parsed = parse_completion_response(&list, 0);
        assert_eq!(parsed.items.len(), 2);
        assert_eq!(parsed.items[0].label.as_ref(), "foo");
        assert_eq!(parsed.items[0].sort_text.as_ref().unwrap().as_ref(), "0foo");
        assert_eq!(parsed.items[1].label.as_ref(), "bar");

        let arr = json!([
            { "label": "baz", "detail": "fn() -> Baz" },
        ]);
        let parsed = parse_completion_response(&arr, 0);
        assert_eq!(parsed.items.len(), 1);
        assert_eq!(parsed.items[0].detail.as_ref().unwrap().as_ref(), "fn() -> Baz");
    }

    #[test]
    fn completion_response_extracts_prefix_start_col_from_text_edit() {
        let resp = json!({
            "items": [{
                "label": "println!",
                "textEdit": {
                    "range": {
                        "start": { "line": 0, "character": 5 },
                        "end":   { "line": 0, "character": 7 }
                    },
                    "newText": "println!"
                }
            }]
        });
        let parsed = parse_completion_response(&resp, 0);
        assert_eq!(parsed.prefix_start_col, Some(5));
        assert_eq!(parsed.items.len(), 1);
        let te = parsed.items[0].text_edit.as_ref().unwrap();
        assert_eq!(te.col_start, 5);
        assert_eq!(te.col_end, 7);
        assert_eq!(te.new_text.as_ref(), "println!");
    }
}
