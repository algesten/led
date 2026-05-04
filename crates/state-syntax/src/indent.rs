//! Auto-indent suggestion via tree-sitter (M23).
//!
//! Port of legacy `led/crates/syntax/src/indent.rs`, translated to
//! ropey + the rewrite's `Language` enum. Public surface is the
//! single function [`suggest_indent`] — the runtime calls it from
//! `dispatch::edit::insert_tab` and `dispatch::edit::insert_newline`
//! to compute the indent string for a line, given the current parse
//! tree.
//!
//! The function returns `None` when:
//! * the language has no `indents.scm`,
//! * `line == 0` (no basis row to reference),
//! * the tree query yields no opinion for the line.
//!
//! Callers fall back to "match previous line's leading whitespace"
//! (newline) or "insert spaces to next 4-col tab stop" (tab) when
//! `None` comes back.
//!
//! Architectural note: `Query::new` is called against
//! `tree.language()` (a `LanguageRef` deref-ing to `&Language`), not
//! against a fresh `tree_sitter_<lang>::LANGUAGE.into()`. This keeps
//! `state-syntax`'s dep graph free of all the per-grammar crates —
//! the `Tree` already carries its grammar pointer, and re-using that
//! pointer is what `tree_sitter::Query` requires anyway (queries are
//! grammar-bound).

use std::ops::Range;
use std::sync::OnceLock;

use ropey::Rope;
use tree_sitter::{Query, QueryCursor, StreamingIterator, Tree};

use crate::Language;

/// The per-language indent query source. Returns `None` for
/// languages that don't ship an `indents.scm`.
fn indents_src(lang: Language) -> Option<&'static str> {
    Some(match lang {
        Language::Rust => include_str!("../queries/rust/indents.scm"),
        Language::TypeScript | Language::Tsx => {
            include_str!("../queries/typescript/indents.scm")
        }
        Language::JavaScript | Language::Jsx => {
            include_str!("../queries/javascript/indents.scm")
        }
        Language::Python => include_str!("../queries/python/indents.scm"),
        Language::Bash => include_str!("../queries/bash/indents.scm"),
        Language::Json => include_str!("../queries/json/indents.scm"),
        Language::Toml => include_str!("../queries/toml/indents.scm"),
        Language::C => include_str!("../queries/c/indents.scm"),
        Language::Swift => include_str!("../queries/swift/indents.scm"),
        // No indents.scm shipped for these languages.
        Language::Markdown
        | Language::Cpp
        | Language::Ruby
        | Language::Make => return None,
    })
}

/// Pre-compile every supported language's indent + imports
/// queries on the calling thread. Call this once at runtime
/// startup before the syntax driver thread spawns. Pre-warming
/// the per-language `OnceLock`s eliminates a contention window
/// where concurrent `Query::new` calls (one in the driver
/// compiling a highlight query, one in dispatch compiling an
/// indent query) race in tree-sitter's FFI and stall.
///
/// Swift and C are deliberately excluded: under the goldens
/// harness's `portable-pty`, calling `Query::new` for the Swift
/// grammar at startup hangs the entire binary (key dispatch
/// stops responding — Down / Right / Tab all get dropped). The
/// same call works fine under a normal Terminal.app PTY, so
/// this looks like a `portable-pty` × tree-sitter-swift
/// static-init interaction rather than a problem with the Swift
/// grammar itself. Lazy-compiling those two on first Tab inside
/// the relevant buffer is the safe workaround; the cost is a
/// one-time ~60ms hit per Swift / C file. If the harness PTY
/// gets re-evaluated (e.g. switching to `expectrl`), this
/// exclusion is the first thing to drop.
///
/// Adding a NEW language to the precompile list: verify under
/// the goldens harness (`cd goldens && cargo test --release
/// --test smoke -- move_cursor_down_right`) before merging.
pub fn precompile_all_queries() {
    for lang in [
        Language::Rust,
        Language::TypeScript,
        Language::Tsx,
        Language::JavaScript,
        Language::Jsx,
        Language::Python,
        Language::Bash,
        Language::Json,
        Language::Toml,
    ] {
        if let Some(ts) = ts_language(lang) {
            let _ = config_for(lang, &ts);
            crate::import::warm(lang, &ts);
        }
    }
}

