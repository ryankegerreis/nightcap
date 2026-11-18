//! One brightness fade across every display a platform can control.
//!
//! Adapters only capture displays and write native values. The group decides what
//! each display receives, keeps healthy displays fading when another fails, and
//! lets a restore retry only the displays that have not returned yet.

use anyhow::{Result, anyhow, bail, ensure};

use super::{AtMinimum, Brightness, Partial, normalized};

/// A display as read when a fade begins. `level` is within `0.0..=1.0`, and
/// `min..=max` is the native range that level maps onto.
pub(super) struct CapturedDisplay<K> {
    pub(super) key: K,
    pub(super) level: f32,
    pub(super) min: u32,
    pub(super) max: u32,
}

/// A native value for one captured display.
pub(super) struct DisplayWrite<K> {
    pub(super) key: K,
    pub(super) value: u32,
}

/// A write that never reached its display, which a later fade step can try again.
#[derive(Debug)]
pub(super) struct NotAttempted(pub(super) String);

impl std::fmt::Display for NotAttempted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for NotAttempted {}

/// The platform-specific reads and writes behind a [`DisplayGroup`].
pub(super) trait DisplayDriver: Send {
    type Key: Clone + Send;

    /// Reads every display that can be controlled now. One display failing to
    /// read leaves it out; an error means none could be read.
    fn capture(&mut self) -> Result<Vec<CapturedDisplay<Self::Key>>>;

    /// Attempts every write in this pass, returning one result per write, in order.
    fn write(&mut self, writes: &[DisplayWrite<Self::Key>]) -> Vec<Result<()>>;
}

struct GroupDisplay<K> {
    key: K,
    baseline: f32,
    min: u32,
    max: u32,
    written: Option<u32>,
    // The value a restore already returned this display to.
    restored: Option<u32>,
    failed: bool,
}

impl<K> GroupDisplay<K> {
    fn value(&self, reference: f32, level: f32) -> u32 {
        let scaled = f64::from(scaled_brightness(self.baseline, reference, level));
        let range = f64::from(self.max.saturating_sub(self.min));
        (f64::from(self.min) + scaled * range).round() as u32
    }
}

/// Keeps displays of different brightness in proportion as the group fades.
fn scaled_brightness(baseline: f32, reference: f32, level: f32) -> f32 {
    if reference <= 0.0 {
        baseline
    } else {
        (baseline * level / reference).clamp(0.0, 1.0)
    }
}

pub(super) struct DisplayGroup<D: DisplayDriver> {
    driver: D,
    displays: Vec<GroupDisplay<D::Key>>,
    reference: f32,
}

impl<D: DisplayDriver> DisplayGroup<D> {
    pub(super) fn new(driver: D) -> Self {
        Self {
            driver,
            displays: Vec::new(),
            reference: 0.0,
        }
    }

    fn write_level(&mut self, level: f32, restoring: bool) -> Result<()> {
        let level = normalized(level)?;
        ensure!(
            !self.displays.is_empty(),
            "brightness must be captured before writing"
        );
        let mut applied = 0;
        let mut pending = Vec::new();
        let mut writes = Vec::new();
        for (index, display) in self.displays.iter_mut().enumerate() {
            // A fade step takes the display away from its restored level, and the
            // user may have adjusted it since that restore.
            if !restoring && display.restored.take().is_some() {
                display.written = None;
            }
            // A display whose fade write failed sits out until a restore retries it.
            if display.failed && !restoring {
                continue;
            }
            let value = display.value(self.reference, level);
            // A retried restore leaves displays that already returned alone, even
            // if the user has adjusted them since. The first restore always writes,
            // because the user may have changed the display after the last fade step.
            let current = if restoring {
                display.restored
            } else {
                display.written
            };
            if current == Some(value) {
                display.failed = false;
                applied += 1;
            } else {
                pending.push((index, value));
                writes.push(DisplayWrite {
                    key: display.key.clone(),
                    value,
                });
            }
        }
        let mut errors = Vec::new();
        if !writes.is_empty() {
            let mut results = self.driver.write(&writes).into_iter();
            for (index, value) in pending {
                let display = &mut self.displays[index];
                let result = results.next().unwrap_or_else(|| {
                    Err(NotAttempted("the display write was not attempted".into()).into())
                });
                match result {
                    Ok(()) => {
                        display.written = Some(value);
                        display.restored = restoring.then_some(value);
                        display.failed = false;
                        applied += 1;
                    }
                    Err(error) => {
                        // The display may have changed before the error was reported.
                        display.written = None;
                        display.restored = None;
                        // After a failed restore, the next fade tries the display again.
                        display.failed = !restoring && !error.is::<NotAttempted>();
                        errors.push(format!("{error:#}"));
                    }
                }
            }
        }
        // Healthy displays keep fading when others fail; only a total failure is fatal.
        let message = errors.join("; ");
        match (applied, errors.is_empty()) {
            (0, true) => bail!("no captured display accepts brightness changes"),
            (_, true) => Ok(()),
            (0, false) => Err(anyhow!(message)),
            _ => Err(Partial(message).into()),
        }
    }
}

