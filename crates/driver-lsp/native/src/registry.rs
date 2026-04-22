//! Language → server binary mapping.
//!
//! Pure lookup table, same entries as legacy
//! `crates/lsp/src/registry.rs` on main. Each entry names the
//! binary, its CLI args, and the file extensions the server
//! claims so the runtime can cross-check against the `Language`
//! that drove the spawn.
//!
//! A test-only `server_override` replaces the binary for EVERY
//! language — used by the goldens harness to point every
//! language at `fake-lsp`.

use led_state_syntax::Language;

/// One entry: language, binary, args, extensions. `&'static str`
/// because the table is compile-time; overrides are `Box::leak`ed
/// to keep the same type at runtime (only done once, at startup).
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub language: Language,
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub extensions: &'static [&'static str],
}

/// Registry of known language servers. Clone-cheap (it's a small
/// `Vec` of `ServerConfig`); hold one in the manager thread.
#[derive(Clone)]
pub struct LspRegistry {
    configs: Vec<ServerConfig>,
}

impl LspRegistry {
    /// Build the default registry.
    ///
    /// `server_override`, when set, replaces the `command` on
    /// EVERY entry (and clears `args`) — the goldens runner
    /// points this at the `fake-lsp` binary so one server
    /// services every language under test.
    pub fn new(server_override: Option<String>) -> Self {
        let mut registry = Self {
            configs: vec![
                ServerConfig {
                    language: Language::Rust,
                    command: "rust-analyzer",
                    args: &[],
                    extensions: &["rs"],
                },
                ServerConfig {
                    language: Language::TypeScript,
                    command: "typescript-language-server",
                    args: &["--stdio"],
                    extensions: &["ts", "tsx"],
                },
                ServerConfig {
                    language: Language::JavaScript,
                    command: "typescript-language-server",
                    args: &["--stdio"],
                    extensions: &["js", "mjs", "cjs", "jsx"],
                },
                ServerConfig {
                    language: Language::Python,
                    command: "pyright-langserver",
                    args: &["--stdio"],
                    extensions: &["py", "pyi"],
                },
                ServerConfig {
                    language: Language::C,
                    command: "clangd",
                    args: &[],
                    extensions: &["c", "h"],
                },
                ServerConfig {
                    language: Language::Cpp,
                    command: "clangd",
                    args: &[],
                    extensions: &["cpp", "hpp", "cc", "cxx", "hh"],
                },
                ServerConfig {
                    language: Language::Swift,
                    command: "sourcekit-lsp",
                    args: &[],
                    extensions: &["swift"],
                },
                ServerConfig {
                    language: Language::Toml,
                    command: "taplo",
                    args: &["lsp", "stdio"],
                    extensions: &["toml"],
                },
                ServerConfig {
                    language: Language::Json,
                    command: "vscode-json-language-server",
                    args: &["--stdio"],
                    extensions: &["json"],
                },
                ServerConfig {
                    language: Language::Bash,
                    command: "bash-language-server",
                    args: &["start"],
                    extensions: &["sh", "bash", "zsh"],
                },
            ],
        };

        if let Some(cmd) = server_override {
            // Leak once at startup — the registry's entries are
            // expected to outlive the program.
            let cmd: &'static str = Box::leak(cmd.into_boxed_str());
            for config in &mut registry.configs {
                config.command = cmd;
                config.args = &[];
            }
        }

        registry
    }

    /// Look up the config for a language. `None` when the
    /// language has no known server (e.g. `Markdown`, `Make`,
    /// `Ruby` — the rewrite's `Language` enum is broader than
    /// the LSP-servered subset).
    pub fn config_for(&self, language: Language) -> Option<&ServerConfig> {
        self.configs.iter().find(|c| c.language == language)
    }

    /// All extensions known to any registered server. Used by
    /// the runtime when it decides whether a buffer needs
    /// `BufferOpened` sent to the LSP driver at all.
    pub fn all_extensions(&self) -> Vec<&'static str> {
        self.configs
            .iter()
            .flat_map(|c| c.extensions.iter().copied())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_has_rust_analyzer() {
        let r = LspRegistry::new(None);
        let c = r.config_for(Language::Rust).expect("rust entry");
        assert_eq!(c.command, "rust-analyzer");
        assert!(c.args.is_empty());
        assert_eq!(c.extensions, &["rs"]);
    }

    #[test]
    fn typescript_and_javascript_point_at_same_server() {
        let r = LspRegistry::new(None);
        let ts = r.config_for(Language::TypeScript).unwrap();
        let js = r.config_for(Language::JavaScript).unwrap();
        assert_eq!(ts.command, js.command);
        assert_eq!(ts.args, js.args);
        // But different extension sets — TS owns tsx, JS owns jsx.
        assert!(ts.extensions.contains(&"ts"));
        assert!(js.extensions.contains(&"js"));
    }

    #[test]
    fn languages_without_entries_return_none() {
        let r = LspRegistry::new(None);
        assert!(r.config_for(Language::Markdown).is_none());
        assert!(r.config_for(Language::Make).is_none());
        assert!(r.config_for(Language::Ruby).is_none());
    }

    #[test]
    fn server_override_rewrites_every_command_and_clears_args() {
        let r = LspRegistry::new(Some("/tmp/fake-lsp".into()));
        for lang in [
            Language::Rust,
            Language::TypeScript,
            Language::Python,
            Language::C,
            Language::Cpp,
            Language::Swift,
            Language::Toml,
            Language::Json,
            Language::Bash,
        ] {
            let c = r.config_for(lang).unwrap();
            assert_eq!(c.command, "/tmp/fake-lsp", "{:?}", lang);
            assert!(c.args.is_empty(), "{:?}", lang);
        }
    }

    #[test]
    fn all_extensions_covers_every_mapped_language() {
        let r = LspRegistry::new(None);
        let exts = r.all_extensions();
        for needed in ["rs", "ts", "js", "py", "c", "cpp", "swift", "toml", "json", "sh"]
        {
            assert!(
                exts.contains(&needed),
                "missing extension {} in {:?}",
                needed,
                exts
            );
        }
    }

    #[test]
    fn c_and_cpp_share_the_clangd_binary() {
        // clangd handles both but legacy led split them so each
        // language has an unambiguous extension list. Exercise
        // the split: c-lang owns .c/.h, cpp-lang owns .cpp/.hpp/etc.
        let r = LspRegistry::new(None);
        let c = r.config_for(Language::C).unwrap();
        let cpp = r.config_for(Language::Cpp).unwrap();
        assert_eq!(c.command, "clangd");
        assert_eq!(cpp.command, "clangd");
        assert!(c.extensions.contains(&"c"));
        assert!(cpp.extensions.contains(&"cpp"));
        assert!(!c.extensions.contains(&"cpp"));
    }
}
