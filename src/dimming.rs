//! Screen dimming: chooses how the screen dims and carries the fade through it.
//!
//! Native brightness is preferred. When it is missing or fails, the fade drives the
//! overlay instead. The overlay's window belongs to the UI thread, so the UI reports
//! whether it can draw one and this module decides what that means for the fade.

use std::time::Duration;

use anyhow::Result;

use crate::{
    fade::{FadePlan, FadeTrack, valid_level},
    platform::{Brightness, Disconnected, Partial},
};

/// Overlay opacity at a level of zero, leaving the screen faintly visible.
const MAX_OVERLAY_ALPHA: f32 = 0.9;

/// How the screen is dimmed right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DimMethod {
    /// The displays' own brightness.
    Native,
    /// A click-through black window over the screen.
    Overlay,
    /// Neither works, so the screen stays as it is.
    #[default]
    Unavailable,
}

/// What the UI needs to show and draw for screen dimming.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DimStatus {
    pub method: DimMethod,
    /// `0.0..=100.0`. `None` until a level applies, and while nothing can dim.
    pub percent: Option<f32>,
    /// Kept up to date while the overlay is gone, so it returns at the fade's level.
    pub overlay_alpha: f32,
}

pub(crate) struct ScreenDimming {
    backend: Option<Box<dyn Brightness>>,
    overlay_available: bool,
    /// Lasts until the next timer, which tries native brightness again.
    native_failed: bool,
    track: FadeTrack,
    fading_on_overlay: bool,
    /// The last native level, which the overlay dims relative to after a failed write.
    overlay_reference: Option<f32>,
    percent: Option<f32>,
    overlay_alpha: f32,
}

impl ScreenDimming {
    /// The overlay counts as available until the UI reports otherwise.
    pub(crate) fn new(backend: Option<Box<dyn Brightness>>) -> Self {
        Self {
            backend,
            overlay_available: true,
            native_failed: false,
            track: FadeTrack::default(),
            fading_on_overlay: false,
            overlay_reference: None,
            percent: None,
            overlay_alpha: 0.0,
        }
    }

    pub(crate) fn status(&self) -> DimStatus {
        let method = if self.backend.is_some() && !self.native_failed {
            DimMethod::Native
        } else if self.overlay_available {
            DimMethod::Overlay
        } else {
            DimMethod::Unavailable
        };
        DimStatus {
            method,
            percent: self.percent.filter(|_| method != DimMethod::Unavailable),
            overlay_alpha: self.overlay_alpha,
        }
    }

    pub(crate) fn set_overlay_available(&mut self, available: bool) {
        self.overlay_available = available;
    }

    /// A new timer gives native brightness another chance.
    pub(crate) fn begin_timer(&mut self) {
        self.native_failed = false;
    }

    pub(crate) fn fade(&mut self, plan: FadePlan, elapsed: Duration, target: u8) -> Result<()> {
        let mut outcome = self.capture();
        let level = self.track.level(plan, elapsed, target);
        if !self.fading_on_overlay
            && self.track.last != Some(level)
            && let Some(backend) = self.backend.as_mut()
            && let Err(error) = backend.set(level)
        {
            // Displays that took the level keep fading natively.
            if !error.is::<Partial>() {
                self.native_failed = true;
                self.fading_on_overlay = true;
                self.overlay_reference = self.track.last;
            }
            outcome = Err(error.context("setting brightness"));
        }
        if self.fading_on_overlay {
            let reference = self.overlay_reference.unwrap_or(1.0);
            let relative = if reference > 0.0 {
                level / reference
            } else {
                1.0
            };
            self.overlay_alpha = MAX_OVERLAY_ALPHA * (1.0 - relative.clamp(0.0, 1.0));
        }
        self.track.last = Some(level);
        self.percent = Some(level * 100.0);
        outcome
    }

    /// Reads the original level when a fade begins, or moves the fade to the overlay.
    fn capture(&mut self) -> Result<()> {
        if self.track.from.is_some() {
            return Ok(());
        }
        let original = match self.track.original {
            Some(level) => Some(Ok(level)),
            None => self
                .backend
                .as_mut()
                .map(|backend| backend.get().and_then(valid_level)),
        };
        match original {
            Some(Ok(level)) => {
                self.track.original.get_or_insert(level);
                self.track.from = Some(level);
                self.track.last = Some(level);
                self.percent = Some(level * 100.0);
                Ok(())
            }
            failed => {
                self.native_failed = true;
                self.fading_on_overlay = true;
                self.track.from = Some(1.0 - self.overlay_alpha / MAX_OVERLAY_ALPHA);
                match failed {
                    Some(Err(error)) => Err(error.context("reading brightness")),
                    _ => Ok(()),
                }
            }
        }
    }

