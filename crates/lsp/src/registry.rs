pub struct ServerConfig {
    pub language_id: String,
    pub command: String,
    pub args: Vec<String>,
    pub extensions: Vec<String>,
}

pub struct LspRegistry {
    configs: Vec<ServerConfig>,
}

impl LspRegistry {
    pub fn new() -> Self {
        Self {
            configs: vec![
                ServerConfig {
                    language_id: "rust".into(),
                    command: "rust-analyzer".into(),
                    args: vec![],
                    extensions: vec!["rs".into()],
                },
                ServerConfig {
                    language_id: "typescript".into(),
                    command: "typescript-language-server".into(),
                    args: vec!["--stdio".into()],
                    extensions: vec!["ts".into(), "tsx".into(), "js".into(), "jsx".into()],
                },
                ServerConfig {
                    language_id: "python".into(),
                    command: "pyright-langserver".into(),
                    args: vec!["--stdio".into()],
                    extensions: vec!["py".into()],
                },
                ServerConfig {
                    language_id: "c".into(),
                    command: "clangd".into(),
                    args: vec![],
                    extensions: vec!["c".into(), "cpp".into(), "h".into()],
                },
                ServerConfig {
                    language_id: "swift".into(),
                    command: "sourcekit-lsp".into(),
                    args: vec![],
                    extensions: vec!["swift".into()],
                },
            ],
        }
    }

    pub fn config_for_extension(&self, ext: &str) -> Option<&ServerConfig> {
        self.configs
            .iter()
            .find(|c| c.extensions.iter().any(|e| e == ext))
    }
}
