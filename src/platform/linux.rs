//! Linux controls without additional native dependencies. logind/backlight writes
//! and power requests remain subject to the user's session and polkit policy.

use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail, ensure};

use super::{
    Backends, Brightness, Power, Volume,
    display_group::{CapturedDisplay, DisplayDriver, DisplayGroup, DisplayWrite},
    normalized,
};
use crate::settings::EndAction;

fn command(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .with_context(|| format!("running {program}"))?;
    ensure!(
        output.status.success(),
        "{program} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).with_context(|| format!("decoding {program} output"))
}

fn parse_wpctl(output: &str) -> Result<f32> {
    let value = output
        .trim()
        .strip_prefix("Volume:")
        .context("missing wpctl Volume field")?
        .split_whitespace()
        .next()
        .context("empty wpctl volume")?;
    let level: f32 = value.parse().context("parsing wpctl volume")?;
    ensure!(level >= 0.0, "negative wpctl volume");
    normalized(level)
}

fn percentage_average<'a>(values: impl Iterator<Item = &'a str>) -> Result<f32> {
    let mut total = 0.0;
    let mut count = 0;
    for value in values {
        let percent: f32 = value.parse().context("parsing audio percentage")?;
        ensure!(
            percent.is_finite() && percent >= 0.0,
            "invalid audio percentage"
        );
        total += percent.min(100.0) / 100.0;
        count += 1;
    }
    ensure!(count > 0, "no channel volume percentages found");
    Ok(total / count as f32)
}

fn parse_pactl(output: &str) -> Result<f32> {
    percentage_average(
        output
            .split_whitespace()
            .filter_map(|word| word.strip_suffix('%')),
    )
}

fn parse_amixer(output: &str) -> Result<f32> {
    // Only playback lines, never Capture percentages or dB values.
    percentage_average(
        output
            .lines()
            .filter(|line| line.contains("Playback"))
            .flat_map(str::split_whitespace)
            .filter_map(|word| word.strip_prefix('[')?.strip_suffix("%]")),
    )
}

#[derive(Clone, Copy, Debug)]
enum AudioTool {
    PipeWire,
    PulseAudio,
    Alsa,
}

struct LinuxVolume(AudioTool);

impl Volume for LinuxVolume {
    fn get(&mut self) -> Result<f32> {
        match self.0 {
            AudioTool::PipeWire => {
                parse_wpctl(&command("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"])?)
            }
            AudioTool::PulseAudio => {
                parse_pactl(&command("pactl", &["get-sink-volume", "@DEFAULT_SINK@"])?)
            }
            AudioTool::Alsa => parse_amixer(&command("amixer", &["-M", "get", "Master"])?),
        }
    }

    fn set(&mut self, level: f32) -> Result<()> {
        let level = normalized(level)?;
        let percent = format!("{}%", (level * 100.0).round() as u8);
        match self.0 {
            AudioTool::PipeWire => command(
                "wpctl",
                &["set-volume", "@DEFAULT_AUDIO_SINK@", &format!("{level:.6}")],
            )?,
            AudioTool::PulseAudio => {
                command("pactl", &["set-sink-volume", "@DEFAULT_SINK@", &percent])?
            }
            AudioTool::Alsa => command("amixer", &["-M", "set", "Master", &percent])?,
        };
        Ok(())
    }
}

fn read_number(path: &Path) -> Result<u32> {
    fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?
        .trim()
        .parse()
        .with_context(|| format!("parsing {}", path.display()))
}

fn normalized_backlight(value: u32, max: u32) -> Result<f32> {
    ensure!(
        max != 0 && value <= max,
        "invalid backlight value {value}/{max}"
    );
    Ok(value as f32 / max as f32)
}

#[derive(Clone, Copy)]
enum BacklightWriter {
    Sysfs,
    Logind,
    Brightnessctl,
}

struct Backlight {
    path: PathBuf,
    name: String,
    writer: BacklightWriter,
}

impl Backlight {
    /// The current level, and the native maximum it is measured against.
    fn read(&self) -> Result<(f32, u32)> {
        let max = read_number(&self.path.join("max_brightness"))?;
        let level = normalized_backlight(read_number(&self.path.join("brightness"))?, max)?;
        Ok((level, max))
    }

    fn set(&self, value: u32) -> Result<()> {
        let value = value.to_string();
        match self.writer {
            BacklightWriter::Sysfs => fs::write(self.path.join("brightness"), value)
                .with_context(|| format!("writing {} brightness", self.name))?,
            BacklightWriter::Logind => {
                if let Err(error) = command(
                    "busctl",
                    &[
                        "--system",
                        "--timeout=3",
                        "--allow-interactive-authorization=no",
                        "call",
                        "org.freedesktop.login1",
                        "/org/freedesktop/login1/session/auto",
                        "org.freedesktop.login1.Session",
                        "SetBrightness",
                        "ssu",
                        "backlight",
                        &self.name,
                        &value,
                    ],
                ) {
                    command("brightnessctl", &["--device", &self.name, "set", &value])
                        .with_context(|| {
                            format!("logind brightness failed: {error:#}; brightnessctl fallback")
                        })?;
                }
            }
            BacklightWriter::Brightnessctl => {
                command("brightnessctl", &["--device", &self.name, "set", &value])?;
            }
        }
        Ok(())
    }
}

