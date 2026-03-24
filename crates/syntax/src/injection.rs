use std::collections::HashMap;
use std::rc::Rc;

use led_core::Doc;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator, Tree};

use crate::config::InjectionConfig;
use crate::language::lang_for_name;
use crate::parse::{DocProvider, node_text, parse_doc};

pub(crate) struct InjectionLayer {
    pub tree: Tree,
    pub highlights_query: Rc<Query>,
    pub included_ranges: Vec<tree_sitter::Range>,
}

/// Cache of compiled highlight queries, keyed by language name.
pub(crate) type QueryCache = HashMap<String, Rc<Query>>;

pub(crate) fn build_injection_layers(
    config: &InjectionConfig,
    tree: &Tree,
    doc: &dyn Doc,
    query_cache: &mut QueryCache,
) -> Vec<InjectionLayer> {
    let mut cursor = QueryCursor::new();

    let mut single_layers: Vec<(String, tree_sitter::Range)> = Vec::new();
    let mut combined_ranges: HashMap<String, Vec<tree_sitter::Range>> = HashMap::new();

    let mut matches = cursor.matches(&config.query, tree.root_node(), DocProvider { doc });
    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let pattern_config = config.patterns.get(m.pattern_index);
        let combined = pattern_config.map_or(false, |p| p.combined);

        let lang_name = pattern_config
            .and_then(|p| p.language.as_deref())
            .map(|s| s.to_string())
            .or_else(|| {
                config.language_capture_ix.and_then(|ix| {
                    m.captures.iter().find_map(|c| {
                        if c.index == ix {
                            Some(node_text(doc, &c.node))
                        } else {
                            None
                        }
                    })
                })
            });

        let Some(lang_name) = lang_name else {
            continue;
        };

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

    for (lang_name, range) in single_layers {
        if let Some(layer) = create_injection_layer(&lang_name, vec![range], doc, query_cache) {
            layers.push(layer);
        }
    }

    for (lang_name, ranges) in combined_ranges {
        if !ranges.is_empty() {
            if let Some(layer) = create_injection_layer(&lang_name, ranges, doc, query_cache) {
                layers.push(layer);
            }
        }
    }

    layers
}

fn create_injection_layer(
    lang_name: &str,
    ranges: Vec<tree_sitter::Range>,
    doc: &dyn Doc,
    query_cache: &mut QueryCache,
) -> Option<InjectionLayer> {
    let (language, hl_query_src) = lang_for_name(lang_name)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    parser.set_included_ranges(&ranges).ok()?;
    let tree = parse_doc(&mut parser, doc, None)?;

    let highlights_query = if let Some(cached) = query_cache.get(lang_name) {
        Rc::clone(cached)
    } else {
        let query = Rc::new(Query::new(&language, &hl_query_src).ok()?);
        query_cache.insert(lang_name.to_string(), Rc::clone(&query));
        query
    };

    Some(InjectionLayer {
        tree,
        highlights_query,
        included_ranges: ranges,
    })
}