impl<D: DisplayDriver> Brightness for DisplayGroup<D> {
    fn get(&mut self) -> Result<f32> {
        // A failed capture must not leave the previous displays writable.
        self.displays.clear();
        self.reference = 0.0;
        let captured = self.driver.capture()?;
        let present = !captured.is_empty();
        // A display already at its minimum cannot dim natively. Its trivially
        // successful writes (e.g. a virtual display reporting 0) would otherwise
        // count as native fading and keep the overlay off displays that failed.
        self.displays = captured
            .into_iter()
            .filter(|display| display.level > 0.0)
            .map(|display| GroupDisplay {
                key: display.key,
                baseline: display.level,
                min: display.min,
                max: display.max,
                written: None,
                restored: None,
                failed: false,
            })
            .collect();
        let reference = self
            .displays
            .iter()
            .map(|display| display.baseline)
            .reduce(f32::max);
        let Some(reference) = reference else {
            return Err(if present {
                AtMinimum.into()
            } else {
                anyhow!("no supported brightness displays")
            });
        };
        self.reference = reference;
        Ok(reference)
    }

    fn set(&mut self, level: f32) -> Result<()> {
        self.write_level(level, false)
    }

    fn restore(&mut self, level: f32) -> Result<()> {
        self.write_level(level, true)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::{Arc, Mutex},
        time::{Duration, SystemTime},
    };

    use super::*;
    use crate::{
        engine::{Clock, ClockReading, Command, Engine},
        platform::{Backends, DryRunPower},
        settings::Settings,
    };

