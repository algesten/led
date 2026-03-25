use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use tree_sitter::{InputEdit, Parser, Query, Tree};

use led_core::{Doc, EditOp};

use crate::bracket;
use crate::config::*;
use crate::highlight::{HighlightSpan, collect_highlights};
use crate::indent;
use crate::injection::{self, InjectionLayer, QueryCache};
use crate::language::{lang_for_ext, lang_for_filename};
use crate::parse::parse_doc;

pub struct SyntaxState {
    parser: Parser,
    tree: Tree,
    highlights_query: Query,
    indents_config: Option<IndentsConfig>,
    brackets_config: Option<BracketsConfig>,
    outline_config: Option<OutlineConfig>,
    injections_config: Option<InjectionConfig>,
    imports_config: Option<ImportsConfig>,
    error_query: Option<Query>,
    injection_layers: Vec<InjectionLayer>,
    injection_query_cache: QueryCache,
    increase_indent_pattern: Option<regex::Regex>,
    decrease_indent_pattern: Option<regex::Regex>,
    reindent_chars: Arc<[char]>,
}

impl SyntaxState {
    pub fn from_path_and_doc(path: &Path, doc: &dyn Doc) -> Option<Self> {
        let entry = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(lang_for_ext)
            .or_else(|| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(lang_for_filename)
            })?;

        let mut parser = Parser::new();
        parser.set_language(&entry.language).ok()?;

        let highlights_query = Query::new(&entry.language, &entry.highlights_query).ok()?;
        let tree = parse_doc(&mut parser, doc, None)?;

        let indents_config = entry
            .indents_query
            .and_then(|q| compile_indents_config(&entry.language, q));
        let brackets_config = entry
            .brackets_query
            .and_then(|q| compile_brackets_config(&entry.language, q));
        let outline_config = entry
            .outline_query
            .and_then(|q| compile_outline_config(&entry.language, q));
        let injections_config = entry
            .injections_query
            .and_then(|q| compile_injection_config(&entry.language, q));
        let imports_config = entry
            .imports_query
            .and_then(|q| compile_imports_config(&entry.language, q));
        let error_query = Query::new(&entry.language, "(ERROR) @error").ok();

        let increase_indent_pattern = entry
            .increase_indent_pattern
            .and_then(|p| regex::Regex::new(p).ok());
        let decrease_indent_pattern = entry
            .decrease_indent_pattern
            .and_then(|p| regex::Regex::new(p).ok());

        let mut injection_query_cache = QueryCache::new();
        let mut injection_layers = Vec::new();
        if let Some(ref inj_config) = injections_config {
            injection_layers = injection::build_injection_layers(
                inj_config,
                &tree,
                doc,
                &mut injection_query_cache,
            );
        }

        let reindent_chars: Arc<[char]> = entry.reindent_chars.into();

