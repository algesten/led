use led_core::{CharOffset, Doc};
use tree_sitter::{QueryCursor, StreamingIterator, Tree};

use crate::config::ImportsConfig;
use crate::parse::{DocProvider, node_text};

#[derive(Debug, Clone)]
pub struct ImportItem {
    pub full_text: String,
    pub start_row: usize,
    pub end_row: usize,
    pub start_byte: usize,
    pub end_byte: usize,
}

pub(crate) fn imports(config: &ImportsConfig, tree: &Tree, doc: &dyn Doc) -> Vec<ImportItem> {
    let mut cursor = QueryCursor::new();
    let mut items = Vec::new();

    let mut matches = cursor.matches(&config.query, tree.root_node(), DocProvider { doc });
    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        for cap in m.captures {
            if cap.index == config.import_capture_ix {
                items.push(ImportItem {
                    full_text: node_text(doc, &cap.node),
                    start_row: cap.node.start_position().row,
                    end_row: cap.node.end_position().row,
                    start_byte: cap.node.start_byte(),
                    end_byte: cap.node.end_byte(),
                });
            }
        }
    }

    items.dedup_by(|a, b| a.start_byte == b.start_byte && a.end_byte == b.end_byte);

    items
}

/// Sort imports and return the replacement text.
/// Returns (start_byte, end_byte, replacement_text) if imports need reordering.
pub fn sort_imports_text(
    doc: &dyn Doc,
    import_items: &[ImportItem],
) -> Option<(usize, usize, String)> {
    if import_items.is_empty() {
        return None;
    }

    // Find contiguous groups of imports (separated by blank lines)
    let mut groups: Vec<Vec<&ImportItem>> = Vec::new();
    let mut current_group: Vec<&ImportItem> = vec![&import_items[0]];

    for i in 1..import_items.len() {
        let prev = &import_items[i - 1];
        let curr = &import_items[i];
        if curr.start_row > prev.end_row + 1 {
            groups.push(std::mem::take(&mut current_group));
        }
        current_group.push(curr);
    }
    if !current_group.is_empty() {
        groups.push(current_group);
    }

    let overall_start = import_items.first()?.start_byte;
    let overall_end = import_items.last()?.end_byte;

    let mut result = String::new();
    for (gi, group) in groups.iter().enumerate() {
        let mut sorted: Vec<&str> = group.iter().map(|i| i.full_text.as_str()).collect();
        sorted.sort();
        for text in &sorted {
            result.push_str(text);
            if !text.ends_with('\n') {
                result.push('\n');
            }
        }
        if gi + 1 < groups.len() {
            result.push('\n');
        }
    }

    // Check if already sorted
    let original_start_char = CharOffset(doc.byte_to_char(overall_start));
    let original_end_char = CharOffset(doc.byte_to_char(overall_end));
    let original = doc.slice(original_start_char, original_end_char);
    if result.trim_end_matches('\n') == original.trim_end_matches('\n') {
        return None;
    }

    Some((overall_start, overall_end, result))
}
