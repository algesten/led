use std::borrow::Cow;
use std::ops::Range;
use std::path::Path;

use ropey::Rope;
use tree_sitter::{
    InputEdit, Language, Parser, Point, Query, QueryCursor, StreamingIterator, Tree,
};

use crate::editing::get_line_indent;

// ---------------------------------------------------------------------------
// Highlight span returned to the renderer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct HighlightSpan {
    pub char_start: usize,
    pub char_end: usize,
    pub capture_name: String,
}

// ---------------------------------------------------------------------------
// Language registry
// ---------------------------------------------------------------------------

struct LangEntry {
    language: Language,
    highlights_query: Cow<'static, str>,
    indents_query: Option<&'static str>,
    brackets_query: Option<&'static str>,
    outline_query: Option<&'static str>,
    injections_query: Option<&'static str>,
    imports_query: Option<&'static str>,
    increase_indent_pattern: Option<&'static str>,
    decrease_indent_pattern: Option<&'static str>,
}

impl LangEntry {
    fn new(language: Language, highlights_query: &'static str) -> Self {
        Self {
            language,
            highlights_query: Cow::Borrowed(highlights_query),
            indents_query: None,
            brackets_query: None,
            outline_query: None,
            injections_query: None,
            imports_query: None,
            increase_indent_pattern: None,
            decrease_indent_pattern: None,
        }
    }
}

fn lang_for_ext(ext: &str) -> Option<LangEntry> {
    match ext {
        "rs" => Some(LangEntry {
            indents_query: Some(include_str!("../queries/rust/indents.scm")),
            brackets_query: Some(include_str!("../queries/rust/brackets.scm")),
            outline_query: Some(include_str!("../queries/rust/outline.scm")),
            injections_query: Some(include_str!("../queries/rust/injections.scm")),
            imports_query: Some(include_str!("../queries/rust/imports.scm")),
            increase_indent_pattern: Some(r"\{[^}]*$"),
            decrease_indent_pattern: Some(r"^\s*\}"),
            ..LangEntry::new(
                tree_sitter_rust::LANGUAGE.into(),
                tree_sitter_rust::HIGHLIGHTS_QUERY,
            )
        }),
        "toml" => Some(LangEntry::new(
            tree_sitter_toml_ng::LANGUAGE.into(),
            tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
        )),
        "json" => Some(LangEntry::new(
            tree_sitter_json::LANGUAGE.into(),
            tree_sitter_json::HIGHLIGHTS_QUERY,
        )),
        "js" | "jsx" | "mjs" => Some(LangEntry::new(
            tree_sitter_javascript::LANGUAGE.into(),
            tree_sitter_javascript::HIGHLIGHT_QUERY,
        )),
        "ts" | "tsx" => {
            let lang = if ext == "tsx" {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            } else {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            };
            // The TS highlights query is a supplement to JS; combine them.
            let combined = format!(
                "{}\n{}",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            );
            Some(LangEntry {
                highlights_query: Cow::Owned(combined),
                ..LangEntry::new(lang, "")
            })
        }
        "md" | "markdown" => Some(LangEntry {
            injections_query: Some(include_str!("../queries/markdown/injections.scm")),
            ..LangEntry::new(
                tree_sitter_md::LANGUAGE.into(),
                tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
            )
        }),
        "py" => Some(LangEntry::new(
            tree_sitter_python::LANGUAGE.into(),
            tree_sitter_python::HIGHLIGHTS_QUERY,
        )),
        "sh" | "bash" => Some(LangEntry::new(
            tree_sitter_bash::LANGUAGE.into(),
            tree_sitter_bash::HIGHLIGHT_QUERY,
        )),
        "swift" => Some(LangEntry::new(
            tree_sitter_swift::LANGUAGE.into(),
            tree_sitter_swift::HIGHLIGHTS_QUERY,
        )),
        "c" | "h" => Some(LangEntry::new(
            tree_sitter_c::LANGUAGE.into(),
            tree_sitter_c::HIGHLIGHT_QUERY,
        )),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some(LangEntry::new(
            tree_sitter_cpp::LANGUAGE.into(),
            tree_sitter_cpp::HIGHLIGHT_QUERY,
        )),
        "mk" => Some(LangEntry::new(
            tree_sitter_make::LANGUAGE.into(),
            tree_sitter_make::HIGHLIGHTS_QUERY,
        )),
        _ => None,
    }
}

fn lang_for_filename(name: &str) -> Option<LangEntry> {
    match name {
        "Makefile" | "makefile" | "GNUmakefile" => Some(LangEntry::new(
            tree_sitter_make::LANGUAGE.into(),
            tree_sitter_make::HIGHLIGHTS_QUERY,
        )),
        _ => None,
    }
}

/// Look up a language by name (used for injections).
pub(crate) fn lang_for_name(name: &str) -> Option<(Language, Cow<'static, str>)> {
    let entry = match name {
        "rust" => lang_for_ext("rs"),
        "python" => lang_for_ext("py"),
        "javascript" | "js" => lang_for_ext("js"),
        "typescript" | "ts" => lang_for_ext("ts"),
        "tsx" => lang_for_ext("tsx"),
        "json" => lang_for_ext("json"),
        "toml" => lang_for_ext("toml"),
        "markdown" | "md" => lang_for_ext("md"),
        "bash" | "sh" => lang_for_ext("sh"),
        "c" => lang_for_ext("c"),
        "cpp" | "c++" => lang_for_ext("cpp"),
        "swift" => lang_for_ext("swift"),
        "make" => lang_for_ext("mk"),
        // "comment" and others → no grammar available
        _ => None,
    };
    entry.map(|e| (e.language, e.highlights_query))
}

// ---------------------------------------------------------------------------
// Rope-based text provider for query predicate evaluation
// ---------------------------------------------------------------------------

struct RopeProvider<'a> {
    rope: &'a Rope,
}

