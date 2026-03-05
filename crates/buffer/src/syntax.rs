use std::path::Path;

use ropey::Rope;
use tree_sitter::{InputEdit, Language, Parser, Point, Query, QueryCursor, StreamingIterator, Tree};

// ---------------------------------------------------------------------------
// Highlight span returned to the renderer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct HighlightSpan {
    pub char_start: usize,
    pub char_end: usize,
    pub capture_name: &'static str,
}

// ---------------------------------------------------------------------------
// Language registry
// ---------------------------------------------------------------------------

struct LangEntry {
    language: Language,
    highlights_query: &'static str,
}

fn lang_for_ext(ext: &str) -> Option<LangEntry> {
    match ext {
        "rs" => Some(LangEntry {
            language: tree_sitter_rust::LANGUAGE.into(),
            highlights_query: tree_sitter_rust::HIGHLIGHTS_QUERY,
        }),
        "toml" => Some(LangEntry {
            language: tree_sitter_toml_ng::LANGUAGE.into(),
            highlights_query: tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
        }),
        "json" => Some(LangEntry {
            language: tree_sitter_json::LANGUAGE.into(),
            highlights_query: tree_sitter_json::HIGHLIGHTS_QUERY,
        }),
        "js" | "jsx" | "mjs" => Some(LangEntry {
            language: tree_sitter_javascript::LANGUAGE.into(),
            highlights_query: tree_sitter_javascript::HIGHLIGHT_QUERY,
        }),
        "ts" | "tsx" => {
            // TypeScript queries reference JS captures, so we concatenate
            let lang = if ext == "tsx" {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            } else {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            };
            Some(LangEntry {
                language: lang,
                highlights_query: tree_sitter_typescript::HIGHLIGHTS_QUERY,
            })
        }
        "md" | "markdown" => Some(LangEntry {
            language: tree_sitter_md::LANGUAGE.into(),
            highlights_query: tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
        }),
        "py" => Some(LangEntry {
            language: tree_sitter_python::LANGUAGE.into(),
            highlights_query: tree_sitter_python::HIGHLIGHTS_QUERY,
        }),
        "sh" | "bash" => Some(LangEntry {
            language: tree_sitter_bash::LANGUAGE.into(),
            highlights_query: tree_sitter_bash::HIGHLIGHT_QUERY,
        }),
        "swift" => Some(LangEntry {
            language: tree_sitter_swift::LANGUAGE.into(),
            highlights_query: tree_sitter_swift::HIGHLIGHTS_QUERY,
        }),
        "c" | "h" => Some(LangEntry {
            language: tree_sitter_c::LANGUAGE.into(),
            highlights_query: tree_sitter_c::HIGHLIGHT_QUERY,
        }),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some(LangEntry {
            language: tree_sitter_cpp::LANGUAGE.into(),
            highlights_query: tree_sitter_cpp::HIGHLIGHT_QUERY,
        }),
        _ => None,
    }
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
// SyntaxState
// ---------------------------------------------------------------------------

pub(crate) struct SyntaxState {
    parser: Parser,
    tree: Tree,
    query: Query,
    /// Capture names owned by the Query, but we store references via indices
    _capture_names: Vec<String>,
}

impl SyntaxState {
    pub(crate) fn from_path_and_rope(path: &Path, rope: &Rope) -> Option<Self> {
        let ext = path.extension()?.to_str()?;
        let entry = lang_for_ext(ext)?;

        let mut parser = Parser::new();
        parser.set_language(&entry.language).ok()?;

        let query = Query::new(&entry.language, entry.highlights_query).ok()?;
        let _capture_names: Vec<String> =
            query.capture_names().iter().map(|s| s.to_string()).collect();

        let tree = parse_rope(&mut parser, rope, None)?;

        Some(Self {
            parser,
            tree,
            query,
            _capture_names,
        })
    }

    pub(crate) fn apply_edit(&mut self, edit: &InputEdit, rope: &Rope) {
        self.tree.edit(edit);
        if let Some(new_tree) = parse_rope(&mut self.parser, rope, Some(&self.tree)) {
            self.tree = new_tree;
        }
    }

    /// Query highlights for visible lines [start_line, end_line).
    /// Returns a vec of (line_index, HighlightSpan).
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

        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(start_byte..end_byte);

        let capture_names = self.query.capture_names();
        let mut result = Vec::new();

        let mut matches = cursor.matches(
            &self.query,
            self.tree.root_node(),
            RopeProvider { rope },
        );
        while let Some(m) = { matches.advance(); matches.get() } {
            for cap in m.captures {
                let name = capture_names[cap.index as usize];
                let node = cap.node;
                let node_start_byte = node.start_byte();
                let node_end_byte = node.end_byte();

                // Convert byte range to per-line char spans
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

                    // Convert byte offsets within line to char offsets within line
                    let line_start_char = rope.byte_to_char(line_start_byte);
                    let char_start = rope.byte_to_char(span_start_byte) - line_start_char;
                    let char_end = rope.byte_to_char(span_end_byte) - line_start_char;

                    // Strip trailing newline from char_end
                    let line_char_count = rope.line(line).len_chars();
                    let effective_line_len = if line_char_count > 0
                        && rope.line(line).char(line_char_count - 1) == '\n'
                    {
                        line_char_count - 1
                    } else {
                        line_char_count
                    };
                    let char_end = char_end.min(effective_line_len);
                    if char_start >= char_end {
                        continue;
                    }

                    // SAFETY: capture_names come from a compiled Query that lives as long
                    // as SyntaxState; we leak the &str to 'static since it's stable for
                    // the lifetime of the query. The Query stores names as &'static str
                    // internally (from include_str!).
                    let name_static: &'static str = unsafe { std::mem::transmute(name) };

                    result.push((
                        line,
                        HighlightSpan {
                            char_start,
                            char_end,
                            capture_name: name_static,
                        },
                    ));
                }
            }
        }

        result
    }
}

// ---------------------------------------------------------------------------
// InputEdit helpers — call BEFORE mutating the rope
// ---------------------------------------------------------------------------

/// Byte offset and Point for a char index in the rope.
fn byte_and_point(rope: &Rope, char_idx: usize) -> (usize, Point) {
    let byte = rope.char_to_byte(char_idx);
    let line = rope.char_to_line(char_idx);
    let line_byte = rope.line_to_byte(line);
    (byte, Point { row: line, column: byte - line_byte })
}

/// Build an InputEdit for inserting `text` at `char_idx`.
/// Must be called BEFORE the rope is mutated.
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
        new_end_position: Point { row: end_row, column: end_col },
    }
}

/// Build an InputEdit for removing chars [char_start, char_end).
/// Must be called BEFORE the rope is mutated.
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
