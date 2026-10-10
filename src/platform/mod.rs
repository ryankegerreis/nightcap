//! Normalized platform controls; all levels use the range `0.0..=1.0`.

use anyhow::Result;

use crate::settings::EndAction;

pub mod fallback;
// Linux's command parsers and sysfs math are portable and tested on every host.
#[cfg(any(target_os = "linux", test))]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

// Shared pure math, also tested on hosts without Windows/Linux display hardware.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
pub(crate) fn scaled_brightness(baseline: f32, reference: f32, level: f32) -> f32 {
    if reference <= 0.0 {
        baseline
    } else {
        (baseline * level / reference).clamp(0.0, 1.0)
    }
}

pub trait Brightness: Send {
    fn get(&mut self) -> Result<f32>;
    fn set(&mut self, level: f32) -> Result<()>;
}

pub trait Volume: Send {
    fn get(&mut self) -> Result<f32>;
    fn set(&mut self, level: f32) -> Result<()>;
}

pub trait Power: Send {
    fn perform(&mut self, action: EndAction) -> Result<()>;
}

pub struct Backends {
    pub brightness: Option<Box<dyn Brightness>>,
    pub volume: Option<Box<dyn Volume>>,
    pub power: Box<dyn Power>,
    /// Checked once at startup, including permission to request hibernation.
    pub hibernate_supported: bool,
}

pub fn backends() -> Backends {
    #[cfg(target_os = "macos")]
    {
        macos::backends()
    }
    #[cfg(target_os = "windows")]
    {
        windows::backends()
    }
    #[cfg(target_os = "linux")]
    {
        linux::backends()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        fallback::backends()
    }
}

/// Used for live UI/fade verification without requesting any system power action.
pub struct DryRunPower;

impl Power for DryRunPower {
    fn perform(&mut self, action: EndAction) -> Result<()> {
        log::warn!("NIGHTCAP_DRY_RUN: would perform {action:?}; no power action requested");
        Ok(())
    }
}

#[cfg(test)]
mod scaling_tests {
    use super::scaled_brightness;

    #[test]
    fn different_displays_fade_without_brightening_and_restore_individually() {
        for baseline in [0.2, 0.8] {
            assert!((scaled_brightness(baseline, 0.8, 0.4) - baseline / 2.0).abs() < 1e-6);
            assert!((scaled_brightness(baseline, 0.8, 0.8) - baseline).abs() < 1e-6);
            for step in 0..=80 {
                assert!(scaled_brightness(baseline, 0.8, step as f32 / 100.0) <= baseline + 1e-6);
            }
        }
        assert_eq!(scaled_brightness(0.0, 0.0, 1.0), 0.0);
        assert_eq!(scaled_brightness(0.2, 0.0, 1.0), 0.2);
        assert_eq!(scaled_brightness(0.8, 0.8, 2.0), 1.0);
        assert_eq!(scaled_brightness(0.8, 0.8, -1.0), 0.0);
    }
}