    #[derive(Default)]
    struct Hardware {
        displays: Vec<(&'static str, f32, u32, u32)>,
        capture_fails: bool,
        broken: BTreeSet<&'static str>,
        /// These displays change brightness and still report an error.
        uncertain: BTreeSet<&'static str>,
        /// Writes to these displays never reach the hardware.
        unreached: BTreeSet<&'static str>,
        values: BTreeMap<&'static str, u32>,
        writes: Vec<(&'static str, u32)>,
        passes: usize,
        drop_results: bool,
    }

    #[derive(Clone)]
    struct FakeDriver(Arc<Mutex<Hardware>>);

    impl DisplayDriver for FakeDriver {
        type Key = &'static str;

        fn capture(&mut self) -> Result<Vec<CapturedDisplay<&'static str>>> {
            let hardware = self.0.lock().unwrap();
            ensure!(!hardware.capture_fails, "capture failed");
            Ok(hardware
                .displays
                .iter()
                .map(|&(key, level, min, max)| CapturedDisplay {
                    key,
                    level,
                    min,
                    max,
                })
                .collect())
        }

        fn write(&mut self, writes: &[DisplayWrite<&'static str>]) -> Vec<Result<()>> {
            let mut hardware = self.0.lock().unwrap();
            hardware.passes += 1;
            if hardware.drop_results {
                return Vec::new();
            }
            writes
                .iter()
                .map(|write| {
                    if hardware.unreached.contains(write.key) {
                        return Err(NotAttempted(format!("{} unreached", write.key)).into());
                    }
                    ensure!(!hardware.broken.contains(write.key), "{} failed", write.key);
                    hardware.values.insert(write.key, write.value);
                    hardware.writes.push((write.key, write.value));
                    ensure!(
                        !hardware.uncertain.contains(write.key),
                        "{} could not be verified",
                        write.key
                    );
                    Ok(())
                })
                .collect()
        }
    }

    fn group() -> (DisplayGroup<FakeDriver>, Arc<Mutex<Hardware>>) {
        group_of(vec![
            ("panel", 0.6, 0, 100),
            ("ddc0", 0.8, 10, 110),
            ("ddc1", 0.4, 20, 220),
        ])
    }

    fn group_of(
        displays: Vec<(&'static str, f32, u32, u32)>,
    ) -> (DisplayGroup<FakeDriver>, Arc<Mutex<Hardware>>) {
        let hardware = Arc::new(Mutex::new(Hardware {
            displays,
            ..Default::default()
        }));
        (DisplayGroup::new(FakeDriver(hardware.clone())), hardware)
    }

    fn written(hardware: &Mutex<Hardware>) -> Vec<(&'static str, u32)> {
        std::mem::take(&mut hardware.lock().unwrap().writes)
    }

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

    #[test]
    fn writes_each_display_its_share_in_one_pass_and_skips_unchanged_values() {
        let (mut group, hardware) = group();
        assert_eq!(group.get().unwrap(), 0.8);
        group.set(0.4).unwrap();
        assert_eq!(
            written(&hardware),
            [("panel", 30), ("ddc0", 50), ("ddc1", 60)]
        );
        assert_eq!(hardware.lock().unwrap().passes, 1);
        group.set(0.4).unwrap();
        assert!(written(&hardware).is_empty());
        assert_eq!(hardware.lock().unwrap().passes, 1);
    }

    #[test]
    fn one_failing_display_keeps_the_others_fading_and_sits_out_the_fade() {
        let (mut group, hardware) = group();
        group.get().unwrap();
        hardware.lock().unwrap().broken.insert("ddc0");
        let error = group.set(0.4).unwrap_err();
        assert!(error.is::<Partial>());
        assert_eq!(written(&hardware), [("panel", 30), ("ddc1", 60)]);
        group.set(0.2).unwrap();
        assert_eq!(written(&hardware), [("panel", 15), ("ddc1", 40)]);
    }

    #[test]
    fn every_display_failing_is_a_total_failure_for_overlay_fallback() {
        let (mut group, hardware) = group();
        group.get().unwrap();
        hardware.lock().unwrap().broken = BTreeSet::from(["panel", "ddc0", "ddc1"]);
        assert!(!group.set(0.4).unwrap_err().is::<Partial>());
        // Every display now sits out, so a later fade step is still a total failure.
        assert!(!group.set(0.2).unwrap_err().is::<Partial>());
        assert!(written(&hardware).is_empty());
    }

    #[test]
    fn restore_reaches_displays_after_a_failed_one() {
        let (mut group, hardware) = group();
        group.get().unwrap();
        group.set(0.4).unwrap();
        written(&hardware);
        hardware.lock().unwrap().broken.insert("panel");
        assert!(group.restore(0.8).unwrap_err().is::<Partial>());
        assert_eq!(written(&hardware), [("ddc0", 90), ("ddc1", 100)]);
    }

    #[test]
    fn restore_retries_only_displays_that_are_not_yet_restored() {
        let (mut group, hardware) = group();
        group.get().unwrap();
        hardware.lock().unwrap().broken.insert("ddc0");
        assert!(group.set(0.4).is_err());
        written(&hardware);
        // Restore retries the display that sat out the fade, alongside the others.
        {
            let mut hardware = hardware.lock().unwrap();
            hardware.broken.clear();
            hardware.broken.insert("panel");
        }
        assert!(group.restore(0.8).unwrap_err().is::<Partial>());
        assert_eq!(written(&hardware), [("ddc0", 90), ("ddc1", 100)]);
        // The user adjusts a restored display before the retry; it is left alone.
        {
            let mut hardware = hardware.lock().unwrap();
            hardware.values.insert("ddc1", 70);
            hardware.broken.clear();
        }
        group.restore(0.8).unwrap();
        assert_eq!(written(&hardware), [("panel", 60)]);
        assert_eq!(hardware.lock().unwrap().values["ddc1"], 70);
    }

    #[test]
    fn a_new_capture_forgets_earlier_failures_and_writes() {
        let (mut group, hardware) = group();
        group.get().unwrap();
        hardware.lock().unwrap().broken.insert("ddc0");
        assert!(group.set(0.4).is_err());
        hardware.lock().unwrap().broken.clear();
        written(&hardware);
        group.get().unwrap();
        group.set(0.4).unwrap();
        assert_eq!(
            written(&hardware),
            [("panel", 30), ("ddc0", 50), ("ddc1", 60)]
        );
    }

    #[test]
    fn displays_at_minimum_are_not_counted_as_native_fading() {
        // Reproduced with a virtual display reporting 0 beside a monitor that
        // ignores writes: the virtual display's writes masked the failure.
        let (mut group, hardware) =
            group_of(vec![("virtual", 0.0, 0, 100), ("panel", 0.6, 0, 100)]);
        assert_eq!(group.get().unwrap(), 0.6);
        hardware.lock().unwrap().broken.insert("panel");
        assert!(!group.set(0.3).unwrap_err().is::<Partial>());
        assert!(written(&hardware).is_empty());
    }

    #[test]
    fn capture_errors_explain_why_nothing_can_fade() {
        let (mut group, _) = group_of(vec![("panel", 0.0, 0, 100)]);
        assert!(group.get().unwrap_err().is::<AtMinimum>());
        let (mut group, _) = group_of(Vec::new());
        let error = group.get().unwrap_err().to_string();
        assert!(error.contains("no supported"), "{error}");
    }

    #[test]
    fn a_failed_capture_does_not_reuse_earlier_displays() {
        let (mut group, hardware) = group();
        group.get().unwrap();
        hardware.lock().unwrap().capture_fails = true;
        assert!(group.get().is_err());
        assert!(group.set(0.5).is_err());
        assert!(written(&hardware).is_empty());
    }

    #[test]
    fn writes_require_a_capture_and_a_finite_level() {
        let (mut group, hardware) = group();
        assert!(group.set(0.5).is_err());
        assert!(group.restore(0.5).is_err());
        group.get().unwrap();
        assert!(group.set(f32::NAN).is_err());
        assert!(written(&hardware).is_empty());
    }

    #[test]
    fn the_first_restore_writes_even_when_a_fade_step_matched_the_original() {
        let (mut group, hardware) = group_of(vec![("panel", 0.8, 0, 100)]);
        group.get().unwrap();
        // An early fade step rounds to the original, then the user changes brightness.
        group.set(0.8).unwrap();
        hardware.lock().unwrap().values.insert("panel", 50);
        group.restore(0.8).unwrap();
        assert_eq!(hardware.lock().unwrap().values["panel"], 80);
    }

    #[test]
    fn a_fade_step_after_a_restore_makes_the_next_restore_write_again() {
        let (mut group, hardware) = group_of(vec![("panel", 0.8, 0, 100)]);
        group.get().unwrap();
        group.restore(0.8).unwrap();
        group.set(0.8).unwrap();
        hardware.lock().unwrap().values.insert("panel", 50);
        group.restore(0.8).unwrap();
        assert_eq!(hardware.lock().unwrap().values["panel"], 80);
    }

    #[test]
    fn a_new_fade_after_a_partial_restore_writes_a_display_the_user_adjusted() {
        let (mut group, hardware) = group_of(vec![("panel", 0.8, 0, 100), ("other", 0.8, 0, 100)]);
        group.get().unwrap();
        group.set(0.4).unwrap();
        hardware.lock().unwrap().broken.insert("other");
        assert!(group.restore(0.8).unwrap_err().is::<Partial>());
        {
            let mut hardware = hardware.lock().unwrap();
            hardware.values.insert("panel", 50);
            hardware.broken.clear();
        }
        // The new timer's first step rounds to the value the restore wrote.
        group.set(0.8).unwrap();
        assert_eq!(hardware.lock().unwrap().values["panel"], 80);
    }

    #[test]
    fn a_failed_restore_does_not_keep_the_display_out_of_the_next_fade() {
        let (mut group, hardware) = group_of(vec![("panel", 0.8, 0, 100)]);
        group.get().unwrap();
        group.set(0.4).unwrap();
        hardware.lock().unwrap().broken.insert("panel");
        assert!(group.restore(0.8).is_err());
        assert!(group.restore(0.8).is_err());
        hardware.lock().unwrap().broken.clear();
        written(&hardware);
        group.set(0.6).unwrap();
        assert_eq!(written(&hardware), [("panel", 60)]);
    }

    #[test]
    fn a_display_that_recovers_fades_natively_in_the_next_timer() {
        #[derive(Clone)]
        struct StepClock(Arc<Mutex<ClockReading>>);

        impl Clock for StepClock {
            fn now(&self) -> ClockReading {
                *self.0.lock().unwrap()
            }
        }

        impl StepClock {
            fn advance_minutes(&self, minutes: u64) {
                let mut reading = self.0.lock().unwrap();
                reading.monotonic += Duration::from_secs(minutes * 60);
                // Keep accelerated timer time from looking like a system wake.
                reading.wall += Duration::from_millis(250);
            }
        }

        let (group, hardware) = group_of(vec![("panel", 0.8, 0, 100)]);
        let clock = StepClock(Arc::new(Mutex::new(ClockReading {
            monotonic: Duration::ZERO,
            wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        })));
        let backends = Backends {
            brightness: Some(Box::new(group)),
            volume: None,
            power: Box::new(DryRunPower),
            hibernate_supported: false,
        };
        let mut engine = Engine::new(Settings::default(), backends, clock.clone());
        let start = || Command::Start {
            duration: Duration::from_secs(30 * 60),
        };
        engine.command(start());
        clock.advance_minutes(15);
        engine.tick();
        assert!(!written(&hardware).is_empty());
        // The cancel and the new timer's start both fail to restore the display.
        hardware.lock().unwrap().broken.insert("panel");
        engine.command(Command::Cancel);
        engine.command(start());
        hardware.lock().unwrap().broken.clear();
        clock.advance_minutes(15);
        engine.tick();
        assert!(!written(&hardware).is_empty());
        assert_eq!(engine.snapshot().overlay_alpha, 0.0);
    }

    #[test]
    fn a_write_that_changed_the_display_before_failing_is_not_trusted() {
        let (mut group, hardware) = group_of(vec![("panel", 0.6, 0, 100)]);
        group.get().unwrap();
        // This step rounds to the original value, which restore would then skip.
        group.set(0.6).unwrap();
        hardware.lock().unwrap().uncertain.insert("panel");
        assert!(group.set(0.4).is_err());
        assert_eq!(hardware.lock().unwrap().values["panel"], 40);
        hardware.lock().unwrap().uncertain.clear();
        group.restore(0.6).unwrap();
        assert_eq!(hardware.lock().unwrap().values["panel"], 60);
    }

    #[test]
    fn a_write_that_never_reached_the_display_is_tried_again_on_the_next_step() {
        let (mut group, hardware) = group();
        group.get().unwrap();
        hardware.lock().unwrap().unreached.insert("panel");
        assert!(group.set(0.4).unwrap_err().is::<Partial>());
        assert_eq!(written(&hardware), [("ddc0", 50), ("ddc1", 60)]);
        hardware.lock().unwrap().unreached.clear();
        group.set(0.2).unwrap();
        assert_eq!(
            written(&hardware),
            [("panel", 15), ("ddc0", 30), ("ddc1", 40)]
        );
    }

    #[test]
    fn a_write_without_a_result_counts_as_failed() {
        let (mut group, hardware) = group();
        group.get().unwrap();
        hardware.lock().unwrap().drop_results = true;
        assert!(!group.set(0.4).unwrap_err().is::<Partial>());
    }
}
