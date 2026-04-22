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
///
/// Most languages use a single grammar. Markdown is special: the
/// block grammar parses paragraph / heading / list structure while
/// the inline grammar parses `**emphasis**`, `` `code` ``, links,
/// etc. We parse both against the same bytes and merge the raw
/// captures before flattening so the painter sees one coherent
/// span list.
fn run_parse(parser: &mut Parser, cursor: &mut QueryCursor, cmd: SyntaxCmd) -> SyntaxOut {
    let grammars = grammars_for(cmd.language);

    // Materialize the rope to a byte buffer. Tree-sitter and the
    // query text-provider both want contiguous bytes; going through
    // a String is the simplest correct option. Files are small
    // enough that the copy is not the bottleneck.
    let bytes = rope_to_bytes(&cmd.rope);

    let mut primary_tree: Option<Arc<tree_sitter::Tree>> = None;
    let mut raw_captures: Vec<(usize, usize, TokenKind)> = Vec::new();

    for (language, query) in &grammars {
        // set_language is cheap; doing it unconditionally keeps the
        // worker correct across language switches + between the
        // block / inline passes for the same cmd.
        let _ = parser.set_language(language);
        let Some(tree) = parser.parse(&bytes, None) else {
            continue;
        };
        collect_raw_captures(
            &bytes,
            &cmd.rope,
            tree.root_node(),
            query,
            cursor,
            &mut raw_captures,
        );
        if primary_tree.is_none() {
            primary_tree = Some(Arc::new(tree));
        }
    }

    let tree = match primary_tree {
        Some(t) => t,
        None => Arc::new(
            parser
                .parse("", None)
                .expect("empty parse should succeed after set_language"),
        ),
    };
    let tokens = flatten_nested(raw_captures);

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

/// Run a single highlight query against a parsed tree, appending
/// its captures (as `(char_start, char_end, kind)` triples) to
/// `out` without flattening. Callers may make multiple calls for
/// multi-grammar languages (markdown's block + inline) and flatten
/// once across the union.
fn collect_raw_captures(
    bytes: &[u8],
    rope: &Rope,
    root: tree_sitter::Node,
    query: &Query,
    cursor: &mut QueryCursor,
    out: &mut Vec<(usize, usize, TokenKind)>,
) {
    let capture_names = query.capture_names();
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
        out.push((char_start, char_end, kind));
    }
}

/// Flatten a set of nested / overlapping `(start, end, kind)`
/// captures into a sequence of non-overlapping spans that picks the
/// innermost capture at each character.
///
/// Uses a sweep over open/close events with a stack of currently-
/// active kinds: the top of the stack wins for the next run. Equal-
/// range captures resolve to the later-opened one, matching tree-
/// sitter's "later pattern overrides earlier" convention.
fn flatten_nested(captures: Vec<(usize, usize, TokenKind)>) -> Vec<TokenSpan> {
    if captures.is_empty() {
        return Vec::new();
    }
    // Assign a stable id per capture so close events can find the
    // matching open on the stack even when ranges coincide.
    #[derive(Clone, Copy)]
    enum Ev {
        Open(TokenKind, usize),
        Close(usize),
    }
    let mut events: Vec<(usize, u8, Ev)> = Vec::with_capacity(captures.len() * 2);
    for (i, (start, end, kind)) in captures.iter().enumerate() {
        // Tie-breaker at same position: closes go first (0), opens
        // after (1). Prevents a zero-width overlap between a span
        // that ends at N and another that starts at N.
        events.push((*start, 1, Ev::Open(*kind, i)));
        events.push((*end, 0, Ev::Close(i)));
    }
    events.sort_by_key(|(pos, tie, _)| (*pos, *tie));

    let mut stack: Vec<(TokenKind, usize)> = Vec::new();
    let mut out: Vec<TokenSpan> = Vec::new();
    let mut cursor: Option<usize> = None;

    for (pos, _tie, ev) in events {
        if let (Some(start), Some(&(kind, _))) = (cursor, stack.last())
            && pos > start
        {
            // Close the previous run using the currently-innermost
            // style. Merge with the trailing output span if kinds
            // match (avoids an N-char span being emitted as N
            // singleton spans when a wrapping capture opens and
            // closes many children inside it).
            if let Some(last) = out.last_mut()
                && last.kind == kind
                && last.char_end == start
            {
                last.char_end = pos;
            } else {
                out.push(TokenSpan {
                    char_start: start,
                    char_end: pos,
                    kind,
                });
            }
        }
        match ev {
            Ev::Open(kind, id) => stack.push((kind, id)),
            Ev::Close(id) => {
                if let Some(pos_in_stack) = stack.iter().rposition(|&(_, i)| i == id) {
                    stack.remove(pos_in_stack);
                }
            }
        }
        cursor = Some(pos);
    }

    out
}