        Some(Self {
            parser,
            tree,
            highlights_query,
            indents_config,
            brackets_config,
            outline_config,
            injections_config,
            imports_config,
            error_query,
            injection_layers,
            injection_query_cache,
            increase_indent_pattern,
            decrease_indent_pattern,
            reindent_chars,
        })
    }

    /// Apply an EditOp from the document to the parse tree.
    /// The doc should already be the NEW doc (after the edit was applied).
    /// We reconstruct the InputEdit from the EditOp and re-parse.
    pub fn apply_edit_op(&mut self, op: &EditOp, doc: &dyn Doc) {
        // Reconstruct InputEdit from EditOp
        // EditOp { offset (char), old_text, new_text }
        let start_byte = doc.char_to_byte(op.offset.min(doc.byte_to_char(doc.len_bytes())));
        let start_line = doc.char_to_line(op.offset.min(doc.byte_to_char(doc.len_bytes())));
        let start_line_byte = doc.line_to_byte(start_line);

        let start_position = tree_sitter::Point {
            row: start_line,
            column: start_byte - start_line_byte,
        };

        // Compute old_end from old_text
        let old_len_bytes = op.old_text.len();
        let old_end_byte = start_byte + old_len_bytes;
        let mut old_end_row = start_position.row;
        let mut old_end_col = start_position.column;
        for b in op.old_text.bytes() {
            if b == b'\n' {
                old_end_row += 1;
                old_end_col = 0;
            } else {
                old_end_col += 1;
            }
        }

        // Compute new_end from new_text
        let new_len_bytes = op.new_text.len();
        let new_end_byte = start_byte + new_len_bytes;
        let mut new_end_row = start_position.row;
        let mut new_end_col = start_position.column;
        for b in op.new_text.bytes() {
            if b == b'\n' {
                new_end_row += 1;
                new_end_col = 0;
            } else {
                new_end_col += 1;
            }
        }

        let edit = InputEdit {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_position,
            old_end_position: tree_sitter::Point {
                row: old_end_row,
                column: old_end_col,
            },
            new_end_position: tree_sitter::Point {
                row: new_end_row,
                column: new_end_col,
            },
        };

        self.tree.edit(&edit);
        if let Some(new_tree) = parse_doc(&mut self.parser, doc, Some(&self.tree)) {
            self.tree = new_tree;
        }

        if let Some(ref inj_config) = self.injections_config {
            self.injection_layers = injection::build_injection_layers(
                inj_config,
                &self.tree,
                doc,
                &mut self.injection_query_cache,
            );
        }
    }

    /// Full re-parse without incremental info.  Use when edit ops are
    /// unavailable — passing the old tree without a prior `tree.edit()`
    /// would leave stale byte offsets in reused nodes.
    pub fn reparse(&mut self, doc: &dyn Doc) {
        if let Some(new_tree) = parse_doc(&mut self.parser, doc, None) {
            self.tree = new_tree;
        }
        if let Some(ref inj_config) = self.injections_config {
            self.injection_layers = injection::build_injection_layers(
                inj_config,
                &self.tree,
                doc,
                &mut self.injection_query_cache,
            );
        }
    }

    pub fn clone_tree(&self) -> Tree {
        self.tree.clone()
    }

    // ── Highlights ──

    pub fn highlights_for_lines(
        &self,
        doc: &dyn Doc,
        start_line: usize,
        end_line: usize,
    ) -> Vec<(usize, HighlightSpan)> {
        let total_lines = doc.line_count();
        if start_line >= total_lines {
            return Vec::new();
        }
        let end_line = end_line.min(total_lines);
        let start_byte = doc.line_to_byte(start_line);
        let end_byte = if end_line >= total_lines {
            doc.len_bytes()
        } else {
            doc.line_to_byte(end_line)
        };

        let mut result = collect_highlights(
            &self.highlights_query,
            &self.tree,
            doc,
            start_byte,
            end_byte,
            start_line,
            end_line,
        );

        for layer in &self.injection_layers {
            let overlaps = layer
                .included_ranges
                .iter()
                .any(|r| r.start_byte < end_byte && r.end_byte > start_byte);
            if !overlaps {
                continue;
            }
            let injection_hl = collect_highlights(
                &layer.highlights_query,
                &layer.tree,
                doc,
                start_byte,
                end_byte,
                start_line,
                end_line,
            );
            result.extend(injection_hl);
        }

        result
    }

    // ── Brackets ──

    pub fn bracket_ranges(&self, doc: &dyn Doc, range: Range<usize>) -> Vec<bracket::BracketMatch> {
        match &self.brackets_config {
            Some(config) => bracket::bracket_ranges(config, &self.tree, doc, range),
            None => Vec::new(),
        }
    }

    pub fn matching_bracket(
        &self,
        doc: &dyn Doc,
        row: usize,
        col: usize,
    ) -> Option<(usize, usize)> {
        let config = self.brackets_config.as_ref()?;
        bracket::matching_bracket(config, &self.tree, doc, row, col)
    }

    // ── Outline ──

    pub fn outline(&self, doc: &dyn Doc) -> Vec<crate::outline::OutlineItem> {
        match &self.outline_config {
            Some(config) => crate::outline::outline(config, &self.tree, doc),
            None => Vec::new(),
        }
    }

    // ── Imports ──

    pub fn imports(&self, doc: &dyn Doc) -> Vec<crate::import::ImportItem> {
        match &self.imports_config {
            Some(config) => crate::import::imports(config, &self.tree, doc),
            None => Vec::new(),
        }
    }

    // ── Indentation ──

    pub fn reindent_chars(&self) -> &Arc<[char]> {
        &self.reindent_chars
    }

    pub fn suggest_indent(&self, doc: &dyn Doc, line: usize) -> Option<IndentSuggestion> {
        let config = self.indents_config.as_ref()?;
        indent::suggest_indent(config, &self.tree, self.error_query.as_ref(), doc, line)
    }

    pub fn suggest_indent_with_tree(
        &self,
        doc: &dyn Doc,
        tree: &Tree,
        line: usize,
    ) -> Option<IndentSuggestion> {
        let config = self.indents_config.as_ref()?;
        indent::suggest_indent_with_tree(config, tree, self.error_query.as_ref(), doc, line)
    }

    pub fn closing_bracket_indent(&self, doc: &dyn Doc, line: usize) -> Option<String> {
        indent::closing_bracket_indent(&self.tree, doc, line)
    }

    /// Compute auto-indent for a line using two-pass tree-sitter analysis with regex fallback.
    pub fn compute_auto_indent(&self, doc: &dyn Doc, line: usize) -> Option<String> {
        let indent_unit = indent::detect_indent_unit(doc);

        // For closing brackets, match the opening bracket's line indent
        if let Some(indent_text) = self.closing_bracket_indent(doc, line) {
            return Some(indent_text);
        }

        // Try tree-sitter based indent
        let new_suggestion = self.suggest_indent(doc, line);

        if let Some(suggestion) = new_suggestion {
            // If within error and we have regex patterns, try regex fallback
            if suggestion.within_error {
                if let Some(indent_text) = indent::regex_indent(
                    doc,
                    line,
                    &indent_unit,
                    self.increase_indent_pattern.as_ref(),
                    self.decrease_indent_pattern.as_ref(),
                ) {
                    return Some(indent_text);
                }
            }

            let basis_indent = indent::get_line_indent(doc, suggestion.basis_row);
            return Some(indent::apply_indent_delta(
                &basis_indent,
                suggestion.delta,
                &indent_unit,
            ));
        }

        // Fallback: regex only
        if let Some(indent_text) = indent::regex_indent(
            doc,
            line,
            &indent_unit,
            self.increase_indent_pattern.as_ref(),
            self.decrease_indent_pattern.as_ref(),
        ) {
            return Some(indent_text);
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::TextDoc;
    use std::sync::Arc;

    fn make_doc(text: &str) -> Arc<dyn Doc> {
        Arc::new(TextDoc::from_reader(text.as_bytes()).unwrap())
    }

    #[test]
    fn rust_queries_parse() {
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let queries = [
            ("indents", include_str!("../queries/rust/indents.scm")),
            ("brackets", include_str!("../queries/rust/brackets.scm")),
            ("outline", include_str!("../queries/rust/outline.scm")),
            ("injections", include_str!("../queries/rust/injections.scm")),
            ("imports", include_str!("../queries/rust/imports.scm")),
        ];
        for (name, src) in queries {
            Query::new(&lang, src).unwrap_or_else(|e| {
                panic!("rust/{name}.scm failed to parse: {e}");
            });
        }
    }

    #[test]
    fn markdown_queries_parse() {
        let lang: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let src = include_str!("../queries/markdown/injections.scm");
        Query::new(&lang, src).unwrap_or_else(|e| {
            panic!("markdown/injections.scm failed to parse: {e}");
        });
    }

    #[test]
    fn rust_highlights_contain_keyword() {
        let doc = make_doc("fn main() {}\n");
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_doc(path, &*doc).unwrap();
        let highlights = state.highlights_for_lines(&*doc, 0, 1);
        let names: Vec<&str> = highlights
            .iter()
            .map(|(_, s)| s.capture_name.as_str())
            .collect();
        assert!(
            names.iter().any(|n| n.contains("keyword")),
            "expected keyword capture in highlights: {names:?}"
        );
    }

    #[test]
    fn rust_indent_after_open_brace() {
        let doc = make_doc("fn main() {\n\n}\n");
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_doc(path, &*doc).unwrap();
        let suggestion = state.suggest_indent(&*doc, 1).unwrap();
        assert_eq!(suggestion.delta, IndentDelta::Greater);
        assert_eq!(suggestion.basis_row, 0);
    }

    #[test]
    fn rust_indent_closing_brace() {
        let doc = make_doc("fn main() {\n    let x = 1;\n}\n");
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_doc(path, &*doc).unwrap();
        let suggestion = state.suggest_indent(&*doc, 2).unwrap();
        assert_eq!(suggestion.delta, IndentDelta::Less);
    }

    #[test]
    fn rust_bracket_matching() {
        let doc = make_doc("fn f() { (1 + 2) }\n");
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_doc(path, &*doc).unwrap();
        let m = state.matching_bracket(&*doc, 0, 7);
        assert!(m.is_some(), "expected bracket match for '{{' at (0,7)");
        let (r, c) = m.unwrap();
        assert_eq!(r, 0);
        assert_eq!(c, 17);
    }

    #[test]
    fn rust_rainbow_brackets() {
        let doc = make_doc("fn f() { (1 + 2) }\n");
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_doc(path, &*doc).unwrap();
        let pairs = state.bracket_ranges(&*doc, 0..doc.len_bytes());
        for pair in &pairs {
            if pair.color_index.is_some() {
                // Rainbow pairs should have assigned depth
                assert!(pair.color_index.unwrap() <= 10);
            }
        }
    }

    #[test]
    fn rust_outline_items() {
        let doc = make_doc(
            "pub fn hello() {}\nstruct Foo {\n    x: i32,\n}\nimpl Foo {\n    fn bar(&self) {}\n}\n",
        );
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_doc(path, &*doc).unwrap();
        let items = state.outline(&*doc);
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert!(
            names.contains(&"hello"),
            "missing 'hello' in outline: {names:?}"
        );
        assert!(
            names.contains(&"Foo"),
            "missing 'Foo' in outline: {names:?}"
        );
        assert!(
            names.contains(&"bar"),
            "missing 'bar' in outline: {names:?}"
        );
    }

    #[test]
    fn rust_imports_detected() {
        let doc = make_doc("use std::io;\nuse std::path::PathBuf;\n\nfn main() {}\n");
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_doc(path, &*doc).unwrap();
        let imports = state.imports(&*doc);
        assert_eq!(
            imports.len(),
            2,
            "expected 2 imports, got {}",
            imports.len()
        );
    }

    /// Regression: DocChunks::next must terminate when chunk_at_byte returns
    /// a chunk whose end equals the requested offset (zero available bytes).
    /// Without the `take == 0` guard this spins forever.
    #[test]
    fn doc_chunks_terminates_at_chunk_boundary() {
        use crate::parse::DocChunks;

        // A doc whose chunk_at_byte always returns the *previous* chunk,
        // simulating the boundary condition that triggers the bug.
        struct BoundaryDoc;
        impl Doc for BoundaryDoc {
            fn line_count(&self) -> usize {
                1
            }
            fn line(&self, _: usize) -> String {
                "hello".into()
            }
            fn line_to_char(&self, _: usize) -> usize {
                0
            }
            fn char_to_line(&self, _: usize) -> usize {
                0
            }
            fn line_len(&self, _: usize) -> usize {
                5
            }
            fn len_bytes(&self) -> usize {
                5
            }
            fn line_to_byte(&self, _: usize) -> usize {
                0
            }
            fn byte_to_line(&self, _: usize) -> usize {
                0
            }
            fn byte_to_char(&self, i: usize) -> usize {
                i
            }
            fn char_to_byte(&self, i: usize) -> usize {
                i
            }
            fn chunk_at_byte(&self, _offset: usize) -> (&str, usize) {
                // Always return a chunk that *ends* at offset 3, so when
                // byte_offset >= 3 the iterator sees available == 0.
                ("hel", 0)
            }
            fn version(&self) -> u64 {
                0
            }
            fn dirty(&self) -> bool {
                false
            }
            fn content_hash(&self) -> u64 {
                0
            }
            fn undo_history_len(&self) -> usize {
                0
            }
            fn undo_entries_from(&self, _: usize) -> Vec<led_core::UndoEntry> {
                vec![]
            }
            fn undo_cursor(&self) -> Option<usize> {
                None
            }
            fn distance_from_save(&self) -> i32 {
                0
            }
            fn pending_edit_ops(&self) -> Vec<led_core::EditOp> {
                vec![]
            }
            fn insert(&self, _: usize, _: &str) -> Arc<dyn Doc> {
                unimplemented!()
            }
            fn remove(&self, _: usize, _: usize) -> Arc<dyn Doc> {
                unimplemented!()
            }
            fn close_undo_group(&self) -> Arc<dyn Doc> {
                unimplemented!()
            }
            fn undo(&self) -> Option<(Arc<dyn Doc>, usize)> {
                None
            }
            fn redo(&self) -> Option<(Arc<dyn Doc>, usize)> {
                None
            }
            fn apply_remote_entry(&self, _: &led_core::UndoEntry) -> Arc<dyn Doc> {
                unimplemented!()
            }
            fn with_distance_from_save(&self, _: i32) -> Arc<dyn Doc> {
                unimplemented!()
            }
            fn slice(&self, _: usize, _: usize) -> String {
                String::new()
            }
            fn write_to(&self, _: &mut dyn std::io::Write) -> std::io::Result<()> {
                Ok(())
            }
            fn mark_saved(&self) -> Arc<dyn Doc> {
                unimplemented!()
            }
            fn clone_box(&self) -> Box<dyn Doc> {
                unimplemented!()
            }
        }

        let doc: &dyn Doc = &BoundaryDoc;
        // Request bytes 0..5. The chunk covers 0..3, so after consuming
        // bytes 0..3 the iterator hits the boundary (available == 0).
        let mut chunks = DocChunks::new(doc, 0, 5);

        let first = chunks.next();
        assert!(first.is_some(), "first chunk should yield bytes");
        assert_eq!(first.unwrap(), b"hel");
        // Second call: byte_offset == 3, chunk still returns ("hel", 0),
        // available == 0.  Without the fix this would loop forever.
        let second = chunks.next();
        assert!(second.is_none(), "must terminate instead of spinning");
    }

    #[test]
    fn auto_indent_after_brace() {
        let doc = make_doc("fn main() {\n\n}\n");
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_doc(path, &*doc).unwrap();
        let indent_text = state.compute_auto_indent(&*doc, 1);
        assert!(indent_text.is_some());
        let indent_text = indent_text.unwrap();
        assert!(
            !indent_text.is_empty(),
            "indent should not be empty after '{{': {indent_text:?}"
        );
    }

    /// Helper: assert indent for a specific line in a Rust file.
    fn assert_indent(source: &str, line: usize, expected: &str) {
        let doc = make_doc(source);
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_doc(path, &*doc).unwrap();
        let indent = state.compute_auto_indent(&*doc, line);
        let actual = indent.as_deref().unwrap_or("");
        assert_eq!(
            actual,
            expected,
            "line {line}: expected {:?} ({} spaces), got {:?} ({} spaces)\n\
             line content: {:?}",
            expected,
            expected.len(),
            actual,
            actual.len(),
            doc.line(line),
        );
    }

    #[test]
    fn indent_method_chain_continuation() {
        // .bar() should be indented one level deeper than foo
        let src = "\
fn main() {
    foo
    .bar()
    .baz();
}
";
        assert_indent(src, 2, "        "); // .bar() → 8 spaces
        assert_indent(src, 3, "        "); // .baz() → 8 spaces
    }

    #[test]
    fn indent_after_multiline_let_ends() {
        // After a multi-line let ends with ;, the next statement
        // returns to the let's indent level, not the continuation's.
        let src = "\
fn main() {
    let x = foo
        .bar();
    let y = 1;
}
";
        assert_indent(src, 2, "        "); // .bar() → 8 (continuation)
        assert_indent(src, 3, "    "); // let y → 4 (back to block level)
    }

    #[test]
    fn indent_closing_brace() {
        let src = "\
fn main() {
    let x = 1;
}
";
        assert_indent(src, 2, ""); // } → 0 spaces (matches fn)
    }

    #[test]
    fn indent_nested_blocks() {
        let src = "\
fn main() {
    if true {
        let x = 1;
    }
}
";
        assert_indent(src, 2, "        "); // let x → 8
        assert_indent(src, 3, "    "); // } → 4 (matches if)
        assert_indent(src, 4, ""); // } → 0 (matches fn)
    }

    #[test]
    fn indent_match_arm_body() {
        let src = "\
fn main() {
    match x {
        Ok(v) => {
            let y = v;
        }
        Err(_) => {}
    }
}
";
        assert_indent(src, 3, "            "); // let y → 12
        assert_indent(src, 4, "        "); // } → 8 (matches Ok =>)
    }

    #[test]
    fn indent_chained_filter_map() {
        // Real-world pattern: chained combinators inside a function body
        let src = "\
fn run() {
    stream
        .filter(|x| x > 0)
        .map(|x| x + 1)
        .collect();
}
";
        assert_indent(src, 2, "        "); // .filter → 8
        assert_indent(src, 3, "        "); // .map → 8
        assert_indent(src, 4, "        "); // .collect → 8
    }

    #[test]
    fn indent_closure_match_body() {
        // match body inside a closure on a method chain — the line after
        // `match result {` must be indented relative to the match, not
        // unwrapped back to the chain start.
        let src = "\
fn run() {
    stream
        .filter_map(|x| match x {
            Ok(v) => Some(v),
            Err(_) => None,
        });
}
";
        assert_indent(src, 3, "            "); // Ok(v) → 12
        assert_indent(src, 4, "            "); // Err(_) → 12
        assert_indent(src, 5, "        "); // }) → 8
    }

    // ── JavaScript / TypeScript ──

    #[test]
    fn javascript_queries_parse() {
        let lang: tree_sitter::Language = tree_sitter_javascript::LANGUAGE.into();
        let queries = [
            ("indents", include_str!("../queries/javascript/indents.scm")),
            (
                "brackets",
                include_str!("../queries/javascript/brackets.scm"),
            ),
        ];
        for (name, src) in queries {
            Query::new(&lang, src).unwrap_or_else(|e| {
                panic!("javascript/{name}.scm failed to parse: {e}");
            });
        }
    }

    #[test]
    fn typescript_queries_parse() {
        let lang: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        let queries = [
            ("indents", include_str!("../queries/javascript/indents.scm")),
            (
                "brackets",
                include_str!("../queries/javascript/brackets.scm"),
            ),
        ];
        for (name, src) in queries {
            Query::new(&lang, src).unwrap_or_else(|e| {
                panic!("typescript/{name}.scm failed to parse: {e}");
            });
        }
    }

    #[test]
    fn tsx_queries_parse() {
        let lang: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TSX.into();
        let queries = [
            ("indents", include_str!("../queries/javascript/indents.scm")),
            (
                "brackets",
                include_str!("../queries/javascript/brackets.scm"),
            ),
        ];
        for (name, src) in queries {
            Query::new(&lang, src).unwrap_or_else(|e| {
                panic!("tsx/{name}.scm failed to parse: {e}");
            });
        }
    }

    #[test]
    fn python_queries_parse() {
        let lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
        for (name, src) in [
            ("indents", include_str!("../queries/python/indents.scm")),
            ("brackets", include_str!("../queries/python/brackets.scm")),
        ] {
            Query::new(&lang, src)
                .unwrap_or_else(|e| panic!("python/{name}.scm failed to parse: {e}"));
        }
    }

    #[test]
    fn c_queries_parse() {
        let lang: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
        for (name, src) in [
            ("indents", include_str!("../queries/c/indents.scm")),
            ("brackets", include_str!("../queries/c/brackets.scm")),
        ] {
            Query::new(&lang, src).unwrap_or_else(|e| panic!("c/{name}.scm failed to parse: {e}"));
        }
    }

    #[test]
    fn cpp_queries_parse() {
        let lang: tree_sitter::Language = tree_sitter_cpp::LANGUAGE.into();
        for (name, src) in [
            ("indents", include_str!("../queries/c/indents.scm")),
            ("brackets", include_str!("../queries/c/brackets.scm")),
        ] {
            Query::new(&lang, src)
                .unwrap_or_else(|e| panic!("cpp/{name}.scm failed to parse: {e}"));
        }
    }

    #[test]
    fn swift_queries_parse() {
        let lang: tree_sitter::Language = tree_sitter_swift::LANGUAGE.into();
        for (name, src) in [
            ("indents", include_str!("../queries/swift/indents.scm")),
            ("brackets", include_str!("../queries/swift/brackets.scm")),
        ] {
            Query::new(&lang, src)
                .unwrap_or_else(|e| panic!("swift/{name}.scm failed to parse: {e}"));
        }
    }

    #[test]
    fn bash_queries_parse() {
        let lang: tree_sitter::Language = tree_sitter_bash::LANGUAGE.into();
        for (name, src) in [
            ("indents", include_str!("../queries/bash/indents.scm")),
            ("brackets", include_str!("../queries/bash/brackets.scm")),
        ] {
            Query::new(&lang, src)
                .unwrap_or_else(|e| panic!("bash/{name}.scm failed to parse: {e}"));
        }
    }

    #[test]
    fn json_queries_parse() {
        let lang: tree_sitter::Language = tree_sitter_json::LANGUAGE.into();
        for (name, src) in [
            ("indents", include_str!("../queries/json/indents.scm")),
            ("brackets", include_str!("../queries/json/brackets.scm")),
        ] {
            Query::new(&lang, src)
                .unwrap_or_else(|e| panic!("json/{name}.scm failed to parse: {e}"));
        }
    }

    #[test]
    fn toml_queries_parse() {
        let lang: tree_sitter::Language = tree_sitter_toml_ng::LANGUAGE.into();
        for (name, src) in [
            ("indents", include_str!("../queries/toml/indents.scm")),
            ("brackets", include_str!("../queries/toml/brackets.scm")),
        ] {
            Query::new(&lang, src)
                .unwrap_or_else(|e| panic!("toml/{name}.scm failed to parse: {e}"));
        }
    }

    fn assert_ts_indent(source: &str, line: usize, expected: &str) {
        let doc = make_doc(source);
        let path = Path::new("test.ts");
        let state = SyntaxState::from_path_and_doc(path, &*doc).unwrap();
        let indent = state.compute_auto_indent(&*doc, line);
        let actual = indent.as_deref().unwrap_or("");
        assert_eq!(
            actual,
            expected,
            "line {line}: expected {:?} ({} spaces), got {:?} ({} spaces)\n\
             line content: {:?}",
            expected,
            expected.len(),
            actual,
            actual.len(),
            doc.line(line),
        );
    }

    #[test]
    fn ts_indent_after_open_brace() {
        let src = "function main() {\n  const x = 1;\n\n}\n";
        assert_ts_indent(src, 2, "  "); // blank line after indented content
    }

    #[test]
    fn ts_indent_closing_brace() {
        let src = "function main() {\n  const x = 1;\n}\n";
        assert_ts_indent(src, 2, "");
    }

    #[test]
    fn ts_indent_object_in_array() {
        let src = "\
const items = [
  {
    id: 1,
    name: 'hello',
  },
  {
    id: 2,
    name: 'world',
  },
];
";
        assert_ts_indent(src, 2, "    "); // id: 1 → 4 spaces
        assert_ts_indent(src, 4, "  "); // }, → 2 (matches {)
        assert_ts_indent(src, 6, "    "); // id: 2 → 4 spaces
    }

    #[test]
    fn ts_highlights_consistent_across_objects() {
        // Regression: highlighting should not break partway through an array of objects
        let src = "\
const products = [
  {
    id: ProductId.Enterprise,
    name: 'Enterprise',
    active: true,
    type: 'service',
    lookback_limits: null,
  },
  {
    id: ProductId.PackageBasic,
    name: 'Package Basic',
    active: true,
    type: 'good',
    lookback_limits: null,
  },
  {
    id: ProductId.PackageAdvanced,
    name: 'Package Advanced',
    active: true,
    type: 'good',
    lookback_limits: null,
  },
];
";
        let doc = make_doc(src);
        let path = Path::new("test.ts");
        let state = SyntaxState::from_path_and_doc(path, &*doc).unwrap();
        let highlights = state.highlights_for_lines(&*doc, 0, doc.line_count());

        // Check that each object's block has highlights
        let first_obj_hl: Vec<_> = highlights
            .iter()
            .filter(|(l, _)| *l >= 2 && *l <= 7)
            .collect();
        let second_obj_hl: Vec<_> = highlights
            .iter()
            .filter(|(l, _)| *l >= 9 && *l <= 14)
            .collect();
        let third_obj_hl: Vec<_> = highlights
            .iter()
            .filter(|(l, _)| *l >= 16 && *l <= 21)
            .collect();

        assert!(
            !first_obj_hl.is_empty(),
            "first object should have highlights"
        );
        assert!(
            !second_obj_hl.is_empty(),
            "second object should have highlights"
        );
        assert!(
            !third_obj_hl.is_empty(),
            "third object should have highlights, got none"
        );

        // The number of highlights per object should be roughly similar
        let ratio = third_obj_hl.len() as f64 / first_obj_hl.len() as f64;
        assert!(
            ratio > 0.5,
            "third object has significantly fewer highlights ({}) than first ({})",
            third_obj_hl.len(),
            first_obj_hl.len()
        );
    }

    #[test]
    fn ts_indent_nested_block() {
        let src = "\
if (true) {
  if (false) {
    console.log('hi');
  }
}
";
        assert_ts_indent(src, 2, "    "); // console.log → 4
        assert_ts_indent(src, 3, "  "); // } → 2 (matches inner if)
        assert_ts_indent(src, 4, ""); // } → 0 (matches outer if)
    }
}
