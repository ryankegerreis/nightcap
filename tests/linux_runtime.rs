//! Linux native discovery and opt-in hardware QA. Never requests a power action.
#![cfg(target_os = "linux")]

use anyhow::{Context, Result, ensure};
use nightcap::{
    engine::{Clock, ClockReading, Command, Engine, TimerState},
    platform::{self, Backends, Power},
    settings::{EndAction, Settings},
};
use std::{
    fs,
    path::Path,
    process::Command as OsCommand,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime},
};

// wpctl prints two decimal places; pactl writes whole percentages.
const TOLERANCE: f32 = 0.015;

fn brightness_tolerance(root: &Path) -> f32 {
    let Ok(entries) = fs::read_dir(root) else {
        return TOLERANCE;
    };
    entries
        .flatten()
        .filter_map(|entry| fs::read_to_string(entry.path().join("max_brightness")).ok())
        .filter_map(|value| value.trim().parse::<u32>().ok())
        .filter(|max| *max > 0)
        // Native writes round to the nearest step. Allow half a step plus the
        // percentage rounding used by the test's target settings.
        .map(|max| TOLERANCE + 0.5 / max as f32)
        .fold(TOLERANCE, f32::max)
}

fn validate_volume_levels(levels: &[f32]) -> Result<()> {
    ensure!(!levels.is_empty(), "no raw volume levels found");
    for level in levels {
        ensure!(
            level.is_finite() && (0.0..=1.0).contains(level),
            "raw volume {level} cannot be restored by the normalized backend; disable amplification before running the hardware test"
        );
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Tolerances {
    brightness: f32,
    volume: f32,
}

impl Tolerances {
    fn discover() -> Result<Self> {
        // Match native discovery's CLI priority, checking raw levels before any
        // guard exists: its Drop would otherwise write a clamped capture back.
        for (program, args) in [
            ("wpctl", vec!["get-volume", "@DEFAULT_AUDIO_SINK@"]),
            ("pactl", vec!["get-sink-volume", "@DEFAULT_SINK@"]),
        ] {
            let Ok(output) = OsCommand::new(program)
                .args(args)
                .env("LC_ALL", "C")
                .output()
            else {
                continue;
            };
            if !output.status.success() {
                continue;
            }
            let output = String::from_utf8(output.stdout)?;
            let levels = if program == "wpctl" {
                vec![
                    output
                        .trim()
                        .strip_prefix("Volume:")
                        .and_then(|value| value.split_whitespace().next())
                        .context("missing raw wpctl volume")?
                        .parse::<f32>()?,
                ]
            } else {
                output
                    .split_whitespace()
                    .filter_map(|word| word.strip_suffix('%'))
                    .map(|value| value.parse::<f32>().map(|value| value / 100.0))
                    .collect::<std::result::Result<Vec<_>, _>>()?
            };
            validate_volume_levels(&levels)?;
            return Ok(Self {
                brightness: brightness_tolerance(Path::new("/sys/class/backlight")),
                volume: TOLERANCE,
            });
        }
        Ok(Self {
            brightness: brightness_tolerance(Path::new("/sys/class/backlight")),
            // ALSA's mapped mixer controls can have coarser steps than the
            // percentage arguments accepted by amixer.
            volume: 0.05,
        })
    }
}

#[test]
fn coarse_backlight_rounding_is_accepted_without_hiding_large_errors() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let panel = dir.path().join("panel");
    fs::create_dir(&panel)?;
    fs::write(panel.join("max_brightness"), "15\n")?;
    let tolerance = brightness_tolerance(dir.path());
    // A 50% request rounds to 8/15 on this panel. A 75% reading is wrong.
    ensure!((8.0 / 15.0 - 0.5_f32).abs() <= tolerance);
    ensure!((0.75 - 0.5_f32).abs() > tolerance);
    Ok(())
}

#[test]
fn raw_volume_preflight_rejects_amplification_and_invalid_levels() {
    assert!(validate_volume_levels(&[0.0, 0.42, 1.0]).is_ok());
    for levels in [
        vec![1.3],
        vec![0.8, 1.1],
        vec![-0.1],
        vec![f32::NAN],
        vec![f32::INFINITY],
        vec![],
    ] {
        assert!(validate_volume_levels(&levels).is_err(), "{levels:?}");
    }
}

#[derive(Clone, Copy, Debug)]
struct Levels {
    brightness: Option<f32>,
    volume: Option<f32>,
}

impl Levels {
    fn read(backends: &mut Backends) -> Result<Self> {
        let levels = Self {
            brightness: backends.brightness.as_mut().map(|b| b.get()).transpose()?,
            volume: backends.volume.as_mut().map(|b| b.get()).transpose()?,
        };
        for level in [levels.brightness, levels.volume].into_iter().flatten() {
            ensure!(level.is_finite() && (0.0..=1.0).contains(&level));
        }
        Ok(levels)
    }

    fn assert_matches(self, expected: Self, tolerances: Tolerances) -> Result<()> {
        for (actual, expected, tolerance) in [
            (self.brightness, expected.brightness, tolerances.brightness),
            (self.volume, expected.volume, tolerances.volume),
        ] {
            match (actual, expected) {
                (Some(actual), Some(expected)) => ensure!(
                    (actual - expected).abs() <= tolerance,
                    "native level {actual}, expected {expected}"
                ),
                (None, None) => {}
                _ => anyhow::bail!("native capabilities changed"),
            }
        }
        Ok(())
    }
}

#[test]
fn native_discovery_and_worker_reads() -> Result<()> {
    let mut backends = platform::backends();
    backends.power = Box::new(platform::DryRunPower);
    let brightness = backends.brightness.is_some();
    let volume = backends.volume.is_some();
    let hibernate = backends.hibernate_supported;
    thread::spawn(move || -> Result<()> {
        println!(
            "native levels: {:?}; hibernate={hibernate}",
            Levels::read(&mut backends)?
        );
        let mut engine = Engine::new(Settings::default(), backends, TestClock::new());
        let snapshot = engine.snapshot();
        ensure!(snapshot.state == TimerState::Idle);
        ensure!(snapshot.brightness_supported == brightness);
        ensure!(snapshot.volume_supported == volume);
        ensure!(snapshot.hibernate_supported == hibernate);
        ensure!(snapshot.last_error.is_none());
        ensure!(!engine.command(Command::Shutdown));
        Ok(())
    })
    .join()
    .expect("native worker panicked")
}

#[derive(Clone)]
struct TestClock(Arc<Mutex<ClockReading>>);

impl TestClock {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(ClockReading {
            monotonic: Duration::ZERO,
            wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        })))
    }

    fn advance(&self, seconds: u64) {
        let mut reading = self.0.lock().unwrap();
        reading.monotonic += Duration::from_secs(seconds);
        // Keep synthetic elapsed time from looking like a system wake.
        reading.wall += Duration::from_millis(250);
    }
}