struct LinuxBacklights {
    devices: Vec<Backlight>,
}

impl LinuxBacklights {
    fn discover() -> Result<Self> {
        let mut paths: Vec<_> = fs::read_dir("/sys/class/backlight")
            .context("enumerating backlights")?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        paths.sort();
        let logind_available = command(
            "busctl",
            &[
                "--system",
                "--timeout=3",
                "introspect",
                "org.freedesktop.login1",
                "/org/freedesktop/login1/session/auto",
                "org.freedesktop.login1.Session",
            ],
        )
        .is_ok_and(|output| output.contains("SetBrightness"));
        let mut devices = Vec::new();
        for path in paths {
            let Some(name) = path
                .file_name()
                .and_then(|value| value.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            // Opening without truncation/writing checks access without changing brightness.
            let writer = if OpenOptions::new()
                .write(true)
                .open(path.join("brightness"))
                .is_ok()
            {
                BacklightWriter::Sysfs
            } else if logind_available {
                BacklightWriter::Logind
            } else if command("brightnessctl", &["--device", &name, "info"]).is_ok() {
                // brightnessctl can have udev/setuid access even when sysfs is read-only.
                BacklightWriter::Brightnessctl
            } else {
                continue;
            };
            let device = Backlight { path, name, writer };
            if device.read().is_ok() {
                devices.push(device);
            }
        }
        ensure!(
            !devices.is_empty(),
            "no usable backlight control (sysfs/logind/brightnessctl)"
        );
        Ok(Self { devices })
    }
}

impl DisplayDriver for LinuxBacklights {
    type Key = usize;

    fn capture(&mut self) -> Result<Vec<CapturedDisplay<usize>>> {
        let mut failure = None;
        let displays: Vec<_> = self
            .devices
            .iter()
            .enumerate()
            .filter_map(|(key, device)| match device.read() {
                Ok((level, max)) => Some(CapturedDisplay {
                    key,
                    level,
                    min: 0,
                    max,
                }),
                Err(error) => {
                    log::warn!("Skipping backlight {}: {error:#}", device.name);
                    failure = Some(error);
                    None
                }
            })
            .collect();
        match failure {
            Some(error) if displays.is_empty() => Err(error),
            _ => Ok(displays),
        }
    }

