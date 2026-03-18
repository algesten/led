use tree_sitter::{Language, Query};

// ── Indentation ──

pub(crate) struct IndentsConfig {
    pub query: Query,
    pub indent_capture_ix: u32,
    pub start_capture_ix: Option<u32>,
    pub end_capture_ix: Option<u32>,
    pub outdent_capture_ix: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndentDelta {
    Greater,
    Less,
    Equal,
}

pub struct IndentSuggestion {
    pub basis_row: usize,
    pub delta: IndentDelta,
    pub within_error: bool,
}

// ── Brackets ──

pub(crate) struct BracketsPatternConfig {
    pub rainbow_exclude: bool,
}

pub(crate) struct BracketsConfig {
    pub query: Query,
    pub open_capture_ix: u32,
    pub close_capture_ix: u32,
    pub patterns: Vec<BracketsPatternConfig>,
}

// ── Outline ──

pub(crate) struct OutlineConfig {
    pub query: Query,
    pub item_capture_ix: u32,
    pub name_capture_ix: u32,
    pub context_capture_ix: Option<u32>,
}

// ── Injections ──

pub(crate) struct InjectionConfig {
    pub query: Query,
    pub content_capture_ix: u32,
    pub language_capture_ix: Option<u32>,
    pub patterns: Vec<InjectionPatternConfig>,
}

pub(crate) struct InjectionPatternConfig {
    pub language: Option<String>,
    pub combined: bool,
}

// ── Imports ──

pub(crate) struct ImportsConfig {
    pub query: Query,
    pub import_capture_ix: u32,
}

// ── Compilation helpers ──

pub(crate) fn compile_indents_config(
    language: &Language,
    query_src: &str,
) -> Option<IndentsConfig> {
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

pub(crate) fn compile_brackets_config(
    language: &Language,
    query_src: &str,
) -> Option<BracketsConfig> {
    let query = Query::new(language, query_src).ok()?;
    let names = query.capture_names();
    let open_ix = names.iter().position(|n| *n == "open")? as u32;
    let close_ix = names.iter().position(|n| *n == "close")? as u32;

    let mut patterns = Vec::new();
    for i in 0..query.pattern_count() {
        let mut rainbow_exclude = false;
        for prop in query.property_settings(i) {
            if &*prop.key == "rainbow.exclude" {
                rainbow_exclude = true;
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

pub(crate) fn compile_outline_config(
    language: &Language,
    query_src: &str,
) -> Option<OutlineConfig> {
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

pub(crate) fn compile_injection_config(
    language: &Language,
    query_src: &str,
) -> Option<InjectionConfig> {
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

pub(crate) fn compile_imports_config(
    language: &Language,
    query_src: &str,
) -> Option<ImportsConfig> {
    let query = Query::new(language, query_src).ok()?;
    let names = query.capture_names();
    let import_ix = names.iter().position(|n| *n == "import")? as u32;
    Some(ImportsConfig {
        query,
        import_capture_ix: import_ix,
    })
}