impl<'a> tree_sitter::TextProvider<&'a [u8]> for RopeProvider<'a> {
    type I = RopeChunks<'a>;
    fn text(&mut self, node: tree_sitter::Node) -> Self::I {
        let start = node.start_byte();
        let end = node.end_byte();
        RopeChunks {
            rope: self.rope,
            byte_offset: start,
            end,
        }
    }
}

struct RopeChunks<'a> {
    rope: &'a Rope,
    byte_offset: usize,
    end: usize,
}

impl<'a> Iterator for RopeChunks<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<Self::Item> {
        if self.byte_offset >= self.end {
            return None;
        }
        let (chunk, chunk_byte_start, _, _) = self.rope.chunk_at_byte(self.byte_offset);
        let start_within = self.byte_offset - chunk_byte_start;
        let available = chunk.len() - start_within;
        let needed = self.end - self.byte_offset;
        let take = available.min(needed);
        let slice = &chunk.as_bytes()[start_within..start_within + take];
        self.byte_offset += take;
        Some(slice)
    }
}

// ---------------------------------------------------------------------------
// Config structs for each query type
// ---------------------------------------------------------------------------

pub(crate) struct IndentsConfig {
    query: Query,
    indent_capture_ix: u32,
    start_capture_ix: Option<u32>,
    end_capture_ix: Option<u32>,
    outdent_capture_ix: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndentDelta {
    Greater,
    Less,
    Equal,
}

pub(crate) struct IndentSuggestion {
    pub basis_row: usize,
    pub delta: IndentDelta,
    pub within_error: bool,
}

struct BracketsPatternConfig {
    rainbow_exclude: bool,
}

pub(crate) struct BracketsConfig {
    query: Query,
    open_capture_ix: u32,
    close_capture_ix: u32,
    patterns: Vec<BracketsPatternConfig>,
}

#[derive(Debug, Clone)]
pub(crate) struct BracketMatch {
    pub open_range: Range<usize>,
    pub close_range: Range<usize>,
    pub color_index: Option<usize>,
}

pub(crate) struct OutlineConfig {
    query: Query,
    item_capture_ix: u32,
    name_capture_ix: u32,
    context_capture_ix: Option<u32>,
}

#[derive(Debug, Clone)]
pub(crate) struct OutlineItem {
    pub depth: usize,
    pub name: String,
    pub context: String,
    pub row: usize,
}

pub(crate) struct InjectionConfig {
    query: Query,
    content_capture_ix: u32,
    language_capture_ix: Option<u32>,
    patterns: Vec<InjectionPatternConfig>,
}

struct InjectionPatternConfig {
    language: Option<String>,
    combined: bool,
}

pub(crate) struct ImportsConfig {
    query: Query,
    import_capture_ix: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct ImportItem {
    pub full_text: String,
    pub start_row: usize,
    pub end_row: usize,
    pub start_byte: usize,
    pub end_byte: usize,
}

// ---------------------------------------------------------------------------
// Injection layers
// ---------------------------------------------------------------------------

pub(crate) struct InjectionLayer {
    tree: Tree,
    highlights_query: Query,
    included_ranges: Vec<tree_sitter::Range>,
}

// ---------------------------------------------------------------------------
// SyntaxState
// ---------------------------------------------------------------------------

pub(crate) struct SyntaxState {
    parser: Parser,
    tree: Tree,
    highlights_query: Query,
    pub(crate) indents_config: Option<IndentsConfig>,
    pub(crate) brackets_config: Option<BracketsConfig>,
    pub(crate) outline_config: Option<OutlineConfig>,
    injections_config: Option<InjectionConfig>,
    pub(crate) imports_config: Option<ImportsConfig>,
    error_query: Option<Query>,
    injection_layers: Vec<InjectionLayer>,
    pub(crate) increase_indent_pattern: Option<regex::Regex>,
    pub(crate) decrease_indent_pattern: Option<regex::Regex>,
}

impl SyntaxState {
    pub(crate) fn from_path_and_rope(path: &Path, rope: &Rope) -> Option<Self> {
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
        let tree = parse_rope(&mut parser, rope, None)?;

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

        // Build initial injection layers
        let mut injection_layers = Vec::new();
        if let Some(ref inj_config) = injections_config {
            injection_layers = build_injection_layers(inj_config, &tree, rope);
        }

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
            increase_indent_pattern,
            decrease_indent_pattern,
        })
    }

    pub(crate) fn apply_edit(&mut self, edit: &InputEdit, rope: &Rope) {
        self.tree.edit(edit);
        if let Some(new_tree) = parse_rope(&mut self.parser, rope, Some(&self.tree)) {
            self.tree = new_tree;
        }

        // Rebuild injection layers from the updated tree
        if let Some(ref inj_config) = self.injections_config {
            self.injection_layers = build_injection_layers(inj_config, &self.tree, rope);
        }
    }

    /// Clone the current tree (cheap — refcounted) for two-pass indent comparison.
    pub(crate) fn clone_tree(&self) -> Tree {
        self.tree.clone()
    }

    // -----------------------------------------------------------------------
    // Highlights
    // -----------------------------------------------------------------------

    pub(crate) fn highlights_for_lines(
        &self,
        rope: &Rope,
        start_line: usize,
        end_line: usize,
    ) -> Vec<(usize, HighlightSpan)> {
        let total_lines = rope.len_lines();
        if start_line >= total_lines {
            return Vec::new();
        }
        let end_line = end_line.min(total_lines);
        let start_byte = rope.line_to_byte(start_line);
        let end_byte = if end_line >= total_lines {
            rope.len_bytes()
        } else {
            rope.line_to_byte(end_line)
        };

        let mut result = collect_highlights(
            &self.highlights_query,
            &self.tree,
            rope,
            start_byte,
            end_byte,
            start_line,
            end_line,
        );

        // Merge injection layer highlights (override parent)
        for layer in &self.injection_layers {
            // Check if any included ranges overlap the visible byte range
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
                rope,
                start_byte,
                end_byte,
                start_line,
                end_line,
            );
            result.extend(injection_hl);
        }

