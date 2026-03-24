use std::borrow::Cow;

use tree_sitter::Language;

pub(crate) struct LangEntry {
    pub language: Language,
    pub highlights_query: Cow<'static, str>,
    pub indents_query: Option<&'static str>,
    pub brackets_query: Option<&'static str>,
    pub outline_query: Option<&'static str>,
    pub injections_query: Option<&'static str>,
    pub imports_query: Option<&'static str>,
    pub increase_indent_pattern: Option<&'static str>,
    pub decrease_indent_pattern: Option<&'static str>,
    /// Characters that trigger re-indentation when typed (e.g. closing brackets).
    pub reindent_chars: &'static [char],
}

impl LangEntry {
    fn new(language: Language, highlights_query: &'static str) -> Self {
        Self {
            language,
            highlights_query: Cow::Borrowed(highlights_query),
            indents_query: None,
            brackets_query: None,
            outline_query: None,
            injections_query: None,
            imports_query: None,
            increase_indent_pattern: None,
            decrease_indent_pattern: None,
            reindent_chars: &[],
        }
    }
}

pub(crate) fn lang_for_ext(ext: &str) -> Option<LangEntry> {
    match ext {
        "rs" => Some(LangEntry {
            indents_query: Some(include_str!("../queries/rust/indents.scm")),
            brackets_query: Some(include_str!("../queries/rust/brackets.scm")),
            outline_query: Some(include_str!("../queries/rust/outline.scm")),
            injections_query: Some(include_str!("../queries/rust/injections.scm")),
            imports_query: Some(include_str!("../queries/rust/imports.scm")),
            increase_indent_pattern: Some(r"\{[^}]*$"),
            decrease_indent_pattern: Some(r"^\s*\}"),
            reindent_chars: &['}', ')', ']'],
            ..LangEntry::new(
                tree_sitter_rust::LANGUAGE.into(),
                tree_sitter_rust::HIGHLIGHTS_QUERY,
            )
        }),
        "toml" => Some(LangEntry::new(
            tree_sitter_toml_ng::LANGUAGE.into(),
            tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
        )),
        "json" => Some(LangEntry::new(
            tree_sitter_json::LANGUAGE.into(),
            tree_sitter_json::HIGHLIGHTS_QUERY,
        )),
        "js" | "jsx" | "mjs" => Some(LangEntry::new(
            tree_sitter_javascript::LANGUAGE.into(),
            tree_sitter_javascript::HIGHLIGHT_QUERY,
        )),
        "ts" | "tsx" => {
            let lang = if ext == "tsx" {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            } else {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            };
            let combined = format!(
                "{}\n{}",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            );
            Some(LangEntry {
                highlights_query: Cow::Owned(combined),
                ..LangEntry::new(lang, "")
            })
        }
        "md" | "markdown" => Some(LangEntry {
            injections_query: Some(include_str!("../queries/markdown/injections.scm")),
            ..LangEntry::new(
                tree_sitter_md::LANGUAGE.into(),
                tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
            )
        }),
        "py" => Some(LangEntry::new(
            tree_sitter_python::LANGUAGE.into(),
            tree_sitter_python::HIGHLIGHTS_QUERY,
        )),
        "sh" | "bash" => Some(LangEntry::new(
            tree_sitter_bash::LANGUAGE.into(),
            tree_sitter_bash::HIGHLIGHT_QUERY,
        )),
        "swift" => Some(LangEntry::new(
            tree_sitter_swift::LANGUAGE.into(),
            tree_sitter_swift::HIGHLIGHTS_QUERY,
        )),
        "c" | "h" => Some(LangEntry::new(
            tree_sitter_c::LANGUAGE.into(),
            tree_sitter_c::HIGHLIGHT_QUERY,
        )),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some(LangEntry::new(
            tree_sitter_cpp::LANGUAGE.into(),
            tree_sitter_cpp::HIGHLIGHT_QUERY,
        )),
        "mk" => Some(LangEntry::new(
            tree_sitter_make::LANGUAGE.into(),
            tree_sitter_make::HIGHLIGHTS_QUERY,
        )),
        _ => None,
    }
}

pub(crate) fn lang_for_filename(name: &str) -> Option<LangEntry> {
    match name {
        "Makefile" | "makefile" | "GNUmakefile" => Some(LangEntry::new(
            tree_sitter_make::LANGUAGE.into(),
            tree_sitter_make::HIGHLIGHTS_QUERY,
        )),
        _ => None,
    }
}

pub(crate) fn lang_for_name(name: &str) -> Option<(Language, Cow<'static, str>)> {
    let entry = match name {
        "rust" => lang_for_ext("rs"),
        "python" => lang_for_ext("py"),
        "javascript" | "js" => lang_for_ext("js"),
        "typescript" | "ts" => lang_for_ext("ts"),
        "tsx" => lang_for_ext("tsx"),
        "json" => lang_for_ext("json"),
        "toml" => lang_for_ext("toml"),
        "markdown" | "md" => lang_for_ext("md"),
        "bash" | "sh" => lang_for_ext("sh"),
        "c" => lang_for_ext("c"),
        "cpp" | "c++" => lang_for_ext("cpp"),
        "swift" => lang_for_ext("swift"),
        "make" => lang_for_ext("mk"),
        _ => None,
    };
    entry.map(|e| (e.language, e.highlights_query))
}