impl Clock for TestClock {
    fn now(&self) -> ClockReading {
        *self.0.lock().unwrap()
    }
}

struct RestoreHardware {
    backends: Backends,
    original: Levels,
}

impl RestoreHardware {
    fn capture(mut backends: Backends) -> Result<Self> {
        backends.power = Box::new(platform::DryRunPower);
        let original = Levels::read(&mut backends)?;
        Ok(Self { backends, original })
    }

    fn restore(&mut self) -> Result<()> {
        // Attempt both even if one fails.
        let brightness = match (self.backends.brightness.as_mut(), self.original.brightness) {
            (Some(backend), Some(level)) => backend.set(level),
            _ => Ok(()),
        };
        let volume = match (self.backends.volume.as_mut(), self.original.volume) {
            (Some(backend), Some(level)) => backend.set(level),
            _ => Ok(()),
        };
        brightness?;
        volume
    }
}

impl Drop for RestoreHardware {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("restoring native levels failed: {error:#}");
        }
    }
}

#[test]
fn guard_restores_each_original_backlight_after_partial_and_zero_observations() -> Result<()> {
    // A read captures the baseline just as LinuxBrightness::get does. Only the
    // hardware I/O is substituted so this failure-path check can run by default.
    struct Backlights {
        levels: Arc<Mutex<[f32; 2]>>,
        baseline: [f32; 2],
        reference: f32,
    }

    impl platform::Brightness for Backlights {
        fn get(&mut self) -> Result<f32> {
            self.baseline = *self.levels.lock().unwrap();
            self.reference = self.baseline.into_iter().fold(0.0, f32::max);
            Ok(self.reference)
        }

        fn set(&mut self, level: f32) -> Result<()> {
            if self.reference > 0.0 {
                let ratio = level / self.reference;
                *self.levels.lock().unwrap() = self.baseline.map(|value| value * ratio);
            }
            Ok(())
        }
    }

    for faded in [[0.8, 0.15], [0.0, 0.0]] {
        let levels = Arc::new(Mutex::new([0.8, 0.2]));
        let backends = || Backends {
            brightness: Some(Box::new(Backlights {
                levels: Arc::clone(&levels),
                baseline: [0.0; 2],
                reference: 0.0,
            })),
            volume: None,
            power: Box::new(platform::DryRunPower),
            hibernate_supported: false,
        };
        {
            let _guard = RestoreHardware::capture(backends())?;
            let mut observer = backends();
            *levels.lock().unwrap() = faded;
            Levels::read(&mut observer)?;
            // Drop models cleanup when a runtime assertion returns an error.
        }
        assert_eq!(*levels.lock().unwrap(), [0.8, 0.2]);
    }
    Ok(())
}

struct RecordedPower(Arc<Mutex<Vec<EndAction>>>);

impl Power for RecordedPower {
    fn perform(&mut self, action: EndAction) -> Result<()> {
        platform::DryRunPower.perform(action)?;
        self.0.lock().unwrap().push(action);
        Ok(())
    }
}