        result
    }

    // -----------------------------------------------------------------------
    // Indentation
    // -----------------------------------------------------------------------

    /// For a line whose first non-whitespace char is a closing bracket,
    /// find the matching open bracket via tree-sitter and return its line's indent.
    pub(crate) fn closing_bracket_indent(&self, rope: &Rope, line: usize) -> Option<String> {
        let line_text = rope.line(line);
        let mut char_offset = 0;
        for ch in line_text.chars() {
            if ch == ' ' || ch == '\t' {
                char_offset += 1;
                continue;
            }
            if ch != '}' && ch != ')' && ch != ']' {
                return None;
            }
            break;
        }
        let bracket_char_idx = rope.line_to_char(line) + char_offset;
        let bracket_byte = rope.char_to_byte(bracket_char_idx);
        let node = self
            .tree
            .root_node()
            .descendant_for_byte_range(bracket_byte, bracket_byte + 1)?;
        let parent = node.parent()?;
        Some(get_line_indent(rope, parent.start_position().row))
    }

    pub(crate) fn suggest_indent(&self, rope: &Rope, line: usize) -> Option<IndentSuggestion> {
        let config = self.indents_config.as_ref()?;

        // Find basis row (previous non-blank line)
        let basis_row = find_basis_row(rope, line)?;

        // Collect indent ranges from query
        let total_bytes = rope.len_bytes();
        let query_start = indent_query_start(rope, &self.tree, basis_row, line);
        let query_end = if line + 1 < rope.len_lines() {
            rope.line_to_byte(line + 1)
        } else {
            total_bytes
        };

        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(query_start..query_end);

        let mut indent_ranges: Vec<Range<usize>> = Vec::new();

        let mut matches =
            cursor.matches(&config.query, self.tree.root_node(), RopeProvider { rope });
        while let Some(m) = {
            matches.advance();
            matches.get()
        } {
            let mut node_range: Option<Range<usize>> = None;
            let mut outdent_pos: Option<usize> = None;

            for cap in m.captures {
                if cap.index == config.indent_capture_ix {
                    node_range = Some(cap.node.start_byte()..cap.node.end_byte());
                } else if Some(cap.index) == config.start_capture_ix {
                    // Only the end position of @start marks indent begin
                    let end_pos = cap.node.end_byte();
                    if let Some(ref mut nr) = node_range {
                        nr.start = end_pos;
                    } else {
                        node_range = Some(end_pos..query_end);
                    }
                } else if Some(cap.index) == config.end_capture_ix {
                    // Only the start position of @end marks indent end
                    let start_pos = cap.node.start_byte();
                    if let Some(ref mut nr) = node_range {
                        nr.end = start_pos;
                    }
                } else if Some(cap.index) == config.outdent_capture_ix {
                    outdent_pos = Some(cap.node.start_byte());
                }
            }

            if let Some(range) = node_range {
                if range.start < range.end {
                    indent_ranges.push(range);
                }
            }

            // Apply outdent: truncate containing range
            if let Some(pos) = outdent_pos {
                for r in &mut indent_ranges {
                    if r.start <= pos && pos < r.end {
                        r.end = pos;
                    }
                }
            }
        }

        // Check if line is within an error node
        let within_error = self.is_within_error(rope, line);

        // Determine delta
        let line_start = rope.line_to_byte(line);

        // Check if an indent range opens on the basis row and extends past the target line
        let basis_opens_indent = indent_ranges
            .iter()
            .any(|r| rope.byte_to_line(r.start) == basis_row && r.end > line_start);

        // Check if an indent range closes on the target line
        let line_at_outdent = indent_ranges
            .iter()
            .any(|r| r.end > 0 && rope.byte_to_line(r.end) == line && r.start < line_start);

        let delta = if line_at_outdent {
            IndentDelta::Less
        } else if basis_opens_indent {
            IndentDelta::Greater
        } else {
            IndentDelta::Equal
        };

        Some(IndentSuggestion {
            basis_row,
            delta,
            within_error,
        })
    }

    /// Suggest indent using a specific tree (for two-pass comparison).
    pub(crate) fn suggest_indent_with_tree(
        &self,
        rope: &Rope,
        tree: &Tree,
        line: usize,
    ) -> Option<IndentSuggestion> {
        let config = self.indents_config.as_ref()?;
        let basis_row = find_basis_row(rope, line)?;

        let total_bytes = rope.len_bytes();
        let query_start = indent_query_start(rope, tree, basis_row, line);
        let query_end = if line + 1 < rope.len_lines() {
            rope.line_to_byte(line + 1)
        } else {
            total_bytes
        };

        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(query_start..query_end);

        let mut indent_ranges: Vec<Range<usize>> = Vec::new();

        let mut matches = cursor.matches(&config.query, tree.root_node(), RopeProvider { rope });
        while let Some(m) = {
            matches.advance();
            matches.get()
        } {
            let mut node_range: Option<Range<usize>> = None;
            let mut outdent_pos: Option<usize> = None;

            for cap in m.captures {
                if cap.index == config.indent_capture_ix {
                    node_range = Some(cap.node.start_byte()..cap.node.end_byte());
                } else if Some(cap.index) == config.start_capture_ix {
                    let end_pos = cap.node.end_byte();
                    if let Some(ref mut nr) = node_range {
                        nr.start = end_pos;
                    } else {
                        node_range = Some(end_pos..query_end);
                    }
                } else if Some(cap.index) == config.end_capture_ix {
                    let start_pos = cap.node.start_byte();
                    if let Some(ref mut nr) = node_range {
                        nr.end = start_pos;
                    }
                } else if Some(cap.index) == config.outdent_capture_ix {
                    outdent_pos = Some(cap.node.start_byte());
                }
            }

            if let Some(range) = node_range {
                if range.start < range.end {
                    indent_ranges.push(range);
                }
            }

            if let Some(pos) = outdent_pos {
                for r in &mut indent_ranges {
                    if r.start <= pos && pos < r.end {
                        r.end = pos;
                    }
                }
            }
        }

        // Check error using the provided tree
        let within_error = if let Some(ref eq) = self.error_query {
            let line_start = rope.line_to_byte(line);
            let line_end = if line + 1 < rope.len_lines() {
                rope.line_to_byte(line + 1)
            } else {
                rope.len_bytes()
            };
            let mut c = QueryCursor::new();
            c.set_byte_range(line_start..line_end);
            let mut m = c.matches(eq, tree.root_node(), RopeProvider { rope });
            let found = {
                m.advance();
                m.get().is_some()
            };
            found
        } else {
            false
        };

        let line_start = rope.line_to_byte(line);

        let basis_opens_indent = indent_ranges
            .iter()
            .any(|r| rope.byte_to_line(r.start) == basis_row && r.end > line_start);

        let line_at_outdent = indent_ranges
            .iter()
            .any(|r| r.end > 0 && rope.byte_to_line(r.end) == line && r.start < line_start);

        let delta = if line_at_outdent {
            IndentDelta::Less
        } else if basis_opens_indent {
            IndentDelta::Greater
        } else {
            IndentDelta::Equal
        };

        Some(IndentSuggestion {
            basis_row,
            delta,
            within_error,
        })
    }

    fn is_within_error(&self, rope: &Rope, line: usize) -> bool {
        let Some(ref eq) = self.error_query else {
            return false;
        };
        let line_start = rope.line_to_byte(line);
        let line_end = if line + 1 < rope.len_lines() {
            rope.line_to_byte(line + 1)
        } else {
            rope.len_bytes()
        };
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(line_start..line_end);
        let mut matches = cursor.matches(eq, self.tree.root_node(), RopeProvider { rope });
        matches.advance();
        matches.get().is_some()
    }

    // -----------------------------------------------------------------------
    // Brackets
    // -----------------------------------------------------------------------

    /// All bracket pairs overlapping a byte range.
    pub(crate) fn bracket_ranges(&self, rope: &Rope, range: Range<usize>) -> Vec<BracketMatch> {
        let config = match &self.brackets_config {
            Some(c) => c,
            None => return Vec::new(),
        };

        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(range.clone());

        let mut pairs = Vec::new();
        let mut matches =
            cursor.matches(&config.query, self.tree.root_node(), RopeProvider { rope });
        while let Some(m) = {
            matches.advance();
            matches.get()
        } {
            let mut open_range: Option<Range<usize>> = None;
            let mut close_range: Option<Range<usize>> = None;

            for cap in m.captures {
                if cap.index == config.open_capture_ix {
                    open_range = Some(cap.node.start_byte()..cap.node.end_byte());
                } else if cap.index == config.close_capture_ix {
                    close_range = Some(cap.node.start_byte()..cap.node.end_byte());
                }
            }

            if let (Some(or), Some(cr)) = (open_range, close_range) {
                let pattern_config = config.patterns.get(m.pattern_index);
                let rainbow_exclude = pattern_config.map_or(false, |p| p.rainbow_exclude);
                pairs.push(BracketMatch {
                    open_range: or,
                    close_range: cr,
                    color_index: if rainbow_exclude { None } else { Some(0) },
                });
            }
        }

        // Assign rainbow depth
        assign_rainbow_depth(&mut pairs);

        pairs
    }

    /// Find the matching bracket for the one at cursor position.
    pub(crate) fn matching_bracket(
        &self,
        rope: &Rope,
        row: usize,
        col: usize,
    ) -> Option<(usize, usize)> {
        let config = self.brackets_config.as_ref()?;
        let char_idx = rope.line_to_char(row) + col;
        let byte_pos = rope.char_to_byte(char_idx);

        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(0..rope.len_bytes());

        let mut matches =
            cursor.matches(&config.query, self.tree.root_node(), RopeProvider { rope });
        while let Some(m) = {
            matches.advance();
            matches.get()
        } {
            let mut open_range: Option<Range<usize>> = None;
            let mut close_range: Option<Range<usize>> = None;

            for cap in m.captures {
                if cap.index == config.open_capture_ix {
                    open_range = Some(cap.node.start_byte()..cap.node.end_byte());
                } else if cap.index == config.close_capture_ix {
                    close_range = Some(cap.node.start_byte()..cap.node.end_byte());
                }
            }

            if let (Some(or), Some(cr)) = (open_range, close_range) {
                // Check if cursor is on the open bracket
                if or.start <= byte_pos && byte_pos < or.end {
                    let target_byte = cr.start;
                    let target_line = rope.byte_to_line(target_byte);
                    let target_char = rope.byte_to_char(target_byte);
                    let line_char = rope.line_to_char(target_line);
                    return Some((target_line, target_char - line_char));
                }
                // Check if cursor is on the close bracket
                if cr.start <= byte_pos && byte_pos < cr.end {
                    let target_byte = or.start;
                    let target_line = rope.byte_to_line(target_byte);
                    let target_char = rope.byte_to_char(target_byte);
                    let line_char = rope.line_to_char(target_line);
                    return Some((target_line, target_char - line_char));
                }
            }
        }

        None
    }

    // -----------------------------------------------------------------------
    // Outline
    // -----------------------------------------------------------------------

    pub(crate) fn outline(&self, rope: &Rope) -> Vec<OutlineItem> {
        let config = match &self.outline_config {
            Some(c) => c,
            None => return Vec::new(),
        };

        let mut cursor = QueryCursor::new();
        let mut raw_items: Vec<(usize, usize, OutlineItem)> = Vec::new(); // (start_byte, end_byte, item)

        let mut matches =
            cursor.matches(&config.query, self.tree.root_node(), RopeProvider { rope });
        while let Some(m) = {
            matches.advance();
            matches.get()
        } {
            let mut item_node: Option<tree_sitter::Node> = None;
            let mut name_text = String::new();
            let mut context_parts: Vec<(usize, usize, String)> = Vec::new();

            for cap in m.captures {
                if cap.index == config.item_capture_ix {
                    item_node = Some(cap.node);
                } else if cap.index == config.name_capture_ix {
                    name_text = node_text(rope, &cap.node);
                } else if Some(cap.index) == config.context_capture_ix {
                    context_parts.push((
                        cap.node.start_byte(),
                        cap.node.end_byte(),
                        node_text(rope, &cap.node),
                    ));
                }
            }

            if let Some(node) = item_node {
                if name_text.is_empty() {
                    continue;
                }

                // Build context string
                context_parts.sort_by_key(|(start, _, _)| *start);
                let context = context_parts
                    .iter()
                    .map(|(_, _, t)| t.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");

                raw_items.push((
                    node.start_byte(),
                    node.end_byte(),
                    OutlineItem {
                        depth: 0,
                        name: name_text,
                        context,
                        row: node.start_position().row,
                    },
                ));
            }
        }

        // Compute hierarchical depth
        // Sort by (start_byte, Reverse(end_byte)) so inner items come after outer
        raw_items.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));

        let mut stack: Vec<usize> = Vec::new(); // end_bytes of containing items
        let mut result = Vec::with_capacity(raw_items.len());
        for (start, end, mut item) in raw_items {
            // Pop items from stack whose end < current start
            while let Some(&top_end) = stack.last() {
                if top_end <= start {
                    stack.pop();
                } else {
                    break;
                }
            }
            item.depth = stack.len();
            stack.push(end);
            result.push(item);
        }

        result
    }

    // -----------------------------------------------------------------------
    // Imports
    // -----------------------------------------------------------------------

    pub(crate) fn imports(&self, rope: &Rope) -> Vec<ImportItem> {
        let config = match &self.imports_config {
            Some(c) => c,
            None => return Vec::new(),
        };

        let mut cursor = QueryCursor::new();
        let mut items = Vec::new();

        let mut matches =
            cursor.matches(&config.query, self.tree.root_node(), RopeProvider { rope });
        while let Some(m) = {
            matches.advance();
            matches.get()
        } {
            for cap in m.captures {
                if cap.index == config.import_capture_ix {
                    items.push(ImportItem {
                        full_text: node_text(rope, &cap.node),
                        start_row: cap.node.start_position().row,
                        end_row: cap.node.end_position().row,
                        start_byte: cap.node.start_byte(),
                        end_byte: cap.node.end_byte(),
                    });
                }
            }
        }

        // Deduplicate imports (the query may match the same use_declaration multiple times)
        items.dedup_by(|a, b| a.start_byte == b.start_byte && a.end_byte == b.end_byte);

        items
    }
}