    fn write(&mut self, writes: &[DisplayWrite<usize>]) -> Vec<Result<()>> {
        writes
            .iter()
            .map(|write| self.devices[write.key].set(write.value))
            .collect()
    }
}

struct LinuxPower;

fn hibernate_supported() -> bool {
    match command(
        "busctl",
        &[
            "--system",
            "--timeout=5",
            "--allow-interactive-authorization=no",
            "call",
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            "CanHibernate",
        ],
    ) {
        Ok(output) => logind_allows_hibernate(&output),
        Err(error) => {
            log::debug!("Hibernate capability check failed: {error:#}");
            false
        }
    }
}

fn logind_allows_hibernate(output: &str) -> bool {
    // "challenge" requires authentication and cannot be used unattended by the timer.
    output.trim() == "s \"yes\""
}

impl Power for LinuxPower {
    fn perform(&mut self, action: EndAction) -> Result<()> {
        let (verb, method) = match action {
            EndAction::Sleep => ("suspend", "Suspend"),
            EndAction::Hibernate => ("hibernate", "Hibernate"),
            EndAction::Shutdown => ("poweroff", "PowerOff"),
        };
        if let Err(error) = command("systemctl", &[verb]) {
            log::debug!("systemctl {verb} failed: {error:#}; trying loginctl");
            if let Err(fallback) = command("loginctl", &[verb]) {
                // Modern loginctl versions do not expose these power verbs; call logind
                // directly when the compatibility command is unavailable.
                if let Err(dbus) = command(
                    "busctl",
                    &[
                        "--system",
                        "--timeout=10",
                        "--allow-interactive-authorization=no",
                        "call",
                        "org.freedesktop.login1",
                        "/org/freedesktop/login1",
                        "org.freedesktop.login1.Manager",
                        method,
                        "b",
                        "false",
                    ],
                ) {
                    bail!(
                        "systemctl {verb}: {error:#}; loginctl fallback: {fallback:#}; logind fallback: {dbus:#}"
                    );
                }
            }
        }
        Ok(())
    }
}

pub fn backends() -> Backends {
    let brightness = match LinuxBacklights::discover() {
        Ok(backlights) => Some(Box::new(DisplayGroup::new(backlights)) as Box<dyn Brightness>),
        Err(error) => {
            log::debug!("Linux brightness unavailable: {error:#}");
            None
        }
    };
    let mut volume = None;
    for tool in [AudioTool::PipeWire, AudioTool::PulseAudio, AudioTool::Alsa] {
        let mut backend = LinuxVolume(tool);
        match backend.get() {
            Ok(_) => {
                volume = Some(Box::new(backend) as Box<dyn Volume>);
                break;
            }
            Err(error) => log::debug!("{tool:?} volume unavailable: {error:#}"),
        }
    }
    Backends {
        brightness,
        volume,
        power: Box::new(LinuxPower),
        hibernate_supported: hibernate_supported(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Partial;

    #[test]
    fn hibernate_requires_logind_to_allow_it_without_authentication() {
        assert!(logind_allows_hibernate("s \"yes\"\n"));
        for output in [
            "s \"no\"",
            "s \"na\"",
            "s \"challenge\"",
            "s \"inhibited\"",
            "s \"inhibitor-blocked\"",
            "s \"challenge-inhibitor-blocked\"",
            "",
            "yes",
            "b true",
            "s \"yes\" garbage",
        ] {
            assert!(!logind_allows_hibernate(output), "{output:?}");
        }
    }

    fn backlights(dir: &Path, levels: &[(&str, u32)]) -> DisplayGroup<LinuxBacklights> {
        let mut devices = Vec::new();
        for (name, level) in levels {
            let path = dir.join(name);
            fs::create_dir(&path).unwrap();
            fs::write(path.join("brightness"), level.to_string()).unwrap();
            fs::write(path.join("max_brightness"), "100").unwrap();
            devices.push(Backlight {
                path,
                name: (*name).into(),
                writer: BacklightWriter::Sysfs,
            });
        }
        DisplayGroup::new(LinuxBacklights { devices })
    }

    fn value(dir: &Path, name: &str) -> u32 {
        read_number(&dir.join(name).join("brightness")).unwrap()
    }

    fn break_writes(dir: &Path, name: &str) {
        let brightness = dir.join(name).join("brightness");
        fs::remove_file(&brightness).unwrap();
        fs::create_dir(&brightness).unwrap();
    }

    #[test]
    fn backlight_baselines_refresh_only_on_get_and_restore_each_device() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let mut backend = backlights(dir, &[("dim", 20), ("bright", 80)]);
        assert_eq!(backend.get().unwrap(), 0.8);
        backend.set(0.4).unwrap();
        assert_eq!((value(dir, "dim"), value(dir, "bright")), (10, 40));
        backend.set(0.8).unwrap();
        assert_eq!((value(dir, "dim"), value(dir, "bright")), (20, 80));
        backend.set(0.4).unwrap();
        assert_eq!(backend.get().unwrap(), 0.4);
        backend.set(0.2).unwrap();
        assert_eq!((value(dir, "dim"), value(dir, "bright")), (5, 20));
    }

    #[test]
    fn a_failed_backlight_does_not_block_fading_or_restoring_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let mut backend = backlights(dir, &[("broken", 80), ("healthy", 80)]);
        assert_eq!(backend.get().unwrap(), 0.8);
        backend.set(0.4).unwrap();
        break_writes(dir, "broken");
        assert!(backend.set(0.2).unwrap_err().is::<Partial>());
        assert_eq!(value(dir, "healthy"), 20);
        assert!(backend.restore(0.8).unwrap_err().is::<Partial>());
        assert_eq!(value(dir, "healthy"), 80);
    }

    #[test]
    fn an_unreadable_backlight_is_left_out_of_the_fade() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let mut backend = backlights(dir, &[("unreadable", 90), ("healthy", 60)]);
        fs::remove_file(dir.join("unreadable/brightness")).unwrap();
        assert_eq!(backend.get().unwrap(), 0.6);
        backend.set(0.3).unwrap();
        assert_eq!(value(dir, "healthy"), 30);
    }

    #[test]
    fn pipewire_levels_preserve_muted_scalar_and_clamp_amplification() {
        assert_eq!(parse_wpctl("Volume: 0.42 [MUTED]\n").unwrap(), 0.42);
        assert_eq!(parse_wpctl("Volume: 1.50").unwrap(), 1.0);
        for value in ["", "Volume: NaN", "Volume: -0.2", "Error: 0.8"] {
            assert!(parse_wpctl(value).is_err());
        }
    }

    #[test]
    fn pulse_and_alsa_average_channels_without_mistaking_decibels_for_volume() {
        assert_eq!(
            parse_pactl(
                "Volume: front-left: 32768 / 50% / -18.06 dB, front-right: 16384 / 25% / -36.12 dB"
            )
            .unwrap(),
            0.375
        );
        assert_eq!(parse_amixer("Simple mixer control 'Master',0\n  Front Left: Playback 32 [50%] [-18.00dB] [on]\n  Front Right: Playback 16 [25%] [-36.00dB] [off]\n").unwrap(), 0.375);
        for value in ["", "Playback [NaN%]", "Playback [-5%]"] {
            assert!(parse_pactl(value).is_err());
            assert!(parse_amixer(value).is_err());
        }
    }

    #[test]
    fn backlight_math_rejects_invalid_driver_ranges() {
        assert_eq!(normalized_backlight(25, 100).unwrap(), 0.25);
        assert_eq!(normalized_backlight(0, 100).unwrap(), 0.0);
        assert!(normalized_backlight(1, 0).is_err());
        assert!(normalized_backlight(101, 100).is_err());
    }
}
