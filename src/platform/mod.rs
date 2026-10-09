//! Normalized platform controls; all levels use the range `0.0..=1.0`.

use anyhow::Result;

use crate::settings::EndAction;

pub mod fallback;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

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