// ---------------------------------------------------------------------------
// Config compilation helpers
// ---------------------------------------------------------------------------

fn compile_indents_config(language: &Language, query_src: &str) -> Option<IndentsConfig> {
    let query = Query::new(language, query_src).ok()?;
    let names = query.capture_names();
    let indent_ix = names.iter().position(|n| *n == "indent")? as u32;
    let start_ix = names.iter().position(|n| *n == "start").map(|i| i as u32);
    let end_ix = names.iter().position(|n| *n == "end").map(|i| i as u32);
    let outdent_ix = names.iter().position(|n| *n == "outdent").map(|i| i as u32);
    Some(IndentsConfig {
        query,
        indent_capture_ix: indent_ix,
        start_capture_ix: start_ix,
        end_capture_ix: end_ix,
        outdent_capture_ix: outdent_ix,
    })
}

fn compile_brackets_config(language: &Language, query_src: &str) -> Option<BracketsConfig> {
    let query = Query::new(language, query_src).ok()?;
    let names = query.capture_names();
    let open_ix = names.iter().position(|n| *n == "open")? as u32;
    let close_ix = names.iter().position(|n| *n == "close")? as u32;

    let mut patterns = Vec::new();
    for i in 0..query.pattern_count() {
        let mut rainbow_exclude = false;
        for prop in query.property_settings(i) {
            match &*prop.key {
                "rainbow.exclude" => rainbow_exclude = true,
                _ => {}
            }
        }
        patterns.push(BracketsPatternConfig { rainbow_exclude });
    }

    Some(BracketsConfig {
        query,
        open_capture_ix: open_ix,
        close_capture_ix: close_ix,
        patterns,
    })
}

