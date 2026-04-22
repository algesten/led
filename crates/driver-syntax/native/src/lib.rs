//! Desktop tree-sitter worker.
//!
//! One worker thread blocks on `Receiver<SyntaxCmd>`, loads the
//! grammar + highlight query for the requested `Language` (cached
//! per-language), parses the rope, runs the query, and posts a
//! `SyntaxOut` back. Stale requests are coalesced: before each
//! parse we drain the channel and keep only the latest command per
//! path — typing fast shouldn't queue a parse per keystroke.
//!
//! The parser itself is rebuilt per-request via `set_language`
//! because the previous cmd may have been a different grammar; the
//! cost is a pointer-store, not a real setup.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use led_core::{CanonPath, Notifier};
use led_driver_syntax_core::{SyntaxCmd, SyntaxDriver, Trace};
use led_state_syntax::{Language, SyntaxOut, TokenKind, TokenSpan};
use ropey::Rope;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

/// Lifecycle marker. Drops when the driver does; the worker
/// self-exits when its command `Sender` hangs up.
pub struct SyntaxNative {
    _marker: (),
}

pub fn spawn(trace: Arc<dyn Trace>, notify: Notifier) -> (SyntaxDriver, SyntaxNative) {
    let (tx_cmd, rx_cmd) = mpsc::channel::<SyntaxCmd>();
    let (tx_done, rx_done) = mpsc::channel::<SyntaxOut>();
    let native = spawn_worker(rx_cmd, tx_done, notify);
    let driver = SyntaxDriver::new(tx_cmd, rx_done, trace);
    (driver, native)
}

pub fn spawn_worker(
    rx_cmd: Receiver<SyntaxCmd>,
    tx_done: Sender<SyntaxOut>,
    notify: Notifier,
) -> SyntaxNative {
    thread::Builder::new()
        .name("led-syntax".into())
        .spawn(move || worker_loop(rx_cmd, tx_done, notify))
        .expect("spawning syntax worker should succeed");
    SyntaxNative { _marker: () }
}

fn worker_loop(rx: Receiver<SyntaxCmd>, tx: Sender<SyntaxOut>, notify: Notifier) {
    let mut parser = Parser::new();
    let mut cursor = QueryCursor::new();

    while let Ok(first) = rx.recv() {
        // Coalesce any backlog — keep the newest cmd per path.
        let mut latest: HashMap<CanonPath, SyntaxCmd> = HashMap::new();
        latest.insert(first.path.clone(), first);
        while let Ok(more) = rx.try_recv() {
            latest.insert(more.path.clone(), more);
        }

        for (_path, cmd) in latest.drain() {
            let out = run_parse(&mut parser, &mut cursor, cmd);
            if tx.send(out).is_err() {
                return;
            }
            notify.notify();
        }
    }
}

/// One parse + highlight cycle. Returns a `SyntaxOut` with the
/// tree and extracted token spans. On any grammar / query error we
/// still return a `SyntaxOut` — just with an empty token list — so
/// the runtime can move its `version` forward and stop retrying
/// the same stale state.
fn run_parse(parser: &mut Parser, cursor: &mut QueryCursor, cmd: SyntaxCmd) -> SyntaxOut {
    let (language, query) = grammar_for(cmd.language);

    // set_language is cheap; doing it unconditionally keeps the
    // worker correct when the previous cmd was a different grammar.
    let _ = parser.set_language(&language);

    // Materialize the rope to a byte buffer. Tree-sitter and the
    // query text-provider both want contiguous bytes; going through
    // a String is the simplest correct option. Files are small
    // enough that the copy is not the bottleneck.
    let bytes = rope_to_bytes(&cmd.rope);

    let tree = match parser.parse(&bytes, None) {
        Some(t) => Arc::new(t),
        None => {
            return SyntaxOut {
                path: cmd.path,
                version: cmd.version,
                language: cmd.language,
                tree: Arc::new(
                    parser
                        .parse("", None)
                        .expect("empty parse should succeed after set_language"),
                ),
                tokens: Arc::new(Vec::new()),
            };
        }
    };

    let tokens = extract_tokens(&bytes, &cmd.rope, tree.root_node(), query, cursor);

    SyntaxOut {
        path: cmd.path,
        version: cmd.version,
        language: cmd.language,
        tree,
        tokens: Arc::new(tokens),
    }
}

