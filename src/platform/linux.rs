//! Linux controls without additional native dependencies. logind/backlight writes
//! and power requests remain subject to the user's session and polkit policy.

use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail, ensure};

use super::{Backends, Brightness, Power, Volume};
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

fn normalized(level: f32) -> Result<f32> {
    ensure!(level.is_finite(), "control level must be finite");
    Ok(level.clamp(0.0, 1.0))
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

fn backlight_value(level: f32, max: u32) -> Result<u32> {
    ensure!(max != 0, "backlight maximum is zero");
    // f64 avoids rounding a large u32 maximum above its representable range.
    Ok((f64::from(normalized(level)?) * f64::from(max)).round() as u32)
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
    fn get(&self) -> Result<f32> {
        normalized_backlight(
            read_number(&self.path.join("brightness"))?,
            read_number(&self.path.join("max_brightness"))?,
        )
    }

    fn set(&self, level: f32) -> Result<()> {
        let value =
            backlight_value(level, read_number(&self.path.join("max_brightness"))?)?.to_string();
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

struct LinuxBrightness {
    devices: Vec<Backlight>,
    baselines: Vec<f32>,
    reference: f32,
}

impl LinuxBrightness {
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
            if device.get().is_ok() {
                devices.push(device);
            }
        }
        ensure!(
            !devices.is_empty(),
            "no usable backlight control (sysfs/logind/brightnessctl)"
        );
        Ok(Self {
            devices,
            baselines: Vec::new(),
            reference: 0.0,
        })
    }
}

impl Brightness for LinuxBrightness {
    fn get(&mut self) -> Result<f32> {
        ensure!(!self.devices.is_empty(), "no backlights");
        let baselines = self
            .devices
            .iter()
            .map(Backlight::get)
            .collect::<Result<Vec<_>>>()?;
        self.reference = baselines.iter().copied().fold(0.0, f32::max);
        self.baselines = baselines;
        Ok(self.reference)
    }

    fn set(&mut self, level: f32) -> Result<()> {
        ensure!(level.is_finite(), "non-finite brightness");
        ensure!(
            self.baselines.len() == self.devices.len(),
            "brightness must be read before setting"
        );
        if self.reference > 0.0 {
            for (device, baseline) in self.devices.iter().zip(&self.baselines) {
                device.set(super::scaled_brightness(*baseline, self.reference, level))?;
            }
        }
        Ok(())
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
    let brightness = match LinuxBrightness::discover() {
        Ok(mut backend) => backend
            .get()
            .ok()
            .map(|_| Box::new(backend) as Box<dyn Brightness>),
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

    #[test]
    fn backlight_baselines_refresh_only_on_get_and_restore_each_device() {
        let dir = tempfile::tempdir().unwrap();
        let mut devices = Vec::new();
        for (name, level) in [("dim", 20), ("bright", 80)] {
            let path = dir.path().join(name);
            fs::create_dir(&path).unwrap();
            fs::write(path.join("brightness"), level.to_string()).unwrap();
            fs::write(path.join("max_brightness"), "100").unwrap();
            devices.push(Backlight {
                path,
                name: name.into(),
                writer: BacklightWriter::Sysfs,
            });
        }
        let mut backend = LinuxBrightness {
            devices,
            baselines: Vec::new(),
            reference: 0.0,
        };
        assert_eq!(backend.get().unwrap(), 0.8);
        backend.set(0.4).unwrap();
        assert_eq!(backend.devices[0].get().unwrap(), 0.1);
        assert_eq!(backend.devices[1].get().unwrap(), 0.4);
        backend.set(0.8).unwrap();
        assert_eq!(backend.devices[0].get().unwrap(), 0.2);
        assert_eq!(backend.devices[1].get().unwrap(), 0.8);
        backend.set(0.4).unwrap();
        assert_eq!(backend.get().unwrap(), 0.4);
        backend.set(0.2).unwrap();
        assert_eq!(backend.devices[0].get().unwrap(), 0.05);
        assert_eq!(backend.devices[1].get().unwrap(), 0.2);
        backend
            .devices
            .iter()
            .for_each(|device| device.set(0.0).unwrap());
        assert_eq!(backend.get().unwrap(), 0.0);
        backend.set(0.8).unwrap();
        assert_eq!(backend.devices[0].get().unwrap(), 0.0);
        assert_eq!(backend.devices[1].get().unwrap(), 0.0);
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
