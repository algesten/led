use std::ops::Range;

use led_core::Doc;
use tree_sitter::{QueryCursor, StreamingIterator, Tree};

use crate::config::BracketsConfig;
use crate::parse::DocProvider;

#[derive(Debug, Clone)]
pub struct BracketMatch {
    pub open_range: Range<usize>,
    pub close_range: Range<usize>,
    pub color_index: Option<usize>,
}

pub(crate) fn bracket_ranges(
    config: &BracketsConfig,
    tree: &Tree,
    doc: &dyn Doc,
    range: Range<usize>,
) -> Vec<BracketMatch> {
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(range);

    let mut pairs = Vec::new();
    let mut matches = cursor.matches(&config.query, tree.root_node(), DocProvider { doc });
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

    assign_rainbow_depth(&mut pairs);

    pairs
}

pub(crate) fn matching_bracket(
    config: &BracketsConfig,
    tree: &Tree,
    doc: &dyn Doc,
    row: usize,
    col: usize,
) -> Option<(usize, usize)> {
    let char_idx = doc.line_to_char(row) + col;
    let byte_pos = doc.char_to_byte(char_idx);

    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(0..doc.len_bytes());

    let mut matches = cursor.matches(&config.query, tree.root_node(), DocProvider { doc });
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
            let len = doc.len_bytes();
            if or.start <= byte_pos && byte_pos < or.end {
                let target_byte = cr.start.min(len.saturating_sub(1));
                let target_line = doc.byte_to_line(target_byte);
                let target_char = doc.byte_to_char(target_byte);
                let line_char = doc.line_to_char(target_line);
                return Some((target_line, target_char - line_char));
            }
            if cr.start <= byte_pos && byte_pos < cr.end {
                let target_byte = or.start.min(len.saturating_sub(1));
                let target_line = doc.byte_to_line(target_byte);
                let target_char = doc.byte_to_char(target_byte);
                let line_char = doc.line_to_char(target_line);
                return Some((target_line, target_char - line_char));
            }
        }
    }

    None
}

pub fn assign_rainbow_depth(pairs: &mut [BracketMatch]) {
    let mut indexed: Vec<(usize, usize)> = pairs
        .iter()
        .enumerate()
        .map(|(i, p)| (i, p.open_range.start))
        .collect();
    indexed.sort_by_key(|&(_, start)| start);

    let mut stack: Vec<usize> = Vec::new();

    for (idx, _) in indexed {
        let pair = &pairs[idx];
        if pair.color_index.is_none() {
            continue;
        }
        let close_pos = pair.close_range.start;

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