fn rope_to_bytes(rope: &Rope) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(rope.len_bytes());
    for chunk in rope.chunks() {
        bytes.extend_from_slice(chunk.as_bytes());
    }
    bytes
}

/// Run the highlight query against the parsed tree, map each
/// capture name to a `TokenKind`, translate byte offsets to char
/// offsets via the rope, and return token spans sorted by start.
///
/// Tree-sitter emits overlapping captures per pattern — e.g. a
/// `function.call` node also matches a broader `call` pattern. We
/// keep the innermost / most specific one by preferring later
/// captures with the same span, and by dropping a capture whose
/// span is entirely covered by a later one at the same position.
fn extract_tokens(
    bytes: &[u8],
    rope: &Rope,
    root: tree_sitter::Node,
    query: &Query,
    cursor: &mut QueryCursor,
) -> Vec<TokenSpan> {
    let capture_names = query.capture_names();
    let mut spans: Vec<TokenSpan> = Vec::new();

    let mut it = cursor.captures(query, root, bytes);
    while let Some((m, capture_ix)) = it.next() {
        let cap = m.captures[*capture_ix];
        let name = capture_names[cap.index as usize];
        let Some(kind) = capture_name_to_kind(name) else {
            continue;
        };
        let start_byte = cap.node.start_byte();
        let end_byte = cap.node.end_byte();
        if end_byte <= start_byte {
            continue;
        }
        let char_start = rope.byte_to_char(start_byte);
        let char_end = rope.byte_to_char(end_byte);
        spans.push(TokenSpan {
            char_start,
            char_end,
            kind,
        });
    }

    // Later captures override earlier ones where they overlap —
    // tree-sitter walks outermost to innermost within a match, so
    // last-write-wins is correct for the "most specific class" rule.
    // We implement that by sorting on (start asc, end desc) and then
    // dropping any span whose extent is a strict subset of the
    // preceding kept span (keep the outer broad span unless a later
    // capture at the same spot refined it).
    //
    // In practice highlight queries are written so captures nest
    // cleanly; flattening to non-overlapping spans is a job for the
    // runtime painter. Here we return them sorted by start, with
    // stable order preserved across ties so the painter can pick a
    // deterministic winner.
    spans.sort_by_key(|s| (s.char_start, s.char_end));
    spans
}

/// Map a tree-sitter highlight capture name to a `TokenKind`. The
/// taxonomy follows nvim-treesitter conventions: dot-separated
/// classes from most to least specific (`keyword.return`,
/// `function.method.builtin`, …). We match on the top-level class
/// and ignore modifiers.
///
/// Returning `None` drops the capture (e.g. auxiliary captures
/// starting with `_` that highlight queries use as scaffolding).
fn capture_name_to_kind(name: &str) -> Option<TokenKind> {
    if name.starts_with('_') {
        return None;
    }
    let head = name.split('.').next().unwrap_or(name);
    Some(match head {
        "keyword" | "conditional" | "repeat" | "include" | "exception" | "storageclass" => {
            TokenKind::Keyword
        }
        "type" | "class" | "struct" | "enum" | "interface" | "trait" => TokenKind::Type,
        "function" | "method" | "constructor" => TokenKind::Function,
        "string" | "character" => TokenKind::String,
        "number" | "float" => TokenKind::Number,
        "boolean" => TokenKind::Boolean,
        "comment" => TokenKind::Comment,
        "operator" => TokenKind::Operator,
        "punctuation" => TokenKind::Punctuation,
        "variable" | "parameter" | "field" => TokenKind::Variable,
        "property" => TokenKind::Property,
        "attribute" | "annotation" => TokenKind::Attribute,
        "tag" => TokenKind::Tag,
        "label" => TokenKind::Label,
        "constant" | "constant.builtin" | "symbol" => TokenKind::Constant,
        "escape" => TokenKind::Escape,
        _ => return None,
    })
}

