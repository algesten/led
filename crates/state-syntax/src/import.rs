//! Sort-imports helper (M23).
//!
//! Port of legacy `led/crates/syntax/src/import.rs`, translated to
//! ropey + the rewrite's `Language` enum.
//!
//! Public surface is [`sort_imports`]: given a `Language`, the
//! parsed `Tree`, and the buffer's `Rope`, returns a
//! [`SortImportsPlan`] describing the byte range to replace and
//! the replacement text — or `None` when:
//! * the language has no `imports.scm`,
//! * the query found no imports,
//! * the imports are already in sorted order (the dispatch arm
//!   surfaces the "Imports already sorted" alert).
//!
//! Same dep-graph note as `indent.rs`: queries compile against
//! `tree.language()`, so `state-syntax` doesn't need any
//! per-grammar deps.

use std::sync::OnceLock;

use ropey::Rope;
use tree_sitter::{Query, QueryCursor, StreamingIterator, Tree};

use crate::Language;

/// Replacement plan produced by [`sort_imports`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortImportsPlan {
    /// First char of the (oldest) import block. Inclusive.
    pub start_char: usize,
    /// Char one past the end of the (newest) import block.
    /// Exclusive.
    pub end_char: usize,
    /// Text to write between `start_char` and `end_char`. Already
    /// includes terminator newlines per group.
    pub replacement: String,
}

fn imports_src(lang: Language) -> Option<&'static str> {
    Some(match lang {
        Language::Rust => include_str!("../queries/rust/imports.scm"),
        // Other languages can ship imports.scm in a follow-up;
        // until then they short to "Imports already sorted".
        _ => return None,
    })
}

struct ImportsConfig {
    query: Query,
    import_capture_ix: u32,
}

fn config_for(lang: Language, ts_lang: &tree_sitter::Language) -> Option<&'static ImportsConfig> {
    macro_rules! slot {
        () => {{
            static SLOT: OnceLock<ImportsConfig> = OnceLock::new();
            &SLOT
        }};
    }
    let slot: &'static OnceLock<ImportsConfig> = match lang {
        Language::Rust => slot!(),
        // Other languages can ship imports.scm in a follow-up;
        // until then they short to "Imports already sorted".
        _ => return None,
    };
    let cfg = slot.get_or_init(|| {
        let src = imports_src(lang)
            .unwrap_or_else(|| panic!("no imports query for {:?}", lang));
        let query = Query::new(ts_lang, src)
            .unwrap_or_else(|e| panic!("{:?} imports.scm: {e}", lang));
        let names = query.capture_names();
        let import_ix = names
            .iter()
            .position(|n| *n == "import")
            .unwrap_or_else(|| panic!("{:?} imports.scm missing @import capture", lang))
            as u32;
        ImportsConfig {
            query,
            import_capture_ix: import_ix,
        }
    });
    Some(cfg)
}

/// Pre-compile this language's imports query at startup. See
/// `indent::precompile_all_queries`.
pub(crate) fn warm(lang: Language, ts_lang: &tree_sitter::Language) {
    let _ = config_for(lang, ts_lang);
}

/// One captured import statement with its byte/row span.
#[derive(Debug, Clone)]
struct ImportItem {
    full_text: String,
    start_row: usize,
    end_row: usize,
    start_byte: usize,
    end_byte: usize,
}

fn collect_imports(cfg: &ImportsConfig, tree: &Tree, rope: &Rope) -> Vec<ImportItem> {
    let bytes = rope_to_bytes(rope);
    let mut cursor = QueryCursor::new();
    let mut items: Vec<ImportItem> = Vec::new();

    let mut matches = cursor.matches(&cfg.query, tree.root_node(), bytes.as_slice());
    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        for cap in m.captures {
            if cap.index == cfg.import_capture_ix {
                let start_byte = cap.node.start_byte();
                let end_byte = cap.node.end_byte();
                let text =
                    String::from_utf8_lossy(&bytes[start_byte..end_byte.min(bytes.len())])
                        .into_owned();
                items.push(ImportItem {
                    full_text: text,
                    start_row: cap.node.start_position().row,
                    end_row: cap.node.end_position().row,
                    start_byte,
                    end_byte,
                });
            }
        }
    }

    items.dedup_by(|a, b| a.start_byte == b.start_byte && a.end_byte == b.end_byte);
    items
}