/// Map a `Language` to its `tree_sitter::Language` grammar handle.
/// Importing the grammars here (rather than relying on
/// `tree.language()` from a worker-parsed tree) avoids a hang in
/// `Query::new` that surfaces when the language pointer crosses
/// thread boundaries on macOS — see the Cargo.toml comment on the
/// grammar deps.
pub(crate) fn ts_language(lang: Language) -> Option<tree_sitter::Language> {
    Some(match lang {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        // tree-sitter-javascript's grammar parses both JS and JSX,
        // so `Jsx` reuses the same `LANGUAGE` handle.
        Language::JavaScript | Language::Jsx => tree_sitter_javascript::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Bash => tree_sitter_bash::LANGUAGE.into(),
        Language::Json => tree_sitter_json::LANGUAGE.into(),
        Language::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
        Language::C => tree_sitter_c::LANGUAGE.into(),
        Language::Swift => tree_sitter_swift::LANGUAGE.into(),
        Language::Markdown
        | Language::Cpp
        | Language::Ruby
        | Language::Make => return None,
    })
}

/// Compiled query + capture indices for one language. We compile
/// lazily on first use and cache forever (`OnceLock` guarded
/// `HashMap`); subsequent calls are a hashmap probe.
struct IndentsConfig {
    query: Query,
    indent_capture_ix: u32,
    start_capture_ix: Option<u32>,
    end_capture_ix: Option<u32>,
    outdent_capture_ix: Option<u32>,
}

fn config_for(lang: Language, ts_lang: &tree_sitter::Language) -> Option<&'static IndentsConfig> {
    macro_rules! slot {
        () => {{
            static SLOT: OnceLock<IndentsConfig> = OnceLock::new();
            &SLOT
        }};
    }
    let slot: &'static OnceLock<IndentsConfig> = match lang {
        Language::Rust => slot!(),
        Language::TypeScript => slot!(),
        Language::Tsx => slot!(),
        Language::JavaScript => slot!(),
        Language::Jsx => slot!(),
        Language::Python => slot!(),
        Language::Bash => slot!(),
        Language::Json => slot!(),
        Language::Toml => slot!(),
        Language::C => slot!(),
        Language::Swift => slot!(),
        Language::Markdown | Language::Cpp | Language::Ruby | Language::Make => return None,
    };
    let cfg = slot.get_or_init(|| {
        let src = indents_src(lang)
            .unwrap_or_else(|| panic!("no indent query for {:?}", lang));
        let query = Query::new(ts_lang, src)
            .unwrap_or_else(|e| panic!("{:?} indents.scm: {e}", lang));
        let names = query.capture_names();
        let indent_ix = names
            .iter()
            .position(|n| *n == "indent")
            .unwrap_or_else(|| panic!("{:?} indents.scm missing @indent capture", lang))
            as u32;
        let start_ix = names.iter().position(|n| *n == "start").map(|i| i as u32);
        let end_ix = names.iter().position(|n| *n == "end").map(|i| i as u32);
        let outdent_ix = names.iter().position(|n| *n == "outdent").map(|i| i as u32);
        IndentsConfig {
            query,
            indent_capture_ix: indent_ix,
            start_capture_ix: start_ix,
            end_capture_ix: end_ix,
            outdent_capture_ix: outdent_ix,
        }
    });
    Some(cfg)
}

/// Indentation delta relative to the basis row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndentDelta {
    Greater,
    Less,
    Equal,
}

#[derive(Debug, Clone)]
struct IndentSuggestion {
    basis_row: usize,
    delta: IndentDelta,
}

