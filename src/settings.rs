//! Persistent timer preferences in the operating system's configuration directory.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;

pub const MIN_DURATION_MIN: u32 = 1;
pub const MAX_DURATION_MIN: u32 = 720;
pub const BEFORE_MINUTES: [u32; 7] = [5, 10, 15, 20, 30, 45, 60];
pub const OVER_MINUTES: [u32; 8] = [1, 5, 10, 15, 20, 30, 45, 60];
pub const MAX_BEFORE_MIN: u32 = BEFORE_MINUTES[BEFORE_MINUTES.len() - 1];
pub const MAX_OVER_MIN: u32 = OVER_MINUTES[OVER_MINUTES.len() - 1];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub dim: FadeSettings,
    pub volume: FadeSettings,
    pub end_action: EndAction,
    pub restore_on_cancel_or_wake: bool,
    pub show_time_in_menu_bar: bool,
    pub last_duration_min: u32,
    /// Ask GitHub about once a day whether a newer release exists.
    pub check_for_updates: bool,
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
            check_for_updates: true,
        }
    }
}

impl Settings {
    /// Keep the selected action consistent with this machine's startup capabilities.
    pub fn apply_hibernate_availability(&mut self, supported: bool) -> bool {
        if self.end_action == EndAction::Hibernate && !supported {
            self.end_action = EndAction::Sleep;
            return true;
        }
        false
    }

