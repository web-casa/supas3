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
    /// The sidebar shows only icons.
    pub sidebar_collapsed: bool,
    /// The details panel is shown beside the panes.
    pub details_open: bool,
    /// A value of the wrong type here resets only these limits, not every
    /// preference.
    #[serde(deserialize_with = "transfers_or_default")]
    pub transfers: TransferSettings,
}

fn transfers_or_default<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<TransferSettings, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// How many transfers run at once, as the backend allows.
pub const CONCURRENCY: std::ops::RangeInclusive<u32> = 1..=20;
/// A limit of one transfer's speed in KiB/s, as the backend allows.
pub const BANDWIDTH_KIB: std::ops::RangeInclusive<u64> = 64..=1_048_576;

/// Limits applied to transfers queued from now on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferSettings {
    /// Transfers running at once.
    pub concurrency: u32,
    /// Speed of each transfer in KiB/s; 0 is unlimited.
    pub bandwidth_kib: u64,
}

impl Default for TransferSettings {
    fn default() -> Self {
        Self {
            concurrency: 5,
            bandwidth_kib: 0,
        }
    }
}

/// Why a transfer setting was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferProblem {
    Concurrency,
    Bandwidth,
}

impl TransferProblem {
    pub fn message_key(self) -> &'static str {
        match self {
            Self::Concurrency => "transfers.concurrency_invalid",
            Self::Bandwidth => "transfers.bandwidth_invalid",
        }
    }
}

impl TransferSettings {
    /// The settings with values the backend would refuse replaced by the
    /// defaults, for a file edited by hand.
    pub fn sanitized(self) -> Self {
        let default = Self::default();
        Self {
            concurrency: if CONCURRENCY.contains(&self.concurrency) {
                self.concurrency
            } else {
                default.concurrency
            },
            bandwidth_kib: if self.bandwidth_kib == 0 || BANDWIDTH_KIB.contains(&self.bandwidth_kib)
            {
                self.bandwidth_kib
            } else {
                default.bandwidth_kib
            },
        }
    }

    /// The speed limit in bytes per second, as the backend takes it.
    pub fn bandwidth_bytes(&self) -> u64 {
        self.bandwidth_kib * 1024
    }

    /// Reads what the user typed: a whole number of transfers, and a speed
    /// in MiB/s that may have decimals, empty for unlimited.
    pub fn parse(concurrency: &str, bandwidth_mib: &str) -> Result<Self, TransferProblem> {
        let concurrency = concurrency
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|value| CONCURRENCY.contains(value))
            .ok_or(TransferProblem::Concurrency)?;
        // A decimal comma is read as a point.
        let bandwidth = bandwidth_mib.trim().replace(',', ".");
        let bandwidth_kib = if bandwidth.is_empty() {
            0
        } else {
            bandwidth
                .parse::<f64>()
                .ok()
                .filter(|mib| mib.is_finite() && *mib > 0.0)
                .map(|mib| (mib * 1024.0).round() as u64)
                .filter(|kib| BANDWIDTH_KIB.contains(kib))
                .ok_or(TransferProblem::Bandwidth)?
        };
        Ok(Self {
            concurrency,
            bandwidth_kib,
        })
    }

    /// The speed limit as typed in MiB/s: empty for unlimited, otherwise
    /// without trailing zeros.
    pub fn bandwidth_text(&self) -> String {
        if self.bandwidth_kib == 0 {
            return String::new();
        }
        let text = format!("{:.4}", self.bandwidth_kib as f64 / 1024.0);
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    }
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
            .and_then(|text| serde_json::from_str::<Self>(&text).ok())
            .map(|settings| Self {
                transfers: settings.transfers.sanitized(),
                ..settings
            })
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
    use super::{Appearance, Language, Settings, TransferProblem, TransferSettings};

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
            sidebar_collapsed: true,
            details_open: true,
            transfers: TransferSettings {
                concurrency: 3,
                bandwidth_kib: 2048,
            },
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
                ..Settings::default()
            }
        );

        std::fs::write(dir.join("interface.json"), r#"{"language":"klingon"}"#).unwrap();
        assert_eq!(Settings::load(&dir), Settings::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn transfer_limits_are_read_as_typed() {
        let parsed = TransferSettings::parse(" 8 ", "").unwrap();
        assert_eq!(parsed.concurrency, 8);
        assert_eq!(parsed.bandwidth_bytes(), 0);
        let parsed = TransferSettings::parse("1", "2.5").unwrap();
        assert_eq!(parsed.bandwidth_kib, 2560);
        assert_eq!(parsed.bandwidth_bytes(), 2560 * 1024);
        assert_eq!(parsed.bandwidth_text(), "2.5");
        assert_eq!(
            TransferSettings::parse("20", "1024")
                .unwrap()
                .bandwidth_text(),
            "1024"
        );
        assert_eq!(
            TransferSettings::parse("1", "0.0625")
                .unwrap()
                .bandwidth_kib,
            64
        );
        assert_eq!(
            TransferSettings::parse("1", "2,5").unwrap().bandwidth_kib,
            2560
        );

        for concurrency in ["0", "21", "-1", "two", ""] {
            assert_eq!(
                TransferSettings::parse(concurrency, ""),
                Err(TransferProblem::Concurrency)
            );
        }
        for bandwidth in ["0", "0.05", "1025", "-3", "fast", "NaN", "inf"] {
            assert_eq!(
                TransferSettings::parse("5", bandwidth),
                Err(TransferProblem::Bandwidth)
            );
        }
    }

    #[test]
    fn hand_edited_transfer_limits_out_of_range_fall_back() {
        let dir = test_dir("transfers");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("interface.json"),
            r#"{"language":"english","transfers":{"concurrency":99,"bandwidth_kib":1}}"#,
        )
        .unwrap();
        let settings = Settings::load(&dir);
        assert_eq!(settings.language, Language::English);
        assert_eq!(settings.transfers, TransferSettings::default());

        // A value of the wrong type costs only the transfer limits.
        std::fs::write(
            dir.join("interface.json"),
            r#"{"language":"english","transfers":{"concurrency":"5"}}"#,
        )
        .unwrap();
        let settings = Settings::load(&dir);
        assert_eq!(settings.language, Language::English);
        assert_eq!(settings.transfers, TransferSettings::default());

        std::fs::write(
            dir.join("interface.json"),
            r#"{"transfers":{"concurrency":2}}"#,
        )
        .unwrap();
        assert_eq!(
            Settings::load(&dir).transfers,
            TransferSettings {
                concurrency: 2,
                bandwidth_kib: 0
            }
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