fn compile_outline_config(language: &Language, query_src: &str) -> Option<OutlineConfig> {
    let query = Query::new(language, query_src).ok()?;
    let names = query.capture_names();
    let item_ix = names.iter().position(|n| *n == "item")? as u32;
    let name_ix = names.iter().position(|n| *n == "name")? as u32;
    let context_ix = names.iter().position(|n| *n == "context").map(|i| i as u32);

    Some(OutlineConfig {
        query,
        item_capture_ix: item_ix,
        name_capture_ix: name_ix,
        context_capture_ix: context_ix,
    })
}

fn compile_injection_config(language: &Language, query_src: &str) -> Option<InjectionConfig> {
    let query = Query::new(language, query_src).ok()?;
    let names = query.capture_names();
    let content_ix = names.iter().position(|n| *n == "injection.content")? as u32;
    let language_ix = names
        .iter()
        .position(|n| *n == "injection.language")
        .map(|i| i as u32);

    let mut patterns = Vec::new();
    for i in 0..query.pattern_count() {
        let mut lang = None;
        let mut combined = false;
        for prop in query.property_settings(i) {
            match &*prop.key {
                "injection.language" => {
                    lang = prop.value.as_ref().map(|v| v.to_string());
                }
                "injection.combined" => combined = true,
                _ => {}
            }
        }
        patterns.push(InjectionPatternConfig {
            language: lang,
            combined,
        });
    }

    Some(InjectionConfig {
        query,
        content_capture_ix: content_ix,
        language_capture_ix: language_ix,
        patterns,
    })
}