/// Map a tree-sitter highlight capture name to a `TokenKind`. The
/// taxonomy follows nvim-treesitter conventions: dot-separated
/// classes from most to least specific (`keyword.return`,
/// `function.method.builtin`, …). We match on the top-level class
/// and ignore modifiers — except for the `text.*` family used by
/// the markdown grammar, where each sub-kind has a different
/// semantic colour (title / literal / uri / reference all
/// distinct), so those are matched on the full dotted name first.
///
/// Returning `None` drops the capture (e.g. auxiliary captures
/// starting with `_` that highlight queries use as scaffolding).
fn capture_name_to_kind(name: &str) -> Option<TokenKind> {
    if name.starts_with('_') {
        return None;
    }
    // Markdown-specific: `text.*` captures route to distinct colour
    // slots (legacy behaviour: title→label, literal→string,
    // reference→attribute, uri→keyword). Checked before the head-
    // based match because the head `"text"` alone is ambiguous.
    match name {
        "text.title" => return Some(TokenKind::Label),
        "text.literal" => return Some(TokenKind::String),
        "text.reference" => return Some(TokenKind::Attribute),
        "text.uri" => return Some(TokenKind::Keyword),
        "text.strong" | "text.emphasis" => return Some(TokenKind::Label),
        _ => {}
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

/// Per-language list of `(grammar, highlight query)` pairs. Each
/// pair is compiled once per session behind a `OnceLock`. Most
/// languages return a single pair; markdown returns two — block
/// and inline — which the worker runs sequentially against the
/// same bytes and merges before flattening.
fn grammars_for(lang: Language) -> Vec<(tree_sitter::Language, &'static Query)> {
    fn compile(lang: tree_sitter::Language, src: &str, label: &str) -> Query {
        Query::new(&lang, src).unwrap_or_else(|e| panic!("{label} highlights.scm: {e}"))
    }
    match lang {
        Language::Rust => {
            static Q: OnceLock<Query> = OnceLock::new();
            let l: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
            let q = Q.get_or_init(|| compile(l.clone(), tree_sitter_rust::HIGHLIGHTS_QUERY, "rust"));
            vec![(l, q)]
        }
        Language::TypeScript => {
            static Q: OnceLock<Query> = OnceLock::new();
            let l: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
            let q = Q
                .get_or_init(|| compile(l.clone(), tree_sitter_typescript::HIGHLIGHTS_QUERY, "typescript"));
            vec![(l, q)]
        }
        Language::JavaScript => {
            static Q: OnceLock<Query> = OnceLock::new();
            let l: tree_sitter::Language = tree_sitter_javascript::LANGUAGE.into();
            let q = Q
                .get_or_init(|| compile(l.clone(), tree_sitter_javascript::HIGHLIGHT_QUERY, "javascript"));
            vec![(l, q)]
        }
        Language::Python => {
            static Q: OnceLock<Query> = OnceLock::new();
            let l: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
            let q = Q.get_or_init(|| compile(l.clone(), tree_sitter_python::HIGHLIGHTS_QUERY, "python"));
            vec![(l, q)]
        }
        Language::Bash => {
            static Q: OnceLock<Query> = OnceLock::new();
            let l: tree_sitter::Language = tree_sitter_bash::LANGUAGE.into();
            let q = Q.get_or_init(|| compile(l.clone(), tree_sitter_bash::HIGHLIGHT_QUERY, "bash"));
            vec![(l, q)]
        }
        Language::Markdown => {
            // Block grammar for headings / lists / fences; inline
            // grammar for `**bold**`, `_em_`, links, code spans.
            // Run both against the same bytes — the tokens merge
            // at flatten time.
            static QB: OnceLock<Query> = OnceLock::new();
            static QI: OnceLock<Query> = OnceLock::new();
            let block: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
            let inline: tree_sitter::Language = tree_sitter_md::INLINE_LANGUAGE.into();
            let qb = QB.get_or_init(|| {
                compile(block.clone(), tree_sitter_md::HIGHLIGHT_QUERY_BLOCK, "markdown-block")
            });
            let qi = QI.get_or_init(|| {
                compile(inline.clone(), tree_sitter_md::HIGHLIGHT_QUERY_INLINE, "markdown-inline")
            });
            vec![(block, qb), (inline, qi)]
        }
        Language::Json => {
            static Q: OnceLock<Query> = OnceLock::new();
            let l: tree_sitter::Language = tree_sitter_json::LANGUAGE.into();
            let q = Q.get_or_init(|| compile(l.clone(), tree_sitter_json::HIGHLIGHTS_QUERY, "json"));
            vec![(l, q)]
        }
        Language::Toml => {
            static Q: OnceLock<Query> = OnceLock::new();
            let l: tree_sitter::Language = tree_sitter_toml_ng::LANGUAGE.into();
            let q = Q.get_or_init(|| compile(l.clone(), tree_sitter_toml_ng::HIGHLIGHTS_QUERY, "toml"));
            vec![(l, q)]
        }
        Language::C => {
            static Q: OnceLock<Query> = OnceLock::new();
            let l: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
            let q = Q.get_or_init(|| compile(l.clone(), tree_sitter_c::HIGHLIGHT_QUERY, "c"));
            vec![(l, q)]
        }
        Language::Cpp => {
            static Q: OnceLock<Query> = OnceLock::new();
            let l: tree_sitter::Language = tree_sitter_cpp::LANGUAGE.into();
            let q = Q.get_or_init(|| compile(l.clone(), tree_sitter_cpp::HIGHLIGHT_QUERY, "cpp"));
            vec![(l, q)]
        }
        Language::Ruby => {
            static Q: OnceLock<Query> = OnceLock::new();
            let l: tree_sitter::Language = tree_sitter_ruby::LANGUAGE.into();
            let q = Q.get_or_init(|| compile(l.clone(), tree_sitter_ruby::HIGHLIGHTS_QUERY, "ruby"));
            vec![(l, q)]
        }
        Language::Swift => {
            static Q: OnceLock<Query> = OnceLock::new();
            let l: tree_sitter::Language = tree_sitter_swift::LANGUAGE.into();
            let q = Q.get_or_init(|| compile(l.clone(), tree_sitter_swift::HIGHLIGHTS_QUERY, "swift"));
            vec![(l, q)]
        }
        Language::Make => {
            static Q: OnceLock<Query> = OnceLock::new();
            let l: tree_sitter::Language = tree_sitter_make::LANGUAGE.into();
            let q = Q.get_or_init(|| compile(l.clone(), tree_sitter_make::HIGHLIGHTS_QUERY, "make"));
            vec![(l, q)]
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

    fn flat(spans: &[led_state_syntax::TokenSpan]) -> Vec<(usize, usize, TokenKind)> {
        spans
            .iter()
            .map(|s| (s.char_start, s.char_end, s.kind))
            .collect()
    }

    #[test]
    fn flatten_inner_capture_wins_over_outer_wrapper() {
        // Outer @attribute covers [0, 30). Inner @type covers [8, 14).
        // Inner's range should render as Type; the rest of the outer
        // range as Attribute — three contiguous runs.
        let out = flatten_nested(vec![
            (0, 30, TokenKind::Attribute),
            (8, 14, TokenKind::Type),
        ]);
        assert_eq!(
            flat(&out),
            vec![
                (0, 8, TokenKind::Attribute),
                (8, 14, TokenKind::Type),
                (14, 30, TokenKind::Attribute),
            ]
        );
    }

    #[test]
    fn flatten_equal_range_later_capture_wins() {
        // Two captures on the same node. Tree-sitter convention:
        // later-declared pattern overrides. We preserve iteration
        // order; the later one opens on top of the stack → wins.
        let out = flatten_nested(vec![
            (5, 10, TokenKind::Type),
            (5, 10, TokenKind::Function),
        ]);
        assert_eq!(flat(&out), vec![(5, 10, TokenKind::Function)]);
    }

    #[test]
    fn flatten_adjacent_same_kind_coalesces() {
        // Two peers of the same kind touching at a boundary should
        // merge into one run (avoids unnecessary style-toggle emits).
        let out = flatten_nested(vec![
            (0, 5, TokenKind::Keyword),
            (5, 10, TokenKind::Keyword),
        ]);
        assert_eq!(flat(&out), vec![(0, 10, TokenKind::Keyword)]);
    }

    #[test]
    fn flatten_preserves_non_overlapping_runs() {
        let out = flatten_nested(vec![
            (0, 2, TokenKind::Keyword),
            (3, 7, TokenKind::Function),
        ]);
        assert_eq!(
            flat(&out),
            vec![(0, 2, TokenKind::Keyword), (3, 7, TokenKind::Function)]
        );
    }
}