    /// Clears the overlay and returns the displays to their original level.
    pub(crate) fn restore(&mut self) -> Result<()> {
        let mut outcome = Ok(());
        if let Some(level) = self.track.original
            && let Some(backend) = self.backend.as_mut()
        {
            match backend.restore(level) {
                Ok(()) => {
                    self.track.original = None;
                    self.percent = Some(level * 100.0);
                }
                Err(error) => {
                    // A display that is gone has nothing left to restore.
                    if error.is::<Disconnected>() {
                        self.track.original = None;
                    }
                    outcome = Err(error.context("restoring brightness"));
                }
            }
        }
        self.overlay_alpha = 0.0;
        if self.fading_on_overlay {
            self.percent = None;
        }
        outcome
    }

    pub(crate) fn restore_pending(&self) -> bool {
        self.track.original.is_some()
    }

    /// Stops retrying, so a later fade captures the displays again.
    pub(crate) fn abandon_restore(&mut self) {
        self.track.original = None;
    }

    /// A kept original means its restore has not finished, so the next fade starts from it.
    /// Without one, nothing will restore: the displays keep their faded level, but the
    /// overlay is cleared, since no one else could remove Nightcap's own window.
    pub(crate) fn end_fade(&mut self, keep_original: bool) {
        if !keep_original {
            self.overlay_alpha = 0.0;
            if self.fading_on_overlay {
                self.percent = None;
            }
        }
        self.track = self.track.reset(keep_original);
        self.fading_on_overlay = false;
        self.overlay_reference = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{platform::fallback::MockBrightness, settings::FadeSettings};

    /// Fades from the first second over 100 seconds, down to a level of zero.
    fn plan() -> FadePlan {
        FadePlan::new(
            Duration::from_secs(100),
            &FadeSettings {
                enabled: true,
                start_before_min: 2,
                over_min: 2,
                target_percent: 0,
            },
        )
    }

    fn at(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    fn close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 0.0001, "{actual} != {expected}");
    }

    fn native(level: f32) -> (ScreenDimming, MockBrightness) {
        let brightness = MockBrightness::new(level);
        (
            ScreenDimming::new(Some(Box::new(brightness.clone()))),
            brightness,
        )
    }

    #[test]
    fn working_brightness_dims_natively_whatever_the_overlay_can_do() {
        let (mut dimming, brightness) = native(0.8);
        dimming.set_overlay_available(false);
        dimming.fade(plan(), at(50), 0).unwrap();
        let status = dimming.status();
        assert_eq!(status.method, DimMethod::Native);
        close(status.percent.unwrap(), 40.0);
        close(status.overlay_alpha, 0.0);
        close(brightness.state.lock().unwrap().level, 0.4);
    }

    #[test]
    fn without_brightness_or_an_overlay_no_level_is_reported() {
        let mut dimming = ScreenDimming::new(None);
        dimming.set_overlay_available(false);
        assert_eq!(dimming.status().method, DimMethod::Unavailable);
        dimming.fade(plan(), at(50), 0).unwrap();
        assert_eq!(dimming.status().method, DimMethod::Unavailable);
        assert_eq!(dimming.status().percent, None);
    }

    #[test]
    fn an_overlay_that_returns_resumes_at_the_level_the_fade_has_reached() {
        let mut dimming = ScreenDimming::new(None);
        dimming.fade(plan(), at(25), 0).unwrap();
        close(dimming.status().overlay_alpha, 0.225);

        dimming.set_overlay_available(false);
        dimming.fade(plan(), at(50), 0).unwrap();
        assert_eq!(dimming.status().method, DimMethod::Unavailable);

        dimming.set_overlay_available(true);
        let status = dimming.status();
        assert_eq!(status.method, DimMethod::Overlay);
        close(status.percent.unwrap(), 50.0);
        close(status.overlay_alpha, 0.45);
    }

    #[test]
    fn failed_native_writes_leave_nothing_to_dim_when_the_overlay_is_gone() {
        let (mut dimming, brightness) = native(0.8);
        dimming.set_overlay_available(false);
        dimming.fade(plan(), at(0), 0).unwrap();
        brightness.state.lock().unwrap().fail_set = true;
        let error = dimming.fade(plan(), at(50), 0).unwrap_err();
        assert!(format!("{error:#}").starts_with("setting brightness: "));
        assert_eq!(dimming.status().method, DimMethod::Unavailable);
        assert_eq!(dimming.status().percent, None);
    }

    #[test]
    fn a_new_timer_tries_native_brightness_again() {
        let (mut dimming, brightness) = native(0.8);
        brightness.state.lock().unwrap().fail_get = true;
        assert!(dimming.fade(plan(), at(50), 0).is_err());
        assert_eq!(dimming.status().method, DimMethod::Overlay);

        dimming.restore().unwrap();
        dimming.end_fade(true);
        assert_eq!(
            dimming.status().method,
            DimMethod::Overlay,
            "the failure stays reported until the next timer"
        );

        brightness.state.lock().unwrap().fail_get = false;
        dimming.begin_timer();
        assert_eq!(dimming.status().method, DimMethod::Native);
        dimming.fade(plan(), at(50), 0).unwrap();
        close(brightness.state.lock().unwrap().level, 0.4);
    }
}
