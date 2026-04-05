use std::ops::Range;

use led_core::{Doc, Row};
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
        doc.line_to_byte(Row(line + 1))
    } else {
        total_bytes
    };

    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(query_start..query_end);

    // (range, explicitly_terminated) — only ranges narrowed by @end or
    // truncated by @outdent should participate in the outdent check.
    // Bare continuation constructs (call_expression, field_expression, etc.)
    // have no @end, so their natural node end must not trigger outdent.
    let mut indent_ranges: Vec<(Range<usize>, bool)> = Vec::new();

    let mut matches = cursor.matches(&config.query, tree.root_node(), DocProvider { doc });
    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut node_range: Option<Range<usize>> = None;
        let mut outdent_pos: Option<usize> = None;
        let mut has_end = false;

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
                has_end = true;
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
                indent_ranges.push((range, has_end));
            }
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

    let within_error = is_within_error(error_query, tree, doc, line);

    let len = doc.len_bytes();
    let line_start = doc.line_to_byte(Row(line));

    // Unwrap basis_row from continuation constructs: if the basis row is
    // inside a non-terminated indent range that ends before the current line,
    // the basis indent is inflated by the continuation. Fall back to the
    // range's start line to get the true indent level.
    //
    // BUT: don't unwrap if the basis row opens a block (any indent range
    // starts on basis_row and extends past the current line). In that case
    // the "inflated" indent is the correct base for the block's content.
    let basis_row = {
        let basis_byte = doc.line_to_byte(Row(basis_row));
        let opens_block = indent_ranges.iter().any(|(r, _)| {
            r.start < len && doc.byte_to_line(r.start).0 == basis_row && r.end > line_start
        });
        if opens_block {
            basis_row
        } else {
            let mut row = basis_row;
            for (r, terminated) in &indent_ranges {
                if *terminated {
                    continue;
                }
                if r.start < basis_byte && r.end > basis_byte && r.end <= line_start {
                    let start_line = doc.byte_to_line(r.start).0;
                    if start_line < row {
                        row = start_line;
                    }
                }
            }
            row
        }
    };

    let basis_opens_indent = indent_ranges
        .iter()
        .filter(|(r, _)| r.start < len)
        .any(|(r, _)| doc.byte_to_line(r.start).0 == basis_row && r.end > line_start);

    let line_at_outdent = indent_ranges
        .iter()
        .filter(|(r, terminated)| *terminated && r.end > 0 && r.end <= len)
        .any(|(r, _)| doc.byte_to_line(r.end).0 == line && r.start < line_start);

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
    let line_start = doc.line_to_byte(Row(line));
    let line_end = if line + 1 < doc.line_count() {
        doc.line_to_byte(Row(line + 1))
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
    let basis_start = doc.line_to_byte(Row(basis_row));
    let line_byte = doc.line_to_byte(Row(line));
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
    led_core::with_line_buf(|line_text| {
        for row in (0..line).rev() {
            doc.line(Row(row), line_text);
            if line_text.chars().any(|c| !c.is_whitespace()) {
                return Some(row);
            }
        }
        if line > 0 { Some(0) } else { None }
    })
}

/// For a line whose first non-whitespace char is a closing bracket,
/// find the matching open bracket via tree-sitter and return its line's indent.
pub(crate) fn closing_bracket_indent(tree: &Tree, doc: &dyn Doc, line: usize) -> Option<String> {
    led_core::with_line_buf(|line_text| {
        doc.line(Row(line), line_text);
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
        let bracket_char_idx = doc.line_to_char(Row(line)).0 + char_offset;
        let bracket_byte = doc.char_to_byte(bracket_char_idx);
        let node = tree
            .root_node()
            .descendant_for_byte_range(bracket_byte, bracket_byte + 1)?;
        let parent = node.parent()?;
        Some(get_line_indent(doc, parent.start_position().row))
    })
}

/// Get leading whitespace of a line.
pub(crate) fn get_line_indent(doc: &dyn Doc, line: usize) -> String {
    led_core::with_line_buf(|line_text| {
        doc.line(Row(line), line_text);
        let mut indent = String::new();
        for ch in line_text.chars() {
            if ch == ' ' || ch == '\t' {
                indent.push(ch);
            } else {
                break;
            }
        }
        indent
    })
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
    led_core::with_line_buf(|line| {
        for i in 0..lines {
            doc.line(Row(i), line);
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
    })
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
    led_core::with_line_buf(|line_buf| {
        doc.line(Row(basis), line_buf);
        let basis_indent = get_line_indent(doc, basis);

        if let Some(re) = increase_pattern {
            if re.is_match(line_buf) {
                return Some(apply_indent_delta(
                    &basis_indent,
                    IndentDelta::Greater,
                    indent_unit,
                ));
            }
        }

        doc.line(Row(line), line_buf);
        if let Some(re) = decrease_pattern {
            if re.is_match(line_buf) {
                return Some(apply_indent_delta(
                    &basis_indent,
                    IndentDelta::Less,
                    indent_unit,
                ));
            }
        }

        None
    })
}

fn find_prev_nonempty_line(doc: &dyn Doc, line: usize) -> Option<usize> {
    let mut line_text = String::new();
    for row in (0..line).rev() {
        doc.line(Row(row), &mut line_text);
        if line_text.chars().any(|c| !c.is_whitespace()) {
            return Some(row);
        }
    }
    if line > 0 { Some(0) } else { None }
}
