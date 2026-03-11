// ============================================================================
// Keys types
// ============================================================================

use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum KeyBinding {
    Action(String),
    Chord(HashMap<String, String>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct Keys {
    #[serde(default)]
    pub keys: HashMap<String, KeyBinding>,
    #[serde(default)]
    pub browser: HashMap<String, String>,
    #[serde(default)]
    pub file_search: HashMap<String, String>,
}
