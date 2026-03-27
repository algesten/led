// ============================================================================
// Theme types
// ============================================================================

use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StyleValue {
    Scalar(String),
    Style(StyleTable),
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct StyleTable {
    pub fg: Option<String>,
    pub bg: Option<String>,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TabsTheme {
    pub active: StyleValue,
    pub inactive: StyleValue,
    pub preview_active: StyleValue,
    pub preview_inactive: StyleValue,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StatusBarTheme {
    pub style: StyleValue,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EditorTheme {
    pub text: StyleValue,
    pub gutter: StyleValue,
    pub selection: StyleValue,
    pub search_match: StyleValue,
    pub search_current: StyleValue,
    #[serde(default)]
    pub inlay_hint: Option<StyleValue>,
    #[serde(default)]
    pub ruler: Option<StyleValue>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BrowserTheme {
    pub directory: StyleValue,
    pub file: StyleValue,
    pub selected: StyleValue,
    pub selected_unfocused: StyleValue,
    pub border: StyleValue,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileSearchTheme {
    pub border: StyleValue,
    pub input: StyleValue,
    pub input_unfocused: StyleValue,
    pub toggle_on: StyleValue,
    pub toggle_off: StyleValue,
    pub file_header: StyleValue,
    pub hit: StyleValue,
    #[serde(rename = "match")]
    pub match_: StyleValue,
    pub selected: StyleValue,
    pub selected_unfocused: StyleValue,
    pub search_current: StyleValue,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiagnosticsTheme {
    pub error: StyleValue,
    pub warning: StyleValue,
    pub info: StyleValue,
    pub hint: StyleValue,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitTheme {
    pub modified: StyleValue,
    pub added: StyleValue,
    pub untracked: StyleValue,
    pub gutter_added: StyleValue,
    pub gutter_modified: StyleValue,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BracketsTheme {
    #[serde(rename = "match")]
    pub match_: StyleValue,
    pub rainbow_0: StyleValue,
    pub rainbow_1: StyleValue,
    pub rainbow_2: StyleValue,
    pub rainbow_3: StyleValue,
    pub rainbow_4: StyleValue,
    pub rainbow_5: StyleValue,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Theme {
    #[serde(rename = "COLORS")]
    pub colors: HashMap<String, StyleValue>,
    pub tabs: TabsTheme,
    pub status_bar: StatusBarTheme,
    pub editor: EditorTheme,
    pub browser: BrowserTheme,
    pub file_search: FileSearchTheme,
    pub diagnostics: DiagnosticsTheme,
    pub git: GitTheme,
    pub brackets: BracketsTheme,
    pub syntax: HashMap<String, StyleValue>,
}