/// Top-level public API. Returns the indent string the editor
/// should set as the line's leading whitespace.
pub fn suggest_indent(
    lang: Language,
    tree: &Tree,
    rope: &Rope,
    line: usize,
) -> Option<String> {
    let ts_lang = ts_language(lang)?;
    let cfg = config_for(lang, &ts_lang)?;
    let indent_unit = detect_indent_unit(rope);

    // Closing brackets at line start match the opener's line
    // indent — beats the structural query for `}`-on-its-own.
    if let Some(text) = closing_bracket_indent(tree, rope, line) {
        return Some(text);
    }

    let suggestion = suggest_indent_inner(cfg, tree, rope, line)?;
    let basis_indent = get_line_indent(rope, suggestion.basis_row);
    Some(apply_indent_delta(
        &basis_indent,
        suggestion.delta,
        &indent_unit,
    ))
}

/// Inner walk: run the indent query over a basis-anchored byte range
/// and assemble `IndentSuggestion`. Mirrors legacy
/// `suggest_indent_with_tree`.
fn suggest_indent_inner(
    cfg: &IndentsConfig,
    tree: &Tree,
    rope: &Rope,
    line: usize,
) -> Option<IndentSuggestion> {
    let basis_row = find_basis_row(rope, line)?;

    let total_bytes = rope.len_bytes();
    let line_count = rope.len_lines();
    let line_start = rope.line_to_byte(line);
    let query_start = indent_query_start(rope, tree, basis_row, line);
    let query_end = if line + 1 < line_count {
        rope.line_to_byte(line + 1)
    } else {
        total_bytes
    };

    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(query_start..query_end);

    // (range, explicitly_terminated). Bare continuation constructs
    // (call_expression, field_expression, …) have no `@end`, so
    // their natural node end must NOT trigger outdent on a sibling
    // line.
    let mut indent_ranges: Vec<(Range<usize>, bool)> = Vec::new();

    let bytes = rope_to_bytes(rope);
    let mut matches = cursor.matches(&cfg.query, tree.root_node(), bytes.as_slice());
    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut node_range: Option<Range<usize>> = None;
        let mut outdent_pos: Option<usize> = None;
        let mut has_end = false;

        for cap in m.captures {
            if cap.index == cfg.indent_capture_ix {
                node_range = Some(cap.node.start_byte()..cap.node.end_byte());
            } else if Some(cap.index) == cfg.start_capture_ix {
                let end_pos = cap.node.end_byte();
                if let Some(ref mut nr) = node_range {
                    nr.start = end_pos;
                } else {
                    node_range = Some(end_pos..query_end);
                }
            } else if Some(cap.index) == cfg.end_capture_ix {
                has_end = true;
                let start_pos = cap.node.start_byte();
                if let Some(ref mut nr) = node_range {
                    nr.end = start_pos;
                }
            } else if Some(cap.index) == cfg.outdent_capture_ix {
                outdent_pos = Some(cap.node.start_byte());
            }
        }

        if let Some(range) = node_range
            && range.start < range.end
        {
            indent_ranges.push((range, has_end));
        }

        if let Some(pos) = outdent_pos {
            for (r, terminated) in &mut indent_ranges {
                if r.start <= pos && pos < r.end {
                    r.end = pos;
                    *terminated = true;
                }
            }
        }
    }

    let len = total_bytes;

    // Unwrap basis row from continuation constructs unless it
    // opens a block that extends past `line`. Mirrors legacy
    // `basis_row` recompute.
    let basis_byte = rope.line_to_byte(basis_row);
    let opens_block = indent_ranges.iter().any(|(r, _)| {
        r.start < len && rope.byte_to_line(r.start) == basis_row && r.end > line_start
    });
    let basis_row = if opens_block {
        basis_row
    } else {
        let mut row = basis_row;
        for (r, terminated) in &indent_ranges {
            if *terminated {
                continue;
            }
            if r.start < basis_byte && r.end > basis_byte && r.end <= line_start {
                let start_line = rope.byte_to_line(r.start);
                if start_line < row {
                    row = start_line;
                }
            }
        }
        row
    };

    let basis_opens_indent = indent_ranges
        .iter()
        .filter(|(r, _)| r.start < len)
        .any(|(r, _)| rope.byte_to_line(r.start) == basis_row && r.end > line_start);

    let line_at_outdent = indent_ranges
        .iter()
        .filter(|(r, terminated)| *terminated && r.end > 0 && r.end <= len)
        .any(|(r, _)| rope.byte_to_line(r.end) == line && r.start < line_start);

    let delta = if line_at_outdent {
        IndentDelta::Less
    } else if basis_opens_indent {
        IndentDelta::Greater
    } else {
        IndentDelta::Equal
    };

    Some(IndentSuggestion { basis_row, delta })
}

