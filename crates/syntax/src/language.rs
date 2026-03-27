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
        "toml" => Some(LangEntry {
            indents_query: Some(include_str!("../queries/toml/indents.scm")),
            brackets_query: Some(include_str!("../queries/toml/brackets.scm")),
            increase_indent_pattern: Some(r"[\{\[]\s*$"),
            decrease_indent_pattern: Some(r"^\s*[}\]]"),
            reindent_chars: &['}', ']'],
            ..LangEntry::new(
                tree_sitter_toml_ng::LANGUAGE.into(),
                tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
            )
        }),
        "json" => Some(LangEntry {
            indents_query: Some(include_str!("../queries/json/indents.scm")),
            brackets_query: Some(include_str!("../queries/json/brackets.scm")),
            increase_indent_pattern: Some(r"[\{\[]\s*$"),
            decrease_indent_pattern: Some(r"^\s*[}\]]"),
            reindent_chars: &['}', ']'],
            ..LangEntry::new(
                tree_sitter_json::LANGUAGE.into(),
                tree_sitter_json::HIGHLIGHTS_QUERY,
            )
        }),
        "js" | "jsx" | "mjs" => Some(LangEntry {
            indents_query: Some(include_str!("../queries/javascript/indents.scm")),
            brackets_query: Some(include_str!("../queries/javascript/brackets.scm")),
            increase_indent_pattern: Some(r"[\{\[\(]\s*$"),
            decrease_indent_pattern: Some(r"^\s*[}\]\)]"),
            reindent_chars: &['}', ')', ']'],
            ..LangEntry::new(
                tree_sitter_javascript::LANGUAGE.into(),
                tree_sitter_javascript::HIGHLIGHT_QUERY,
            )
        }),
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
                indents_query: Some(include_str!("../queries/javascript/indents.scm")),
                brackets_query: Some(include_str!("../queries/javascript/brackets.scm")),
                increase_indent_pattern: Some(r"[\{\[\(]\s*$"),
                decrease_indent_pattern: Some(r"^\s*[}\]\)]"),
                reindent_chars: &['}', ')', ']'],
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
        "py" => Some(LangEntry {
            indents_query: Some(include_str!("../queries/python/indents.scm")),
            brackets_query: Some(include_str!("../queries/python/brackets.scm")),
            increase_indent_pattern: Some(r":\s*(#.*)?$"),
            decrease_indent_pattern: Some(r"^\s*(return|pass|break|continue|raise)\b"),
            reindent_chars: &['}', ')', ']'],
            ..LangEntry::new(
                tree_sitter_python::LANGUAGE.into(),
                tree_sitter_python::HIGHLIGHTS_QUERY,
            )
        }),
        "sh" | "bash" => Some(LangEntry {
            indents_query: Some(include_str!("../queries/bash/indents.scm")),
            brackets_query: Some(include_str!("../queries/bash/brackets.scm")),
            increase_indent_pattern: Some(r"(then|do|else|\{)\s*(#.*)?$"),
            decrease_indent_pattern: Some(r"^\s*(fi|done|else|elif|esac|\})"),
            reindent_chars: &['}'],
            ..LangEntry::new(
                tree_sitter_bash::LANGUAGE.into(),
                tree_sitter_bash::HIGHLIGHT_QUERY,
            )
        }),
        "swift" => Some(LangEntry {
            indents_query: Some(include_str!("../queries/swift/indents.scm")),
            brackets_query: Some(include_str!("../queries/swift/brackets.scm")),
            increase_indent_pattern: Some(r"[\{\[\(]\s*$"),
            decrease_indent_pattern: Some(r"^\s*[}\]\)]"),
            reindent_chars: &['}', ')', ']'],
            ..LangEntry::new(
                tree_sitter_swift::LANGUAGE.into(),
                tree_sitter_swift::HIGHLIGHTS_QUERY,
            )
        }),
        "c" | "h" => Some(LangEntry {
            indents_query: Some(include_str!("../queries/c/indents.scm")),
            brackets_query: Some(include_str!("../queries/c/brackets.scm")),
            increase_indent_pattern: Some(r"[\{\[\(]\s*$"),
            decrease_indent_pattern: Some(r"^\s*[}\]\)]"),
            reindent_chars: &['}', ')', ']'],
            ..LangEntry::new(
                tree_sitter_c::LANGUAGE.into(),
                tree_sitter_c::HIGHLIGHT_QUERY,
            )
        }),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some(LangEntry {
            indents_query: Some(include_str!("../queries/c/indents.scm")),
            brackets_query: Some(include_str!("../queries/c/brackets.scm")),
            increase_indent_pattern: Some(r"[\{\[\(]\s*$"),
            decrease_indent_pattern: Some(r"^\s*[}\]\)]"),
            reindent_chars: &['}', ')', ']'],
            ..LangEntry::new(
                tree_sitter_cpp::LANGUAGE.into(),
                tree_sitter_cpp::HIGHLIGHT_QUERY,
            )
        }),
        "mk" => Some(LangEntry::new(
            tree_sitter_make::LANGUAGE.into(),
            tree_sitter_make::HIGHLIGHTS_QUERY,
        )),
        "rb" => Some(LangEntry {
            increase_indent_pattern: Some(
                r"(def|class|module|if|unless|while|until|for|begin|do|case|else|elsif|when|rescue|ensure)\b.*$|\{[^}]*$|\bdo\s*(\|[^|]*\|)?\s*$",
            ),
            decrease_indent_pattern: Some(r"^\s*(end|else|elsif|when|rescue|ensure|\})"),
            reindent_chars: &['}', ']', ')'],
            ..LangEntry::new(
                tree_sitter_ruby::LANGUAGE.into(),
                tree_sitter_ruby::HIGHLIGHTS_QUERY,
            )
        }),
        _ => None,
    }
}

pub(crate) fn lang_for_filename(name: &str) -> Option<LangEntry> {
    match name {
        "Makefile" | "makefile" | "GNUmakefile" | "BSDmakefile" => Some(LangEntry::new(
            tree_sitter_make::LANGUAGE.into(),
            tree_sitter_make::HIGHLIGHTS_QUERY,
        )),
        "Gemfile" | "Rakefile" | "Vagrantfile" | "Guardfile" | "Podfile" | "Capfile"
        | "Brewfile" | "Thorfile" | "Dangerfile" | "Berksfile" | "Puppetfile" | "Steepfile"
        | "Fastfile" | "Appfile" | "Matchfile" | "Deliverfile" | "Snapfile" | "Scanfile"
        | "Gymfile" => lang_for_ext("rb"),
        ".bashrc" | ".bash_profile" | ".bash_logout" | ".bash_aliases" | ".profile" | ".envrc"
        | "PKGBUILD" => lang_for_ext("sh"),
        "SConstruct" | "SConscript" | "Snakefile" | "wscript" => lang_for_ext("py"),
        "Pipfile" => lang_for_ext("toml"),
        ".babelrc" => lang_for_ext("json"),
        _ => None,
    }
}

pub(crate) fn lang_entry_for_name(name: &str) -> Option<LangEntry> {
    match name {
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
        "ruby" => lang_for_ext("rb"),
        _ => None,
    }
}

pub(crate) fn lang_for_name(name: &str) -> Option<(Language, Cow<'static, str>)> {
    lang_entry_for_name(name).map(|e| (e.language, e.highlights_query))
}
