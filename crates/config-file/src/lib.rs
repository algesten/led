use std::path::PathBuf;
use std::sync::Arc;

use led_core::keys::Keys;
use led_core::theme::Theme;
use led_core::{AStream, Alert, AlertExt, FanoutStreamExt, StreamOpsExt, watch};
use tokio::fs;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigFileOut {
    ConfigDir(ConfigDir),
    Persist,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigDir {
    pub config: PathBuf,
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

pub trait TomlFile: serde::de::DeserializeOwned + Send + 'static {
    fn default_toml() -> &'static str;
    fn file_name() -> &'static str;
}

// ============================================================================
// Driver implementation
// ============================================================================

pub fn driver<F: TomlFile>(
    out: impl AStream<ConfigFileOut>,
) -> impl AStream<Result<ConfigFile<F>, Alert>> {
    let out = out.broadcast();

    // All ConfigDir
    let config_s = out
        .latest()
        .filter_map(|o| match o {
            ConfigFileOut::ConfigDir(v) => Some(v),
            _ => None,
        })
        .broadcast();

    // Watcher for ConfigDir
    let watch_s = config_s
        .latest()
        .map(|c| watch(&c.config))
        .map(ReceiverStream::new)
        .flatten()
        .map(|_| ());

    let new_s = config_s.latest().map(|_| ());

    // Trigger for re-reading config
    let trig_s =
        // watcher or new incoming file
        watch_s.or(new_s);

    // On trigger, read the config
    let read_s = trig_s
        // Whatever the config_s is when the trigger comes
        .sample_combine(config_s.latest())
        .map(|(_, c)| c)
        .then(read_file);

    read_s
}

async fn read_file<F: TomlFile>(c: ConfigDir) -> Result<ConfigFile<F>, Alert> {
    let file_path = c.config.join(F::file_name());

    let toml = fs::read_to_string(&file_path)
        .await
        .unwrap_or_else(|_| F::default_toml().to_string());

    // Report error in parsing as info since the user might have screwed up the format
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