fn compile_imports_config(language: &Language, query_src: &str) -> Option<ImportsConfig> {
    let query = Query::new(language, query_src).ok()?;
    let names = query.capture_names();
    let import_ix = names.iter().position(|n| *n == "import")? as u32;
    Some(ImportsConfig {
        query,
        import_capture_ix: import_ix,
    })
}

// ---------------------------------------------------------------------------
// Injection layer building
// ---------------------------------------------------------------------------

fn build_injection_layers(
    config: &InjectionConfig,
    tree: &Tree,
    rope: &Rope,
) -> Vec<InjectionLayer> {
    let mut cursor = QueryCursor::new();

    // Group: lang_name -> (combined, Vec<Range>)
    let mut single_layers: Vec<(String, tree_sitter::Range)> = Vec::new();
    let mut combined_ranges: std::collections::HashMap<String, Vec<tree_sitter::Range>> =
        std::collections::HashMap::new();

    let mut matches = cursor.matches(&config.query, tree.root_node(), RopeProvider { rope });
    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let pattern_config = config.patterns.get(m.pattern_index);
        let combined = pattern_config.map_or(false, |p| p.combined);

        // Determine language name
        let lang_name = pattern_config
            .and_then(|p| p.language.as_deref())
            .map(|s| s.to_string())
            .or_else(|| {
                config.language_capture_ix.and_then(|ix| {
                    m.captures.iter().find_map(|c| {
                        if c.index == ix {
                            Some(node_text(rope, &c.node))
                        } else {
                            None
                        }
                    })
                })
            });

        let Some(lang_name) = lang_name else {
            continue;
        };

        // Collect content ranges
        for cap in m.captures {
            if cap.index == config.content_capture_ix {
                let range = cap.node.range();
                if combined {
                    combined_ranges
                        .entry(lang_name.clone())
                        .or_default()
                        .push(range);
                } else {
                    single_layers.push((lang_name.clone(), range));
                }
            }
        }
    }

    let mut layers = Vec::new();

    // Build single layers
    for (lang_name, range) in single_layers {
        if let Some(layer) = create_injection_layer(&lang_name, vec![range], rope) {
            layers.push(layer);
        }
    }

    // Build combined layers
    for (lang_name, ranges) in combined_ranges {
        if !ranges.is_empty() {
            if let Some(layer) = create_injection_layer(&lang_name, ranges, rope) {
                layers.push(layer);
            }
        }
    }

    layers
}

fn create_injection_layer(
    lang_name: &str,
    ranges: Vec<tree_sitter::Range>,
    rope: &Rope,
) -> Option<InjectionLayer> {
    let (language, hl_query_src) = lang_for_name(lang_name)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    parser.set_included_ranges(&ranges).ok()?;
    let tree = parse_rope(&mut parser, rope, None)?;
    let highlights_query = Query::new(&language, &hl_query_src).ok()?;

    Some(InjectionLayer {
        tree,
        highlights_query,
        included_ranges: ranges,
    })
}

// ---------------------------------------------------------------------------
// Highlight collection helper
// ---------------------------------------------------------------------------

