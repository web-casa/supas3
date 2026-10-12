// SPDX-License-Identifier: AGPL-3.0-or-later
//! Interface preferences that persist between runs.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "interface.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    /// Follow the system language.
    #[default]
    System,
    English,
    SimplifiedChinese,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    /// Follow the system's light or dark setting.
    #[default]
    System,
    Light,
    Dark,
}

/// Preferences saved in the configuration directory. Unknown or missing
/// values fall back to following the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub language: Language,
    pub appearance: Appearance,
}

impl Settings {
    fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(FILE_NAME)
    }

    /// Reads saved preferences. A missing or unreadable file means defaults:
    /// preferences are a convenience and must never stop the app from opening.
    pub fn load(config_dir: &Path) -> Self {
        std::fs::read_to_string(Self::path(config_dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, config_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(config_dir)?;
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        // Write beside the file and rename over it, so an interrupted write
        // cannot leave a half-written file that silently resets preferences.
        let path = Self::path(config_dir);
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, &path)
    }
}

#[cfg(test)]
mod tests {
    use super::{Appearance, Language, Settings};

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("supas3-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn saved_preferences_are_read_back() {
        let dir = test_dir("roundtrip");
        let settings = Settings {
            language: Language::SimplifiedChinese,
            appearance: Appearance::Dark,
        };
        settings.save(&dir).unwrap();
        assert_eq!(Settings::load(&dir), settings);
        // Saving again replaces the file and leaves no temporary behind.
        Settings::default().save(&dir).unwrap();
        assert_eq!(Settings::load(&dir), Settings::default());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_damaged_or_partial_files_fall_back_to_the_system() {
        let dir = test_dir("fallback");
        assert_eq!(Settings::load(&dir), Settings::default());

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("interface.json"), "{not json").unwrap();
        assert_eq!(Settings::load(&dir), Settings::default());

        std::fs::write(dir.join("interface.json"), r#"{"language":"english"}"#).unwrap();
        assert_eq!(
            Settings::load(&dir),
            Settings {
                language: Language::English,
                appearance: Appearance::System,
            }
        );

        std::fs::write(dir.join("interface.json"), r#"{"language":"klingon"}"#).unwrap();
        assert_eq!(Settings::load(&dir), Settings::default());
        let _ = std::fs::remove_dir_all(dir);
    }
}
