use led_core::LanguageId;

#[derive(Clone)]
pub(crate) struct ServerConfig {
    pub(crate) language: LanguageId,
    pub(crate) command: &'static str,
    pub(crate) args: &'static [&'static str],
    pub(crate) extensions: &'static [&'static str],
}

pub(crate) struct LspRegistry {
    configs: Vec<ServerConfig>,
}

impl LspRegistry {
    pub(crate) fn new(server_override: Option<String>) -> Self {
        let mut registry = Self {
            configs: vec![
                ServerConfig {
                    language: LanguageId::Rust,
                    command: "rust-analyzer",
                    args: &[],
                    extensions: &["rs"],
                },
                ServerConfig {
                    language: LanguageId::TypeScript,
                    command: "typescript-language-server",
                    args: &["--stdio"],
                    extensions: &["ts", "tsx", "js", "jsx"],
                },
                ServerConfig {
                    language: LanguageId::Python,
                    command: "pyright-langserver",
                    args: &["--stdio"],
                    extensions: &["py"],
                },
                ServerConfig {
                    language: LanguageId::C,
                    command: "clangd",
                    args: &[],
                    extensions: &["c", "h", "cpp", "hpp", "cc", "cxx"],
                },
                ServerConfig {
                    language: LanguageId::Swift,
                    command: "sourcekit-lsp",
                    args: &[],
                    extensions: &["swift"],
                },
                ServerConfig {
                    language: LanguageId::Toml,
                    command: "taplo",
                    args: &["lsp", "stdio"],
                    extensions: &["toml"],
                },
                ServerConfig {
                    language: LanguageId::Json,
                    command: "vscode-json-language-server",
                    args: &["--stdio"],
                    extensions: &["json"],
                },
                ServerConfig {
                    language: LanguageId::Bash,
                    command: "bash-language-server",
                    args: &["start"],
                    extensions: &["sh", "bash"],
                },
            ],
        };

        if let Some(cmd) = server_override {
            let cmd: &'static str = Box::leak(cmd.into_boxed_str());
            for config in &mut registry.configs {
                config.command = cmd;
                config.args = &[];
            }
        }

        registry
    }

    pub(crate) fn config_for_extension(&self, ext: &str) -> Option<&ServerConfig> {
        self.configs.iter().find(|c| c.extensions.contains(&ext))
    }

    pub(crate) fn extensions_for_language(&self, language: LanguageId) -> Vec<String> {
        self.configs
            .iter()
            .find(|c| c.language == language)
            .map(|c| c.extensions.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default()
    }
}
