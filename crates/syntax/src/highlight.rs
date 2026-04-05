use led_core::{Doc, Row};
use tree_sitter::{Query, QueryCursor, StreamingIterator, Tree};

use crate::parse::DocProvider;

#[derive(Debug, Clone)]
pub struct HighlightSpan {
    pub char_start: usize,
    pub char_end: usize,
    pub capture_name: String,
}

pub(crate) fn collect_highlights(
    query: &Query,
    tree: &Tree,
    doc: &dyn Doc,
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    end_line: usize,
) -> Vec<(usize, HighlightSpan)> {
    let total_lines = doc.line_count();
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(start_byte..end_byte);

    let capture_names = query.capture_names();
    let mut result = Vec::new();

    let mut matches = cursor.matches(query, tree.root_node(), DocProvider { doc });
    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        for cap in m.captures {
            let name = capture_names[cap.index as usize];
            let node = cap.node;
            let len = doc.len_bytes();
            // Clamp: tree may be out of sync with doc after edits
            let node_start_byte = node.start_byte().min(len);
            let node_end_byte = node.end_byte().min(len);
            if node_start_byte >= node_end_byte {
                continue;
            }

            let node_start_line = doc.byte_to_line(node_start_byte).0;
            let node_end_line = doc.byte_to_line(node_end_byte.min(len.max(1) - 1)).0;

            for line in node_start_line..=node_end_line {
                if line < start_line || line >= end_line {
                    continue;
                }
                let line_start_byte = doc.line_to_byte(Row(line));
                let line_end_byte = if line + 1 < total_lines {
                    doc.line_to_byte(Row(line + 1))
                } else {
                    doc.len_bytes()
                };

                let span_start_byte = node_start_byte.max(line_start_byte);
                let span_end_byte = node_end_byte.min(line_end_byte);
                if span_start_byte >= span_end_byte {
                    continue;
                }

                let line_start_char = doc.byte_to_char(line_start_byte);
                let char_start = doc.byte_to_char(span_start_byte) - line_start_char;
                let char_end = doc.byte_to_char(span_end_byte) - line_start_char;

                // Clamp to effective line length (exclude trailing newline)
                let effective_line_len = doc.line_len(Row(line));
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