fn rope_to_bytes(rope: &Rope) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(rope.len_bytes());
    for chunk in rope.chunks() {
        bytes.extend_from_slice(chunk.as_bytes());
    }
    bytes
}

fn indent_query_start(rope: &Rope, tree: &Tree, basis_row: usize, line: usize) -> usize {
    let basis_start = rope.line_to_byte(basis_row);
    let line_byte = rope.line_to_byte(line);
    let mut node = tree
        .root_node()
        .descendant_for_byte_range(line_byte, line_byte);
    while let Some(n) = node {
        if n.start_byte() < basis_start {
            return n.start_byte();
        }
        node = n.parent();
    }
    basis_start
}

fn find_basis_row(rope: &Rope, line: usize) -> Option<usize> {
    if line == 0 {
        return None;
    }
    for r in (0..line).rev() {
        let l = rope.line(r);
        if l.chars().any(|c| !c.is_whitespace()) {
            return Some(r);
        }
    }
    Some(0)
}

/// For a line whose first non-whitespace char is `}` / `)` / `]`,
/// match the opener's line indent. Falls through (returns `None`)
/// for any other shape.
fn closing_bracket_indent(tree: &Tree, rope: &Rope, line: usize) -> Option<String> {
    if line >= rope.len_lines() {
        return None;
    }
    let line_slice = rope.line(line);
    let mut col_off = 0;
    let mut found = false;
    for ch in line_slice.chars() {
        if ch == ' ' || ch == '\t' {
            col_off += 1;
            continue;
        }
        if ch != '}' && ch != ')' && ch != ']' {
            return None;
        }
        found = true;
        break;
    }
    if !found {
        return None;
    }
    let bracket_char_idx = rope.line_to_char(line) + col_off;
    let bracket_byte = rope.char_to_byte(bracket_char_idx);
    let node = tree
        .root_node()
        .descendant_for_byte_range(bracket_byte, bracket_byte + 1)?;
    let parent = node.parent()?;
    Some(get_line_indent(rope, parent.start_position().row))
}

/// Leading whitespace of `line` — the basis-indent input to
/// `apply_indent_delta`.
pub fn get_line_indent(rope: &Rope, line: usize) -> String {
    if line >= rope.len_lines() {
        return String::new();
    }
    let mut indent = String::new();
    for ch in rope.line(line).chars() {
        if ch == ' ' || ch == '\t' {
            indent.push(ch);
        } else {
            break;
        }
    }
    indent
}

/// `Greater` adds one unit; `Less` removes one unit; `Equal`
/// returns the basis unchanged. Mirrors legacy.
fn apply_indent_delta(basis_indent: &str, delta: IndentDelta, indent_unit: &str) -> String {
    match delta {
        IndentDelta::Greater => {
            let mut s = basis_indent.to_string();
            s.push_str(indent_unit);
            s
        }
        IndentDelta::Less => {
            let s = basis_indent.to_string();
            if s.ends_with(indent_unit) {
                s[..s.len() - indent_unit.len()].to_string()
            } else if s.ends_with('\t') {
                s[..s.len() - 1].to_string()
            } else {
                let trimmed = s.trim_end_matches(' ');
                let removed = s.len() - trimmed.len();
                if removed > 0 {
                    let remove_count = removed.min(indent_unit.len());
                    s[..s.len() - remove_count].to_string()
                } else {
                    s
                }
            }
        }
        IndentDelta::Equal => basis_indent.to_string(),
    }
}

