use std::sync::Arc;

use led_core::keys::Keys;
use led_core::rx::Stream;
use led_core::theme::Theme;
use led_core::{Alert, AlertExt, UserPath};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigFileOut {
    ConfigDir(ConfigDir),
    Persist,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigDir {
    pub config: UserPath,
    pub read_only: bool,
}

#[derive(Debug, Clone)]
pub struct ConfigFile<File: TomlFile> {
    pub file: Arc<File>,
}

impl<File: TomlFile + PartialEq> PartialEq for ConfigFile<File> {
    fn eq(&self, other: &Self) -> bool {
        self.file == other.file
    }
}

pub trait TomlFile: serde::de::DeserializeOwned + Send + Sync + 'static {
    fn default_toml() -> &'static str;
    fn file_name() -> &'static str;
}

/// Start a config-file driver. Takes a stream of commands, returns a stream of results.
pub fn driver<F: TomlFile>(out: Stream<ConfigFileOut>) -> Stream<Result<ConfigFile<F>, Alert>> {
    let stream: Stream<Result<ConfigFile<F>, Alert>> = Stream::new();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<ConfigFileOut>(64);
    let (result_tx, mut result_rx) = mpsc::channel::<Result<ConfigFile<F>, Alert>>(64);

    // Bridge out: rx::Stream → channel
    out.on(move |opt: Option<&ConfigFileOut>| {
        if let Some(cmd) = opt {
            cmd_tx.try_send(cmd.clone()).ok();
        }
    });

    // Async driver task
    tokio::task::spawn_local(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                ConfigFileOut::ConfigDir(dir) => {
                    read_and_send::<F>(&dir, &result_tx).await;
                }
                ConfigFileOut::Persist => {}
            }
        }
    });

    // Bridge in: channel → rx::Stream
    let s = stream.clone();
    tokio::task::spawn_local(async move {
        while let Some(v) = result_rx.recv().await {
            s.push(v);
        }
    });

    stream
}

async fn read_and_send<F: TomlFile>(
    dir: &ConfigDir,
    tx: &mpsc::Sender<Result<ConfigFile<F>, Alert>>,
) {
    let result = read_file::<F>(dir);
    let _ = tx.send(result).await;
}

fn read_file<F: TomlFile>(c: &ConfigDir) -> Result<ConfigFile<F>, Alert> {
    let file_path = c.config.join(F::file_name());

    let toml =
        std::fs::read_to_string(&file_path).unwrap_or_else(|_| F::default_toml().to_string());

    let file: F = toml::from_str(&toml).as_info()?;

    Ok(ConfigFile {
        file: Arc::new(file),
    })
}

impl TomlFile for Theme {
    fn default_toml() -> &'static str {
        include_str!("default_theme.toml")
    }

    fn file_name() -> &'static str {
        "theme.toml"
    }
}

impl TomlFile for Keys {
    fn default_toml() -> &'static str {
        include_str!("default_keys.toml")
    }

    fn file_name() -> &'static str {
        "keys.toml"
    }
}
