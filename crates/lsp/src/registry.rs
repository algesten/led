#[derive(Clone)]
pub(crate) struct ServerConfig {
    pub(crate) language_id: &'static str,
    pub(crate) command: &'static str,
    pub(crate) args: &'static [&'static str],
    pub(crate) extensions: &'static [&'static str],
}

pub(crate) struct LspRegistry {
    configs: Vec<ServerConfig>,
}

impl LspRegistry {
    pub(crate) fn new() -> Self {
        Self {
            configs: vec![
                ServerConfig {
                    language_id: "rust",
                    command: "rust-analyzer",
                    args: &[],
                    extensions: &["rs"],
                },
                ServerConfig {
                    language_id: "typescript",
                    command: "typescript-language-server",
                    args: &["--stdio"],
                    extensions: &["ts", "tsx", "js", "jsx"],
                },
                ServerConfig {
                    language_id: "python",
                    command: "pyright-langserver",
                    args: &["--stdio"],
                    extensions: &["py"],
                },
                ServerConfig {
                    language_id: "c",
                    command: "clangd",
                    args: &[],
                    extensions: &["c", "h", "cpp", "hpp", "cc", "cxx"],
                },
                ServerConfig {
                    language_id: "swift",
                    command: "sourcekit-lsp",
                    args: &[],
                    extensions: &["swift"],
                },
                ServerConfig {
                    language_id: "toml",
                    command: "taplo",
                    args: &["lsp", "stdio"],
                    extensions: &["toml"],
                },
                ServerConfig {
                    language_id: "json",
                    command: "vscode-json-language-server",
                    args: &["--stdio"],
                    extensions: &["json"],
                },
                ServerConfig {
                    language_id: "bash",
                    command: "bash-language-server",
                    args: &["start"],
                    extensions: &["sh", "bash"],
                },
            ],
        }
    }

    pub(crate) fn config_for_extension(&self, ext: &str) -> Option<&ServerConfig> {
        self.configs.iter().find(|c| c.extensions.contains(&ext))
    }
}