/// Detect whether the buffer indents with tabs or N-space stops.
/// First-look at the first 100 lines. Matches legacy.
pub fn detect_indent_unit(rope: &Rope) -> String {
    let lines = rope.len_lines().min(100);
    for r in 0..lines {
        let mut indent = String::new();
        for ch in rope.line(r).chars() {
            if ch == '\t' {
                return "\t".to_string();
            } else if ch == ' ' {
                indent.push(' ');
            } else {
                break;
            }
        }
        if !indent.is_empty() {
            return indent;
        }
    }
    "    ".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tree_sitter::Parser;

    fn parse_rust(src: &str) -> (Tree, Arc<Rope>) {
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(src, None).unwrap();
        (tree, Arc::new(Rope::from_str(src)))
    }

    #[test]
    fn detect_indent_unit_finds_4_spaces() {
        let r = Rope::from_str("fn main() {\n    let x = 1;\n}\n");
        assert_eq!(detect_indent_unit(&r), "    ");
    }

    #[test]
    fn detect_indent_unit_finds_tab() {
        let r = Rope::from_str("fn main() {\n\tlet x = 1;\n}\n");
        assert_eq!(detect_indent_unit(&r), "\t");
    }

    #[test]
    fn detect_indent_unit_defaults_to_4_spaces_on_unindented_file() {
        let r = Rope::from_str("plain\nlines\nhere\n");
        assert_eq!(detect_indent_unit(&r), "    ");
    }

    #[test]
    fn apply_indent_delta_greater_adds_one_unit() {
        assert_eq!(apply_indent_delta("    ", IndentDelta::Greater, "    "), "        ");
        assert_eq!(apply_indent_delta("", IndentDelta::Greater, "\t"), "\t");
    }

    #[test]
    fn apply_indent_delta_less_removes_one_unit() {
        assert_eq!(apply_indent_delta("        ", IndentDelta::Less, "    "), "    ");
        assert_eq!(apply_indent_delta("\t\t", IndentDelta::Less, "\t"), "\t");
    }

    #[test]
    fn suggest_indent_returns_none_on_row_zero() {
        let (tree, rope) = parse_rust("fn main() {}\n");
        assert_eq!(suggest_indent(Language::Rust, &tree, &rope, 0), None);
    }

    #[test]
    fn suggest_indent_inside_block_returns_basis_plus_unit() {
        // basis row is `fn main() {`; new line inside block should
        // indent one unit.
        let (tree, rope) = parse_rust("fn main() {\n\n}\n");
        // line 1 (empty) sits inside the `{ … }` opened by line 0.
        let s = suggest_indent(Language::Rust, &tree, &rope, 1);
        assert_eq!(s.as_deref(), Some("    "));
    }

    #[test]
    fn suggest_indent_closing_bracket_matches_opener() {
        // `}` on line 2 matches line 0's indent (empty).
        let (tree, rope) = parse_rust("fn main() {\n    let x = 1;\n}\n");
        let s = suggest_indent(Language::Rust, &tree, &rope, 2);
        assert_eq!(s.as_deref(), Some(""));
    }

    #[test]
    fn suggest_indent_unknown_language_returns_none() {
        let (tree, rope) = parse_rust("fn main() {}\n");
        // Pretend the tree was parsed with markdown → the cache
        // returns None because Markdown has no indents.scm. (The
        // tree's actual language is Rust, but `lang` is what we
        // route on.)
        assert_eq!(suggest_indent(Language::Markdown, &tree, &rope, 1), None);
    }

    #[test]
    fn get_line_indent_extracts_leading_whitespace() {
        let r = Rope::from_str("    let x = 1;\n");
        assert_eq!(get_line_indent(&r, 0), "    ");

        let r = Rope::from_str("\t\tnested\n");
        assert_eq!(get_line_indent(&r, 0), "\t\t");

        let r = Rope::from_str("flush\n");
        assert_eq!(get_line_indent(&r, 0), "");
    }
}