    fn normalize(&mut self) {
        for fade in [&mut self.dim, &mut self.volume] {
            fade.target_percent = fade.target_percent.min(100);
            // BEFORE_MINUTES starts at 5; OVER_MINUTES starts at 1. Both top out at 60.
            fade.start_before_min = fade
                .start_before_min
                .clamp(BEFORE_MINUTES[0], MAX_BEFORE_MIN);
            fade.over_min = fade.over_min.clamp(OVER_MINUTES[0], MAX_OVER_MIN);
        }
        self.last_duration_min = self
            .last_duration_min
            .clamp(MIN_DURATION_MIN, MAX_DURATION_MIN);
    }

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
            .and_then(|bytes| {
                // Windows editors and PowerShell 5's UTF-8 output start with a BOM,
                // which serde_json rejects; that would reset and overwrite every setting.
                let json = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
                Self::from_json(json)
            });
        match result {
            Ok(mut settings) => {
                settings.normalize();
                settings
            }
            Err(error) => {
                log::warn!("Using default settings: {error:#}");
                Self::default()
            }
        }
    }

    /// The typed schema decides what a valid file means. When it rejects the file,
    /// each preference is read on its own: a missing or invalid value keeps its
    /// default, so one bad field (a hand edit, or a value a newer Nightcap wrote)
    /// cannot reset every other preference, which the next save would then overwrite.
    /// Raw values are left unparsed until read, so even a number no type can hold
    /// (`1e400`) only affects its own field, or none if Nightcap does not know it.
    fn from_json(json: &[u8]) -> Result<Self> {
        let error = match serde_json::from_slice::<Self>(json) {
            Ok(settings) => return Ok(settings),
            Err(error) => error,
        };
        let fields: Fields<'_> = serde_json::from_slice(json)
            .with_context(|| format!("parsing settings JSON ({error})"))?;
        log::warn!("Recovering settings one by one: {error}");
        let defaults = Self::default();
        Ok(Self {
            dim: fade(&fields, "dim", defaults.dim),
            volume: fade(&fields, "volume", defaults.volume),
            end_action: field(&fields, "end_action", defaults.end_action),
            restore_on_cancel_or_wake: field(
                &fields,
                "restore_on_cancel_or_wake",
                defaults.restore_on_cancel_or_wake,
            ),
            show_time_in_menu_bar: field(
                &fields,
                "show_time_in_menu_bar",
                defaults.show_time_in_menu_bar,
            ),
            last_duration_min: field(&fields, "last_duration_min", defaults.last_duration_min),
            check_for_updates: field(&fields, "check_for_updates", defaults.check_for_updates),
        })
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

/// An object's members, each still unparsed JSON.
type Fields<'a> = std::collections::BTreeMap<String, &'a RawValue>;

fn field<T: DeserializeOwned>(fields: &Fields<'_>, name: &str, default: T) -> T {
    read(fields.get(name).copied(), name, default)
}

/// `label` names the value in the log, e.g. `dim.target_percent`.
fn read<T: DeserializeOwned>(value: Option<&RawValue>, label: &str, default: T) -> T {
    match value {
        None => default,
        Some(value) => serde_json::from_str(value.get()).unwrap_or_else(|error| {
            log::warn!("Ignoring the saved {label} setting: {error}");
            default
        }),
    }
}

/// A fade the typed schema accepts loads whole; otherwise each of its fields falls
/// back to that fade's own default, not to `FadeSettings::default`.
fn fade(fields: &Fields<'_>, name: &str, default: FadeSettings) -> FadeSettings {
    let Some(raw) = fields.get(name) else {
        return default;
    };
    if let Ok(fade) = serde_json::from_str(raw.get()) {
        return fade;
    }
    let fade: Fields<'_> = match serde_json::from_str(raw.get()) {
        Ok(fade) => fade,
        Err(error) => {
            log::warn!("Ignoring the saved {name} settings: {error}");
            return default;
        }
    };
    let value = |key: &str| (fade.get(key).copied(), format!("{name}.{key}"));
    let (enabled, label) = value("enabled");
    let enabled = read(enabled, &label, default.enabled);
    let (before, label) = value("start_before_min");
    let start_before_min = read(before, &label, default.start_before_min);
    let (over, label) = value("over_min");
    let over_min = read(over, &label, default.over_min);
    let (target, label) = value("target_percent");
    let target_percent = read(target, &label, default.target_percent);
    FadeSettings {
        enabled,
        start_before_min,
        over_min,
        target_percent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loaded_settings_are_normalized_to_ui_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = Settings {
            dim: FadeSettings {
                enabled: true,
                start_before_min: 0,
                over_min: 0,
                target_percent: 255,
            },
            volume: FadeSettings {
                enabled: false,
                start_before_min: u32::MAX,
                over_min: u32::MAX,
                target_percent: 200,
            },
            last_duration_min: u32::MAX,
            ..Settings::default()
        };
        settings.save_to(&path).unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.dim.start_before_min, BEFORE_MINUTES[0]);
        assert_eq!(loaded.dim.over_min, OVER_MINUTES[0]);
        assert_eq!(loaded.dim.target_percent, 100);
        assert_eq!(loaded.volume.start_before_min, 60);
        assert_eq!(loaded.volume.over_min, 60);
        assert_eq!(loaded.volume.target_percent, 100);
        assert_eq!(loaded.last_duration_min, 720);
        let settings = Settings {
            dim: FadeSettings {
                start_before_min: 7,
                over_min: 1,
                target_percent: 0,
                ..FadeSettings::default()
            },
            last_duration_min: 0,
            ..Settings::default()
        };
        settings.save_to(&path).unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.last_duration_min, MIN_DURATION_MIN);
        assert_eq!(loaded.dim.start_before_min, 7);
        assert_eq!(loaded.dim.over_min, 1);
        assert_eq!(loaded.dim.target_percent, 0);
    }

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
        assert!(settings.check_for_updates);
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
        settings.check_for_updates = false;
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
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.last_duration_min, 60);
        // Files saved before update checks existed turn them on.
        assert!(loaded.check_for_updates);
    }

    #[test]
    fn every_field_survives_a_save_and_load() {
        // Every value differs from its default, so a field `from_json` forgot fails here.
        let settings = Settings {
            dim: FadeSettings {
                enabled: false,
                start_before_min: 30,
                over_min: 20,
                target_percent: 35,
            },
            volume: FadeSettings {
                enabled: false,
                start_before_min: 45,
                over_min: 30,
                target_percent: 40,
            },
            end_action: EndAction::Shutdown,
            restore_on_cancel_or_wake: false,
            show_time_in_menu_bar: false,
            last_duration_min: 120,
            check_for_updates: false,
        };
        let json = serde_json::to_value(&settings).unwrap();
        let defaults = serde_json::to_value(Settings::default()).unwrap();
        for (key, value) in json.as_object().unwrap() {
            assert_ne!(Some(value), defaults.get(key), "{key} matches its default");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), settings);
    }

    #[test]
    fn an_invalid_value_resets_only_that_setting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "dim": {"enabled": false, "start_before_min": 30, "over_min": 20, "target_percent": 300},
                "volume": {"target_percent": 5},
                "end_action": "Lock",
                "restore_on_cancel_or_wake": false,
                "show_time_in_menu_bar": "yes",
                "last_duration_min": -5,
                "check_for_updates": false,
                "added_by_a_newer_version": true
            }"#,
        )
        .unwrap();
        let loaded = Settings::load_from(&path);
        let defaults = Settings::default();
        // Out of range for its type: that one field takes the dim default.
        assert_eq!(
            loaded.dim,
            FadeSettings {
                enabled: false,
                start_before_min: 30,
                over_min: 20,
                target_percent: defaults.dim.target_percent,
            }
        );
        // A partial fade keeps the volume defaults, not the dim ones.
        assert_eq!(
            loaded.volume,
            FadeSettings {
                target_percent: 5,
                ..defaults.volume
            }
        );
        assert_eq!(loaded.end_action, EndAction::Sleep);
        assert!(!loaded.restore_on_cancel_or_wake);
        assert_eq!(loaded.show_time_in_menu_bar, defaults.show_time_in_menu_bar);
        assert_eq!(loaded.last_duration_min, defaults.last_duration_min);
        assert!(!loaded.check_for_updates);
    }

    #[test]
    fn a_settings_file_that_is_not_an_object_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        for json in ["[]", "null", "42", "\"text\""] {
            std::fs::write(&path, json).unwrap();
            assert_eq!(Settings::load_from(&path), Settings::default(), "{json}");
        }
    }

    #[test]
    fn a_malformed_fade_resets_only_that_fade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        for dim in ["7", "null", "\"off\"", "[false]"] {
            std::fs::write(
                &path,
                format!(r#"{{"dim": {dim}, "last_duration_min": 90, "check_for_updates": null}}"#),
            )
            .unwrap();
            let loaded = Settings::load_from(&path);
            assert_eq!(loaded.dim, Settings::default().dim, "{dim}");
            assert_eq!(loaded.last_duration_min, 90, "{dim}");
            assert!(loaded.check_for_updates, "{dim}");
        }
    }

    #[test]
    fn a_number_no_setting_can_hold_affects_only_its_own_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // Unknown: ignored, as the typed schema always has.
        std::fs::write(
            &path,
            r#"{"last_duration_min": 90, "from_a_newer_version": 1e400}"#,
        )
        .unwrap();
        assert_eq!(Settings::load_from(&path).last_duration_min, 90);
        // Unknown, while an invalid dim sends loading down the field-by-field path,
        // which must still read every valid value.
        std::fs::write(
            &path,
            r#"{"dim": 7, "from_a_newer_version": 1e400, "last_duration_min": 90,
                "end_action": "Shutdown", "restore_on_cancel_or_wake": false,
                "show_time_in_menu_bar": false, "check_for_updates": false,
                "volume": {"enabled": false}}"#,
        )
        .unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(
            loaded,
            Settings {
                volume: FadeSettings {
                    enabled: false,
                    ..Settings::default().volume
                },
                end_action: EndAction::Shutdown,
                restore_on_cancel_or_wake: false,
                show_time_in_menu_bar: false,
                last_duration_min: 90,
                check_for_updates: false,
                ..Settings::default()
            }
        );
        // Known: only that field takes its default.
        std::fs::write(
            &path,
            r#"{"last_duration_min": 1e400, "dim": {"target_percent": 1e400, "over_min": 5}, "show_time_in_menu_bar": false}"#,
        )
        .unwrap();
        let loaded = Settings::load_from(&path);
        let defaults = Settings::default();
        assert_eq!(loaded.last_duration_min, defaults.last_duration_min);
        assert_eq!(
            loaded.dim,
            FadeSettings {
                over_min: 5,
                ..defaults.dim
            }
        );
        assert!(!loaded.show_time_in_menu_bar);
    }

    #[test]
    fn positional_fades_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let positional = FadeSettings {
            enabled: false,
            start_before_min: 30,
            over_min: 20,
            target_percent: 35,
        };
        std::fs::write(&path, r#"{"dim": [false, 30, 20, 35]}"#).unwrap();
        assert_eq!(Settings::load_from(&path).dim, positional);
        // Also when another field sends loading down the field-by-field path.
        std::fs::write(
            &path,
            r#"{"dim": [false, 30, 20, 35], "end_action": "Lock"}"#,
        )
        .unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.dim, positional);
        assert_eq!(loaded.end_action, EndAction::Sleep);
    }

    #[test]
    fn files_saved_with_a_utf8_bom_keep_their_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"\xEF\xBB\xBF{\"last_duration_min\": 90}").unwrap();
        assert_eq!(Settings::load_from(&path).last_duration_min, 90);
    }
}