/// Top-level public API. Returns a plan whose `replacement` is
/// the sorted imports joined with `\n`s; groups (separated by a
/// blank line in the source) sort independently.
pub fn sort_imports(lang: Language, tree: &Tree, rope: &Rope) -> Option<SortImportsPlan> {
    let ts_lang = crate::indent::ts_language(lang)?;
    let cfg = config_for(lang, &ts_lang)?;
    let items = collect_imports(cfg, tree, rope);
    if items.is_empty() {
        return None;
    }

    // Find contiguous groups (separated by ≥1 blank line). Items
    // arrive in document order from the query.
    let mut groups: Vec<Vec<&ImportItem>> = Vec::new();
    let mut current: Vec<&ImportItem> = vec![&items[0]];
    for i in 1..items.len() {
        let prev = &items[i - 1];
        let curr = &items[i];
        if curr.start_row > prev.end_row + 1 {
            groups.push(std::mem::take(&mut current));
        }
        current.push(curr);
    }
    if !current.is_empty() {
        groups.push(current);
    }

    let overall_start_byte = items.first()?.start_byte;
    let overall_end_byte = items.last()?.end_byte;

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

    // Already sorted? Compare against original (trim trailing
    // newlines so the import block's terminator doesn't make
    // every sort look "needed").
    let start_char = rope.byte_to_char(overall_start_byte);
    let end_char = rope.byte_to_char(overall_end_byte);
    let original: String = rope.slice(start_char..end_char).chars().collect();
    if result.trim_end_matches('\n') == original.trim_end_matches('\n') {
        return None;
    }

    // Trim trailing `\n`s from the replacement so we don't
    // invent blank lines after the block on apply.
    let trimmed = result.trim_end_matches('\n').to_string();

    Some(SortImportsPlan {
        start_char,
        end_char,
        replacement: trimmed,
    })
}

fn rope_to_bytes(rope: &Rope) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(rope.len_bytes());
    for chunk in rope.chunks() {
        bytes.extend_from_slice(chunk.as_bytes());
    }
    bytes
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
    fn sort_imports_already_sorted_returns_none() {
        let src = "use std::a;\nuse std::b;\nuse std::c;\n\nfn main() {}\n";
        let (tree, rope) = parse_rust(src);
        assert_eq!(sort_imports(Language::Rust, &tree, &rope), None);
    }

    #[test]
    fn sort_imports_unsorted_returns_plan() {
        let src = "use std::path::Path;\nuse std::collections::HashMap;\nuse std::fs;\n\nfn main() {}\n";
        let (tree, rope) = parse_rust(src);
        let plan = sort_imports(Language::Rust, &tree, &rope).expect("plan");
        // Sorted alphabetically by full text.
        assert_eq!(
            plan.replacement,
            "use std::collections::HashMap;\nuse std::fs;\nuse std::path::Path;"
        );
        // Span covers the three import lines.
        let start_byte = rope.char_to_byte(plan.start_char);
        let end_byte = rope.char_to_byte(plan.end_char);
        let original = String::from_utf8_lossy(&rope_to_bytes(&rope)[start_byte..end_byte])
            .into_owned();
        assert_eq!(
            original,
            "use std::path::Path;\nuse std::collections::HashMap;\nuse std::fs;"
        );
    }

    #[test]
    fn sort_imports_groups_separated_by_blank_line_sort_independently() {
        let src = "use b;\nuse a;\n\nuse y;\nuse x;\n\nfn main() {}\n";
        let (tree, rope) = parse_rust(src);
        let plan = sort_imports(Language::Rust, &tree, &rope).expect("plan");
        // Group 1 sorts to a,b; group 2 sorts to x,y; blank line
        // separates them.
        assert_eq!(plan.replacement, "use a;\nuse b;\n\nuse x;\nuse y;");
    }

    #[test]
    fn sort_imports_unknown_language_returns_none() {
        let (tree, rope) = parse_rust("use a;\n");
        assert_eq!(sort_imports(Language::Markdown, &tree, &rope), None);
    }

    #[test]
    fn sort_imports_no_imports_returns_none() {
        let (tree, rope) = parse_rust("fn main() {}\n");
        assert_eq!(sort_imports(Language::Rust, &tree, &rope), None);
    }
}
