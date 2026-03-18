use led_core::Doc;
use tree_sitter::{QueryCursor, StreamingIterator, Tree};

use crate::config::OutlineConfig;
use crate::parse::{DocProvider, node_text};

#[derive(Debug, Clone)]
pub struct OutlineItem {
    pub depth: usize,
    pub name: String,
    pub context: String,
    pub row: usize,
}

pub(crate) fn outline(config: &OutlineConfig, tree: &Tree, doc: &dyn Doc) -> Vec<OutlineItem> {
    let mut cursor = QueryCursor::new();
    let mut raw_items: Vec<(usize, usize, OutlineItem)> = Vec::new();

    let mut matches = cursor.matches(&config.query, tree.root_node(), DocProvider { doc });
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
                name_text = node_text(doc, &cap.node);
            } else if Some(cap.index) == config.context_capture_ix {
                context_parts.push((
                    cap.node.start_byte(),
                    cap.node.end_byte(),
                    node_text(doc, &cap.node),
                ));
            }
        }

        if let Some(node) = item_node {
            if name_text.is_empty() {
                continue;
            }

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

    raw_items.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));

    let mut stack: Vec<usize> = Vec::new();
    let mut result = Vec::with_capacity(raw_items.len());
    for (start, end, mut item) in raw_items {
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
