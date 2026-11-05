use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Record new clipboard contents into the history.
    pub history_enabled: bool,
    /// Oldest entries beyond this count are deleted.
    pub max_entries: u32,
    /// Open the popup with the preview pane visible.
    pub show_preview: bool,
    /// When the app that owns the clipboard exits, re-offer the newest entry.
    pub keep_clipboard: bool,
    /// Clipboard contents larger than this are not recorded.
    pub max_item_mb: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            history_enabled: true,
            max_entries: 500,
            show_preview: true,
            keep_clipboard: true,
            max_item_mb: 64,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        crate::config_dir().join("config.toml")
    }

    /// Loads the config, falling back to defaults if it is missing or invalid.
    pub fn load() -> Self {
        let Ok(text) = std::fs::read_to_string(Self::path()) else {
            return Self::default();
        };
        toml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("wisp: ignoring invalid {}: {e}", Self::path().display());
            Self::default()
        })
    }

    pub fn save(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(crate::config_dir())?;
        std::fs::write(Self::path(), toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn max_item_bytes(&self) -> usize {
        self.max_item_mb as usize * 1024 * 1024
    }
}