#[test]
#[ignore = "changes real Linux brightness/volume; run alone with stable devices; power is dry-run"]
fn native_fades_controls_completion_and_restoration() -> Result<()> {
    let tolerances = Tolerances::discover()?;
    // Brightness reads recapture per-device baselines. Keep the restore guard's
    // capture untouched, even when assertion reads see partial fades or zero.
    let mut restore = RestoreHardware::capture(platform::backends())?;
    let original = restore.original;
    let mut observer = platform::backends();
    observer.power = Box::new(platform::DryRunPower);
    Levels::read(&mut observer)?.assert_matches(original, tolerances)?;
    println!("original levels: {original:?}");
    let actions = Arc::new(Mutex::new(Vec::new()));
    let mut backends = platform::backends();
    backends.power = Box::new(RecordedPower(actions.clone()));
    let mut settings = Settings::default();
    settings.dim.start_before_min = 15;
    settings.dim.over_min = 10;
    settings.dim.target_percent = (original.brightness.unwrap_or(1.0) * 50.0).floor() as u8;
    settings.volume.target_percent = (original.volume.unwrap_or(1.0) * 50.0).floor() as u8;
    let clock = TestClock::new();
    let mut engine = Engine::new(settings.clone(), backends, clock.clone());
    let start = Command::Start {
        duration: Duration::from_secs(900),
    };
    engine.command(start);
    clock.advance(300);
    engine.tick();
    let faded = engine.snapshot();
    ensure!(faded.state == TimerState::Running && faded.last_error.is_none());
    let lowered = Levels::read(&mut observer)?;
    lowered.assert_matches(
        Levels {
            brightness: original.brightness.map(|v| v * 0.75),
            volume: original.volume.map(|v| v * 0.75),
        },
        tolerances,
    )?;
    if original.brightness.is_none() {
        ensure!((faded.overlay_alpha - 0.225).abs() < 0.001);
    }
    println!(
        "faded levels: {lowered:?}; overlay alpha={}",
        faded.overlay_alpha
    );
    engine.command(Command::Pause);
    clock.advance(120);
    engine.tick();
    ensure!(engine.snapshot().remaining == faded.remaining);
    engine.command(Command::AddMinutes(15));
    ensure!(engine.snapshot().remaining == faded.remaining + Duration::from_secs(900));
    Levels::read(&mut observer)?.assert_matches(lowered, tolerances)?;
    engine.command(Command::Resume);
    clock.advance(60);
    engine.tick();
    ensure!(engine.snapshot().remaining == Duration::from_secs(1440));
    engine.command(Command::Cancel);
    ensure!(engine.snapshot().state == TimerState::Idle);
    ensure!(engine.snapshot().overlay_alpha == 0.0);
    Levels::read(&mut observer)?.assert_matches(original, tolerances)?;
    ensure!(actions.lock().unwrap().is_empty());

    let mut expected_actions = Vec::new();
    for action in [EndAction::Sleep, EndAction::Hibernate, EndAction::Shutdown] {
        if action == EndAction::Hibernate && !engine.snapshot().hibernate_supported {
            println!("hibernate unavailable; skipping hibernate completion");
            continue;
        }
        settings.end_action = action;
        engine.command(Command::UpdateSettings(settings.clone()));
        engine.command(Command::Start {
            duration: Duration::from_secs(900),
        });
        clock.advance(900);
        engine.tick();
        ensure!(engine.snapshot().state == TimerState::Finishing);
        ensure!(engine.snapshot().last_error.is_none());
        expected_actions.push(action);
        ensure!(*actions.lock().unwrap() == expected_actions);
        Levels::read(&mut observer)?.assert_matches(
            Levels {
                brightness: original
                    .brightness
                    .map(|_| f32::from(settings.dim.target_percent) / 100.0),
                volume: original
                    .volume
                    .map(|_| f32::from(settings.volume.target_percent) / 100.0),
            },
            tolerances,
        )?;
        engine.command(Command::SystemWoke);
        ensure!(engine.snapshot().state == TimerState::Idle);
        ensure!(engine.snapshot().overlay_alpha == 0.0);
        Levels::read(&mut observer)?.assert_matches(original, tolerances)?;
    }

    settings.restore_on_cancel_or_wake = false;
    engine.command(Command::UpdateSettings(settings.clone()));
    engine.command(Command::Start {
        duration: Duration::from_secs(900),
    });
    clock.advance(300);
    engine.tick();
    let retained = engine.snapshot();
    engine.command(Command::Cancel);
    ensure!(engine.snapshot().overlay_alpha == retained.overlay_alpha);
    Levels::read(&mut observer)?.assert_matches(lowered, tolerances)?;
    restore.restore()?;

    settings.restore_on_cancel_or_wake = true;
    engine.command(Command::UpdateSettings(settings));
    engine.command(Command::Start {
        duration: Duration::from_secs(900),
    });
    clock.advance(300);
    engine.tick();
    ensure!(!engine.command(Command::Shutdown));
    ensure!(engine.snapshot().state == TimerState::Idle);
    ensure!(engine.snapshot().overlay_alpha == 0.0);
    ensure!(engine.snapshot().last_error.is_none());
    Levels::read(&mut observer)?.assert_matches(original, tolerances)?;
    println!(
        "native fades, pause/resume/extend, cancel, dry-run completion, synthetic wake, restore-off and shutdown verified"
    );
    Ok(())
}
