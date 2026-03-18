use std::ops::Range;

use led_core::Doc;
use tree_sitter::{Query, QueryCursor, StreamingIterator, Tree};

use crate::config::{IndentDelta, IndentSuggestion, IndentsConfig};
use crate::parse::DocProvider;

/// Suggest indent for a line using the current tree.
pub(crate) fn suggest_indent(
    config: &IndentsConfig,
    tree: &Tree,
    error_query: Option<&Query>,
    doc: &dyn Doc,
    line: usize,
) -> Option<IndentSuggestion> {
    suggest_indent_with_tree(config, tree, error_query, doc, line)
}

/// Suggest indent using a specific tree (for two-pass comparison).
pub(crate) fn suggest_indent_with_tree(
    config: &IndentsConfig,
    tree: &Tree,
    error_query: Option<&Query>,
    doc: &dyn Doc,
    line: usize,
) -> Option<IndentSuggestion> {
    let basis_row = find_basis_row(doc, line)?;

    let total_bytes = doc.len_bytes();
    let query_start = indent_query_start(doc, tree, basis_row, line);
    let query_end = if line + 1 < doc.line_count() {
        doc.line_to_byte(line + 1)
    } else {
        total_bytes
    };

    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(query_start..query_end);

    let mut indent_ranges: Vec<Range<usize>> = Vec::new();

    let mut matches = cursor.matches(&config.query, tree.root_node(), DocProvider { doc });
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

    let within_error = is_within_error(error_query, tree, doc, line);

    let len = doc.len_bytes();
    let line_start = doc.line_to_byte(line);

    let basis_opens_indent = indent_ranges
        .iter()
        .filter(|r| r.start < len)
        .any(|r| doc.byte_to_line(r.start) == basis_row && r.end > line_start);

    let line_at_outdent = indent_ranges
        .iter()
        .filter(|r| r.end > 0 && r.end <= len)
        .any(|r| doc.byte_to_line(r.end) == line && r.start < line_start);

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

fn is_within_error(error_query: Option<&Query>, tree: &Tree, doc: &dyn Doc, line: usize) -> bool {
    let Some(eq) = error_query else {
        return false;
    };
    let line_start = doc.line_to_byte(line);
    let line_end = if line + 1 < doc.line_count() {
        doc.line_to_byte(line + 1)
    } else {
        doc.len_bytes()
    };
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(line_start..line_end);
    let mut matches = cursor.matches(eq, tree.root_node(), DocProvider { doc });
    matches.advance();
    matches.get().is_some()
}

fn indent_query_start(doc: &dyn Doc, tree: &Tree, basis_row: usize, line: usize) -> usize {
    let basis_start = doc.line_to_byte(basis_row);
    let line_byte = doc.line_to_byte(line);
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

fn find_basis_row(doc: &dyn Doc, line: usize) -> Option<usize> {
    for row in (0..line).rev() {
        let line_text = doc.line(row);
        if line_text.chars().any(|c| !c.is_whitespace()) {
            return Some(row);
        }
    }
    if line > 0 { Some(0) } else { None }
}

/// For a line whose first non-whitespace char is a closing bracket,
/// find the matching open bracket via tree-sitter and return its line's indent.
pub(crate) fn closing_bracket_indent(tree: &Tree, doc: &dyn Doc, line: usize) -> Option<String> {
    let line_text = doc.line(line);
    let mut char_offset = 0;
    let mut found_bracket = false;
    for ch in line_text.chars() {
        if ch == ' ' || ch == '\t' {
            char_offset += 1;
            continue;
        }
        if ch != '}' && ch != ')' && ch != ']' {
            return None;
        }
        found_bracket = true;
        break;
    }
    if !found_bracket {
        return None;
    }
    let bracket_char_idx = doc.line_to_char(line) + char_offset;
    let bracket_byte = doc.char_to_byte(bracket_char_idx);
    let node = tree
        .root_node()
        .descendant_for_byte_range(bracket_byte, bracket_byte + 1)?;
    let parent = node.parent()?;
    Some(get_line_indent(doc, parent.start_position().row))
}

/// Get leading whitespace of a line.
pub(crate) fn get_line_indent(doc: &dyn Doc, line: usize) -> String {
    let line_text = doc.line(line);
    let mut indent = String::new();
    for ch in line_text.chars() {
        if ch == ' ' || ch == '\t' {
            indent.push(ch);
        } else {
            break;
        }
    }
    indent
}

/// Apply an indent delta to a basis indentation string.
pub(crate) fn apply_indent_delta(
    basis_indent: &str,
    delta: IndentDelta,
    indent_unit: &str,
) -> String {
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

/// Detect the indent unit used in the file.
pub(crate) fn detect_indent_unit(doc: &dyn Doc) -> String {
    let lines = doc.line_count().min(100);
    for i in 0..lines {
        let line = doc.line(i);
        let mut indent = String::new();
        for ch in line.chars() {
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

/// Regex-based indent fallback for when the tree is in an error state.
pub(crate) fn regex_indent(
    doc: &dyn Doc,
    line: usize,
    indent_unit: &str,
    increase_pattern: Option<&regex::Regex>,
    decrease_pattern: Option<&regex::Regex>,
) -> Option<String> {
    let basis = find_prev_nonempty_line(doc, line)?;
    let basis_text = doc.line(basis);
    let basis_indent = get_line_indent(doc, basis);

    if let Some(re) = increase_pattern {
        if re.is_match(&basis_text) {
            return Some(apply_indent_delta(
                &basis_indent,
                IndentDelta::Greater,
                indent_unit,
            ));
        }
    }

    let current_text = doc.line(line);
    if let Some(re) = decrease_pattern {
        if re.is_match(&current_text) {
            return Some(apply_indent_delta(
                &basis_indent,
                IndentDelta::Less,
                indent_unit,
            ));
        }
    }

    None
}

fn find_prev_nonempty_line(doc: &dyn Doc, line: usize) -> Option<usize> {
    for row in (0..line).rev() {
        let line_text = doc.line(row);
        if line_text.chars().any(|c| !c.is_whitespace()) {
            return Some(row);
        }
    }
    if line > 0 { Some(0) } else { None }
}