fn collect_highlights(
    query: &Query,
    tree: &Tree,
    rope: &Rope,
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    end_line: usize,
) -> Vec<(usize, HighlightSpan)> {
    let total_lines = rope.len_lines();
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(start_byte..end_byte);

    let capture_names = query.capture_names();
    let mut result = Vec::new();

    let mut matches = cursor.matches(query, tree.root_node(), RopeProvider { rope });
    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        for cap in m.captures {
            let name = capture_names[cap.index as usize];
            let node = cap.node;
            let node_start_byte = node.start_byte();
            let node_end_byte = node.end_byte();

            let node_start_line = rope.byte_to_line(node_start_byte);
            let node_end_line = rope.byte_to_line(node_end_byte.min(rope.len_bytes().max(1) - 1));

            for line in node_start_line..=node_end_line {
                if line < start_line || line >= end_line {
                    continue;
                }
                let line_start_byte = rope.line_to_byte(line);
                let line_end_byte = if line + 1 < total_lines {
                    rope.line_to_byte(line + 1)
                } else {
                    rope.len_bytes()
                };

                let span_start_byte = node_start_byte.max(line_start_byte);
                let span_end_byte = node_end_byte.min(line_end_byte);
                if span_start_byte >= span_end_byte {
                    continue;
                }

                let line_start_char = rope.byte_to_char(line_start_byte);
                let char_start = rope.byte_to_char(span_start_byte) - line_start_char;
                let char_end = rope.byte_to_char(span_end_byte) - line_start_char;

                let line_char_count = rope.line(line).len_chars();
                let effective_line_len =
                    if line_char_count > 0 && rope.line(line).char(line_char_count - 1) == '\n' {
                        line_char_count - 1
                    } else {
                        line_char_count
                    };
                let char_end = char_end.min(effective_line_len);
                if char_start >= char_end {
                    continue;
                }

                result.push((
                    line,
                    HighlightSpan {
                        char_start,
                        char_end,
                        capture_name: name.to_string(),
                    },
                ));
            }
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

fn node_text(rope: &Rope, node: &tree_sitter::Node) -> String {
    let start = rope.byte_to_char(node.start_byte());
    let end = rope.byte_to_char(node.end_byte().min(rope.len_bytes()));
    rope.slice(start..end).to_string()
}

/// Compute the query start byte for indent queries.
/// Walks up the tree from the target line to find the innermost ancestor
/// that starts before basis_row, ensuring the query captures the opening
/// bracket of any enclosing block.
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
    for row in (0..line).rev() {
        let line_text = rope.line(row);
        if line_text.chars().any(|c| !c.is_whitespace()) {
            return Some(row);
        }
    }
    // If no non-blank line found, use line 0 if it exists and line > 0
    if line > 0 { Some(0) } else { None }
}

fn assign_rainbow_depth(pairs: &mut [BracketMatch]) {
    // Sort by open position for stack-based depth assignment
    let mut indexed: Vec<(usize, usize)> = pairs
        .iter()
        .enumerate()
        .map(|(i, p)| (i, p.open_range.start))
        .collect();
    indexed.sort_by_key(|&(_, start)| start);

    let mut stack: Vec<usize> = Vec::new(); // close positions

    for (idx, _) in indexed {
        let pair = &pairs[idx];
        if pair.color_index.is_none() {
            continue; // rainbow excluded
        }
        let close_pos = pair.close_range.start;

        // Pop brackets that have closed before this one opens
        while let Some(&top) = stack.last() {
            if top <= pair.open_range.start {
                stack.pop();
            } else {
                break;
            }
        }

        pairs[idx].color_index = Some(stack.len());
        stack.push(close_pos);
    }
}

// ---------------------------------------------------------------------------
// InputEdit helpers — call BEFORE mutating the rope
// ---------------------------------------------------------------------------

fn byte_and_point(rope: &Rope, char_idx: usize) -> (usize, Point) {
    let byte = rope.char_to_byte(char_idx);
    let line = rope.char_to_line(char_idx);
    let line_byte = rope.line_to_byte(line);
    (
        byte,
        Point {
            row: line,
            column: byte - line_byte,
        },
    )
}

pub(crate) fn edit_for_insert(rope: &Rope, char_idx: usize, text: &str) -> InputEdit {
    let (start_byte, start_pos) = byte_and_point(rope, char_idx);
    let new_end_byte = start_byte + text.len();
    let mut end_row = start_pos.row;
    let mut end_col = start_pos.column;
    for b in text.bytes() {
        if b == b'\n' {
            end_row += 1;
            end_col = 0;
        } else {
            end_col += 1;
        }
    }
    InputEdit {
        start_byte,
        old_end_byte: start_byte,
        new_end_byte,
        start_position: start_pos,
        old_end_position: start_pos,
        new_end_position: Point {
            row: end_row,
            column: end_col,
        },
    }
}

pub(crate) fn edit_for_remove(rope: &Rope, char_start: usize, char_end: usize) -> InputEdit {
    let (start_byte, start_pos) = byte_and_point(rope, char_start);
    let (end_byte, end_pos) = byte_and_point(rope, char_end);
    InputEdit {
        start_byte,
        old_end_byte: end_byte,
        new_end_byte: start_byte,
        start_position: start_pos,
        old_end_position: end_pos,
        new_end_position: start_pos,
    }
}

// ---------------------------------------------------------------------------
// Parse rope contents
// ---------------------------------------------------------------------------

fn parse_rope(parser: &mut Parser, rope: &Rope, old_tree: Option<&Tree>) -> Option<Tree> {
    #[allow(deprecated)]
    parser.parse_with(
        &mut |byte_offset, _position: Point| -> &[u8] {
            if byte_offset >= rope.len_bytes() {
                return &[];
            }
            let (chunk, chunk_byte_start, _, _) = rope.chunk_at_byte(byte_offset);
            let start_within = byte_offset - chunk_byte_start;
            &chunk.as_bytes()[start_within..]
        },
        old_tree,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify all Tree-sitter query files parse against their grammars.
    #[test]
    fn rust_queries_parse() {
        let lang: Language = tree_sitter_rust::LANGUAGE.into();
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
        let lang: Language = tree_sitter_md::LANGUAGE.into();
        let src = include_str!("../queries/markdown/injections.scm");
        Query::new(&lang, src).unwrap_or_else(|e| {
            panic!("markdown/injections.scm failed to parse: {e}");
        });
    }

    #[test]
    fn rust_indent_after_open_brace() {
        // After `{`, the next line should be indented (Greater)
        let code = "fn main() {\n\n}\n";
        let rope = Rope::from_str(code);
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_rope(path, &rope).unwrap();
        let suggestion = state.suggest_indent(&rope, 1).unwrap();
        assert_eq!(
            suggestion.delta,
            IndentDelta::Greater,
            "line after '{{' should indent"
        );
        assert_eq!(suggestion.basis_row, 0);
    }

    #[test]
    fn rust_indent_inside_block() {
        // A line inside a block (not right after `{`) should be Equal
        let code = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        let rope = Rope::from_str(code);
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_rope(path, &rope).unwrap();
        let suggestion = state.suggest_indent(&rope, 2).unwrap();
        assert_eq!(
            suggestion.delta,
            IndentDelta::Equal,
            "line inside block (not after opener) should be Equal"
        );
    }

    #[test]
    fn rust_indent_closing_brace() {
        // The `}` line should be outdented (Less)
        let code = "fn main() {\n    let x = 1;\n}\n";
        let rope = Rope::from_str(code);
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_rope(path, &rope).unwrap();
        let suggestion = state.suggest_indent(&rope, 2).unwrap();
        assert_eq!(
            suggestion.delta,
            IndentDelta::Less,
            "closing '}}' line should outdent"
        );
    }

    #[test]
    fn rust_indent_nested_blocks() {
        // fn inside trait inside mod — nested blocks must not merge
        let code = "mod private {\n    use super::Config;\n\n    pub trait ConfigScope {\n    fn config(&mut self) -> &mut Config;\n    }\n}\n";
        let rope = Rope::from_str(code);
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_rope(path, &rope).unwrap();

        // line 4: `fn config` — basis is trait opener, should be Greater
        let s = state.suggest_indent(&rope, 4).unwrap();
        assert_eq!(s.basis_row, 3);
        assert_eq!(
            s.delta,
            IndentDelta::Greater,
            "fn inside trait should indent"
        );

        // line 5: `}` closing trait — should be Less
        let s = state.suggest_indent(&rope, 5).unwrap();
        assert_eq!(
            s.delta,
            IndentDelta::Less,
            "closing trait '}}' should outdent"
        );

        // line 3: `pub trait` — same level as use, should be Equal
        let s = state.suggest_indent(&rope, 3).unwrap();
        assert_eq!(
            s.delta,
            IndentDelta::Equal,
            "trait decl should be Equal to use"
        );
    }

    #[test]
    fn rust_outline_items() {
        let code = r#"
pub fn hello() {}
struct Foo {
    x: i32,
}
impl Foo {
    fn bar(&self) {}
}
"#;
        let rope = Rope::from_str(code);
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_rope(path, &rope).unwrap();
        let items = state.outline(&rope);
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
    fn rust_bracket_matching() {
        let code = "fn f() { (1 + 2) }\n";
        let rope = Rope::from_str(code);
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_rope(path, &rope).unwrap();
        // Opening brace at row 0, col 7 should match closing brace
        let m = state.matching_bracket(&rope, 0, 7);
        assert!(m.is_some(), "expected bracket match for '{{' at (0,7)");
        let (r, c) = m.unwrap();
        assert_eq!(r, 0);
        assert_eq!(c, 17, "closing '}}' should be at col 17");
    }

    #[test]
    fn rust_bracket_nested_parens() {
        // Regression: tuple parens inside .map() must be found
        let code = "fn f() { v.map(|rl| (Arc::new(rl.0), true)) }\n";
        let rope = Rope::from_str(code);
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_rope(path, &rope).unwrap();
        let pairs = state.bracket_ranges(&rope, 0..rope.len_bytes());
        // Collect (open_char, open_byte, close_byte, color_index) for debugging
        let info: Vec<(char, usize, usize, Option<usize>)> = pairs
            .iter()
            .map(|bm| {
                let ch = rope.char(rope.byte_to_char(bm.open_range.start));
                (
                    ch,
                    bm.open_range.start,
                    bm.close_range.start,
                    bm.color_index,
                )
            })
            .collect();
        // Count paren pairs
        let paren_count = info.iter().filter(|i| i.0 == '(').count();
        // There should be at least 3 paren pairs: f(), map(...), tuple(...), Arc::new(...)
        assert!(
            paren_count >= 3,
            "expected >=3 paren pairs, got {paren_count}: {info:?}",
        );
        // All non-excluded pairs should have a color_index
        for (ch, open, close, ci) in &info {
            assert!(
                ci.is_some(),
                "pair '{ch}' at {open}..{close} has no color_index"
            );
        }
    }

    #[test]
    fn rust_matching_bracket_tuple() {
        // Regression: matching_bracket must work for tuple parens inside .map()
        let code = "fn f() { v.map(|rl| (Arc::new(rl.0), true)) }\n";
        let rope = Rope::from_str(code);
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_rope(path, &rope).unwrap();

        // Find positions of all '(' characters
        let code_str = code;
        let paren_positions: Vec<usize> = code_str
            .char_indices()
            .filter(|&(_, c)| c == '(')
            .map(|(i, _)| i)
            .collect();

        // Each '(' should have a matching bracket
        for &byte_pos in &paren_positions {
            let row = rope.byte_to_line(byte_pos);
            let col = rope.byte_to_char(byte_pos) - rope.line_to_char(row);
            let result = state.matching_bracket(&rope, row, col);
            assert!(
                result.is_some(),
                "matching_bracket at col {} (byte {}, char '{}') returned None",
                col,
                byte_pos,
                code_str.as_bytes()[byte_pos] as char,
            );
        }
    }

    #[test]
    fn rust_bracket_multiline_tuple() {
        // Realistic multi-line case with tuple field access (rl.0)
        let code = "fn main() {\n    let result: Vec<_> = items\n        .iter()\n        .map(|rl| (Arc::new(rl.0), true))\n        .collect();\n}\n";
        let rope = Rope::from_str(code);
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_rope(path, &rope).unwrap();

        // Get brackets for the whole file
        let all_pairs = state.bracket_ranges(&rope, 0..rope.len_bytes());
        let paren_pairs: Vec<_> = all_pairs
            .iter()
            .filter(|bm| {
                let ch = rope.char(rope.byte_to_char(bm.open_range.start));
                ch == '('
            })
            .collect();
        assert!(
            paren_pairs.len() >= 4,
            "expected >=4 paren pairs, got {}",
            paren_pairs.len(),
        );

        // Test with restricted byte range (just the .map line)
        let map_line = 3;
        let line_start = rope.line_to_byte(map_line);
        let line_end = rope.line_to_byte(map_line + 1);
        let restricted_pairs = state.bracket_ranges(&rope, line_start..line_end);
        let restricted_parens: Vec<_> = restricted_pairs
            .iter()
            .filter(|bm| {
                let ch = rope.char(rope.byte_to_char(bm.open_range.start));
                ch == '('
            })
            .collect();
        assert!(
            restricted_parens.len() >= 3,
            "restricted range: expected >=3 paren pairs on .map line, got {}",
            restricted_parens.len(),
        );

        // Each paren should have a rainbow color_index
        for bm in &restricted_parens {
            assert!(bm.color_index.is_some());
        }
    }

    #[test]
    fn rust_imports_detected() {
        let code = "use std::io;\nuse std::path::PathBuf;\n\nfn main() {}\n";
        let rope = Rope::from_str(code);
        let path = Path::new("test.rs");
        let state = SyntaxState::from_path_and_rope(path, &rope).unwrap();
        let imports = state.imports(&rope);
        assert_eq!(
            imports.len(),
            2,
            "expected 2 imports, got {}",
            imports.len()
        );
    }
}
