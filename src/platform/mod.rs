//! Normalized platform controls; all levels use the range `0.0..=1.0`.

use anyhow::Result;

use crate::settings::EndAction;

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux", test))]
mod display_group;
pub mod fallback;
// Linux's command parsers and sysfs math are portable and tested on every host.
#[cfg(any(target_os = "linux", test))]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
mod process;
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux", test))]
pub(crate) fn normalized(level: f32) -> Result<f32> {
    anyhow::ensure!(level.is_finite(), "control level must be finite");
    Ok(level.clamp(0.0, 1.0))
}

/// Some displays took the level and others failed. The engine keeps fading the
/// healthy displays natively, and keeps retrying a restore until all succeed.
#[derive(Debug)]
pub struct Partial(pub String);

/// The captured device no longer exists, so its level cannot be restored.
/// The engine drops that original instead of retrying it.
#[derive(Debug)]
pub struct Disconnected(pub String);

/// Every display is already at its minimum, so none of them can fade natively.
#[derive(Debug)]
pub struct AtMinimum;

impl std::fmt::Display for AtMinimum {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("every display is already at its minimum brightness")
    }
}

impl std::fmt::Display for Partial {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::fmt::Display for Disconnected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AtMinimum {}
impl std::error::Error for Partial {}
impl std::error::Error for Disconnected {}

pub trait Brightness: Send {
    /// Captures the displays for a fade and returns the level to restore.
    fn get(&mut self) -> Result<f32>;
    fn set(&mut self, level: f32) -> Result<()>;

    /// Return to the captured level. Unlike `set`, this also retries displays
    /// that failed earlier in the fade.
    fn restore(&mut self, level: f32) -> Result<()>;
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