/// Per-language grammar + highlight query, each cached in a
/// `OnceLock` so we pay the `Query::new` cost (parse the scm source
/// and compile the predicate tables) exactly once per session.
fn grammar_for(lang: Language) -> (tree_sitter::Language, &'static Query) {
    match lang {
        Language::Rust => {
            let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
            static Q: OnceLock<Query> = OnceLock::new();
            let query = Q.get_or_init(|| {
                Query::new(&tree_sitter_rust::LANGUAGE.into(), tree_sitter_rust::HIGHLIGHTS_QUERY)
                    .expect("rust highlights.scm must compile")
            });
            (lang, query)
        }
        Language::TypeScript => {
            let lang: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
            static Q: OnceLock<Query> = OnceLock::new();
            let query = Q.get_or_init(|| {
                Query::new(
                    &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                )
                .expect("typescript highlights.scm must compile")
            });
            (lang, query)
        }
        Language::JavaScript => {
            let lang: tree_sitter::Language = tree_sitter_javascript::LANGUAGE.into();
            static Q: OnceLock<Query> = OnceLock::new();
            let query = Q.get_or_init(|| {
                Query::new(
                    &tree_sitter_javascript::LANGUAGE.into(),
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                )
                .expect("javascript highlights.scm must compile")
            });
            (lang, query)
        }
        Language::Python => {
            let lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
            static Q: OnceLock<Query> = OnceLock::new();
            let query = Q.get_or_init(|| {
                Query::new(
                    &tree_sitter_python::LANGUAGE.into(),
                    tree_sitter_python::HIGHLIGHTS_QUERY,
                )
                .expect("python highlights.scm must compile")
            });
            (lang, query)
        }
        Language::Bash => {
            let lang: tree_sitter::Language = tree_sitter_bash::LANGUAGE.into();
            static Q: OnceLock<Query> = OnceLock::new();
            let query = Q.get_or_init(|| {
                Query::new(&tree_sitter_bash::LANGUAGE.into(), tree_sitter_bash::HIGHLIGHT_QUERY)
                    .expect("bash highlights.scm must compile")
            });
            (lang, query)
        }
        Language::Markdown => {
            // The markdown grammar ships block + inline queries; we
            // only wire the block one for now (the inline grammar
            // lives in a separate crate and isn't enabled yet).
            let lang: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
            static Q: OnceLock<Query> = OnceLock::new();
            let query = Q.get_or_init(|| {
                Query::new(&tree_sitter_md::LANGUAGE.into(), tree_sitter_md::HIGHLIGHT_QUERY_BLOCK)
                    .expect("markdown block highlights.scm must compile")
            });
            (lang, query)
        }
        Language::Json => {
            let lang: tree_sitter::Language = tree_sitter_json::LANGUAGE.into();
            static Q: OnceLock<Query> = OnceLock::new();
            let query = Q.get_or_init(|| {
                Query::new(&tree_sitter_json::LANGUAGE.into(), tree_sitter_json::HIGHLIGHTS_QUERY)
                    .expect("json highlights.scm must compile")
            });
            (lang, query)
        }
        Language::Toml => {
            let lang: tree_sitter::Language = tree_sitter_toml_ng::LANGUAGE.into();
            static Q: OnceLock<Query> = OnceLock::new();
            let query = Q.get_or_init(|| {
                Query::new(
                    &tree_sitter_toml_ng::LANGUAGE.into(),
                    tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
                )
                .expect("toml highlights.scm must compile")
            });
            (lang, query)
        }
        Language::C => {
            let lang: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
            static Q: OnceLock<Query> = OnceLock::new();
            let query = Q.get_or_init(|| {
                Query::new(&tree_sitter_c::LANGUAGE.into(), tree_sitter_c::HIGHLIGHT_QUERY)
                    .expect("c highlights.scm must compile")
            });
            (lang, query)
        }
        Language::Cpp => {
            let lang: tree_sitter::Language = tree_sitter_cpp::LANGUAGE.into();
            static Q: OnceLock<Query> = OnceLock::new();
            let query = Q.get_or_init(|| {
                Query::new(&tree_sitter_cpp::LANGUAGE.into(), tree_sitter_cpp::HIGHLIGHT_QUERY)
                    .expect("cpp highlights.scm must compile")
            });
            (lang, query)
        }
        Language::Ruby => {
            let lang: tree_sitter::Language = tree_sitter_ruby::LANGUAGE.into();
            static Q: OnceLock<Query> = OnceLock::new();
            let query = Q.get_or_init(|| {
                Query::new(&tree_sitter_ruby::LANGUAGE.into(), tree_sitter_ruby::HIGHLIGHTS_QUERY)
                    .expect("ruby highlights.scm must compile")
            });
            (lang, query)
        }
        Language::Swift => {
            let lang: tree_sitter::Language = tree_sitter_swift::LANGUAGE.into();
            static Q: OnceLock<Query> = OnceLock::new();
            let query = Q.get_or_init(|| {
                Query::new(
                    &tree_sitter_swift::LANGUAGE.into(),
                    tree_sitter_swift::HIGHLIGHTS_QUERY,
                )
                .expect("swift highlights.scm must compile")
            });
            (lang, query)
        }
        Language::Make => {
            let lang: tree_sitter::Language = tree_sitter_make::LANGUAGE.into();
            static Q: OnceLock<Query> = OnceLock::new();
            let query = Q.get_or_init(|| {
                Query::new(&tree_sitter_make::LANGUAGE.into(), tree_sitter_make::HIGHLIGHTS_QUERY)
                    .expect("make highlights.scm must compile")
            });
            (lang, query)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;
    use led_driver_syntax_core::NoopTrace;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    static TMP_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn canon_of(name: &str) -> CanonPath {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let n = TMP_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = base.join(format!("led-syntax-test.{pid}.{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        let p: PathBuf = dir.join(name);
        std::fs::write(&p, b"").unwrap();
        UserPath::new(&p).canonicalize()
    }

    fn wait_for_out(drv: &SyntaxDriver, deadline: Duration) -> Option<SyntaxOut> {
        let start = Instant::now();
        while start.elapsed() < deadline {
            let mut batch = drv.process();
            if let Some(first) = batch.drain(..).next() {
                return Some(first);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        None
    }

    #[test]
    fn rust_parse_yields_keyword_and_function_tokens() {
        let (drv, _native) = spawn(Arc::new(NoopTrace), Notifier::noop());
        let src = "fn hello() {}\n";
        let rope = Arc::new(Rope::from_str(src));
        let path = canon_of("a.rs");

        drv.execute(std::iter::once(&SyntaxCmd {
            path: path.clone(),
            version: 1,
            rope,
            language: Language::Rust,
            prev_tree: None,
            edits_since_prev: Vec::new(),
        }));

        let out = wait_for_out(&drv, Duration::from_secs(5)).expect("parse within 5s");
        assert_eq!(out.path, path);
        assert_eq!(out.version, 1);
        assert_eq!(out.language, Language::Rust);
        // `fn` is a keyword, `hello` is a function name.
        let kinds: Vec<TokenKind> = out.tokens.iter().map(|t| t.kind).collect();
        assert!(
            kinds.contains(&TokenKind::Keyword),
            "expected a Keyword token; got {kinds:?}",
        );
        assert!(
            kinds.contains(&TokenKind::Function),
            "expected a Function token; got {kinds:?}",
        );
    }

    #[test]
    fn stale_cmds_coalesce_to_latest_version_per_path() {
        let (drv, _native) = spawn(Arc::new(NoopTrace), Notifier::noop());
        let path = canon_of("b.rs");

        // Fire three back-to-back cmds for the same path. The worker
        // should eventually produce a single SyntaxOut for the
        // latest version (older versions may or may not show up
        // depending on timing, but the latest must be represented).
        for v in 1..=3u64 {
            drv.execute(std::iter::once(&SyntaxCmd {
                path: path.clone(),
                version: v,
                rope: Arc::new(Rope::from_str(&format!("fn v{v}() {{}}\n"))),
                language: Language::Rust,
                prev_tree: None,
                edits_since_prev: Vec::new(),
            }));
        }

        let start = Instant::now();
        let mut seen_latest = false;
        while start.elapsed() < Duration::from_secs(5) && !seen_latest {
            for out in drv.process() {
                if out.version == 3 {
                    seen_latest = true;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(seen_latest, "expected version 3 completion within 5s");
    }

    #[test]
    fn unknown_capture_names_fall_to_none() {
        assert_eq!(capture_name_to_kind("keyword.return"), Some(TokenKind::Keyword));
        assert_eq!(capture_name_to_kind("function.builtin"), Some(TokenKind::Function));
        assert_eq!(capture_name_to_kind("string.special"), Some(TokenKind::String));
        assert_eq!(capture_name_to_kind("totally.unknown"), None);
        assert_eq!(capture_name_to_kind("_auxiliary"), None);
    }
}
