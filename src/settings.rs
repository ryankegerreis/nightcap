//! Persistent timer preferences in the operating system's configuration directory.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub dim: FadeSettings,
    pub volume: FadeSettings,
    pub end_action: EndAction,
    pub restore_on_cancel_or_wake: bool,
    pub show_time_in_menu_bar: bool,
    pub last_duration_min: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FadeSettings {
    pub enabled: bool,
    pub start_before_min: u32,
    pub over_min: u32,
    pub target_percent: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndAction {
    #[default]
    Sleep,
    Hibernate,
    Shutdown,
}

impl Default for FadeSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            start_before_min: 20,
            over_min: 15,
            target_percent: 10,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dim: FadeSettings::default(),
            volume: FadeSettings {
                enabled: true,
                start_before_min: 15,
                over_min: 10,
                target_percent: 20,
            },
            end_action: EndAction::default(),
            restore_on_cancel_or_wake: true,
            show_time_in_menu_bar: true,
            last_duration_min: 45,
        }
    }
}

impl Settings {
    pub fn config_path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("com", "Nightcap", "Nightcap")
            .ok_or_else(|| anyhow!("could not find the Nightcap configuration directory"))?;
        Ok(dirs.config_dir().join("settings.json"))
    }

    pub fn load() -> Self {
        match Self::config_path() {
            Ok(path) => Self::load_from(&path),
            Err(error) => {
                log::warn!("Using default settings: {error:#}");
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::config_path()?)
    }

    fn load_from(path: &Path) -> Self {
        let result = fs::read(path)
            .with_context(|| format!("reading {}", path.display()))
            .and_then(|bytes| serde_json::from_slice(&bytes).context("parsing settings JSON"));
        match result {
            Ok(settings) => settings,
            Err(error) => {
                log::warn!("Using default settings: {error:#}");
                Self::default()
            }
        }
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("settings path has no parent directory")?;
        fs::create_dir_all(parent).context("creating settings directory")?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).context("creating temporary settings file")?;
        serde_json::to_writer_pretty(&mut temporary, self).context("serializing settings")?;
        temporary.write_all(b"\n")?;
        temporary
            .as_file()
            .sync_all()
            .context("flushing settings")?;
        temporary.persist(path).context("replacing settings file")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_timer_preferences() {
        let settings = Settings::default();
        assert_eq!(
            settings.dim,
            FadeSettings {
                enabled: true,
                start_before_min: 20,
                over_min: 15,
                target_percent: 10
            }
        );
        assert_eq!(
            settings.volume,
            FadeSettings {
                enabled: true,
                start_before_min: 15,
                over_min: 10,
                target_percent: 20
            }
        );
        assert_eq!(settings.end_action, EndAction::Sleep);
        assert!(settings.restore_on_cancel_or_wake);
        assert!(settings.show_time_in_menu_bar);
        assert_eq!(settings.last_duration_min, 45);
    }

    #[test]
    fn settings_roundtrip_and_replace_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/settings.json");
        let mut settings = Settings::default();
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), settings);
        settings.end_action = EndAction::Hibernate;
        settings.dim.enabled = false;
        settings.last_duration_min = 90;
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), settings);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn missing_and_corrupt_files_use_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(Settings::load_from(&path), Settings::default());
        std::fs::write(&path, "broken json").unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
        std::fs::write(&path, r#"{"last_duration_min": 60}"#).unwrap();
        assert_eq!(Settings::load_from(&path).last_duration_min, 60);
    }
}
