//! Native Windows QA; run ignored hardware tests alone with stable output devices.
#![cfg(target_os = "windows")]

use anyhow::{Context, Result, ensure};
use nightcap::{
    engine::{Clock, ClockReading, Command, Engine, EngineHandle, Snapshot, TimerState},
    platform::{self, Backends, Power},
    settings::{EndAction, Settings},
};
use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant, SystemTime},
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};

const BRIGHTNESS_TOLERANCE: f32 = 0.02;
const VOLUME_TOLERANCE: f32 = 0.003;

struct UiApartment;

impl UiApartment {
    fn enter() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .context("initializing the simulated UI thread's STA")?;
        Ok(Self)
    }
}

impl Drop for UiApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn read_only_level(level: f32) -> Result<()> {
    ensure!(
        level.is_finite() && (0.0..=1.0).contains(&level),
        "native backend returned invalid level {level}"
    );
    Ok(())
}

#[test]
fn native_discovery_without_an_existing_com_apartment() {
    thread::spawn(|| {
        let mut backends = platform::backends();
        backends.power = Box::new(platform::DryRunPower);
        if let Some(volume) = backends.volume.as_mut() {
            read_only_level(volume.get().unwrap()).unwrap();
        }
    })
    .join()
    .unwrap();
}

#[test]
fn native_discovery_on_ui_thread_and_volume_read_on_worker() -> Result<()> {
    let _apartment = UiApartment::enter()?;
    let mut backends = platform::backends();
    backends.power = Box::new(platform::DryRunPower);
    let volume_available = backends.volume.is_some();
    let brightness_available = backends.brightness.is_some();
    let hibernate_supported = backends.hibernate_supported;
    thread::spawn(move || -> Result<()> {
        if let Some(volume) = backends.volume.as_mut() {
            read_only_level(
                volume
                    .get()
                    .context("reading volume after transfer to worker")?,
            )?;
        } else {
            eprintln!("no native output volume available; cross-thread volume read skipped");
        }
        let mut engine = Engine::new(Settings::default(), backends, TestClock::new());
        let snapshot = engine.snapshot();
        ensure!(
            snapshot.state == TimerState::Idle,
            "discovery started a timer"
        );
        ensure!(snapshot.volume_supported == volume_available);
        ensure!(snapshot.brightness_supported == brightness_available);
        ensure!(snapshot.hibernate_supported == hibernate_supported);
        ensure!(snapshot.last_error.is_none());
        ensure!(
            !engine.command(Command::Shutdown),
            "idle worker did not stop"
        );
        Ok(())
    })
    .join()
    .map_err(|_| anyhow::anyhow!("read-only native worker panicked"))?
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
        reading.wall += Duration::from_millis(250);
    }
}

impl Clock for TestClock {
    fn now(&self) -> ClockReading {
        *self.0.lock().unwrap()
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
            read_only_level(level)?;
        }
        Ok(levels)
    }

    fn matches(self, expected: Self) -> bool {
        let close =
            |actual: Option<f32>, target: Option<f32>, tolerance: f32| match (actual, target) {
                (Some(actual), Some(target)) => (actual - target).abs() <= tolerance,
                (None, None) => true,
                _ => false,
            };
        close(self.brightness, expected.brightness, BRIGHTNESS_TOLERANCE)
            && close(self.volume, expected.volume, VOLUME_TOLERANCE)
    }

    fn from_snapshot(snapshot: &Snapshot, original: Self) -> Self {
        Self {
            brightness: original
                .brightness
                .and(snapshot.brightness_percent.map(|v| v / 100.0)),
            volume: original
                .volume
                .and(snapshot.volume_percent.map(|v| v / 100.0)),
        }
    }
}

struct RestoreHardware {
    controls: Backends,
    original: Levels,
}

impl RestoreHardware {
    fn capture() -> Result<Self> {
        let mut controls = platform::backends();
        controls.power = Box::new(platform::DryRunPower);
        let original = Levels::read(&mut controls)?;
        Ok(Self { controls, original })
    }

    fn restore(&mut self) -> Result<()> {
        let brightness = match (self.controls.brightness.as_mut(), self.original.brightness) {
            (Some(backend), Some(level)) => backend.set(level),
            _ => Ok(()),
        };
        let volume = match (self.controls.volume.as_mut(), self.original.volume) {
            (Some(backend), Some(level)) => backend.set(level),
            _ => Ok(()),
        };
        brightness.context("restoring original per-monitor brightness")?;
        volume.context("restoring original output volume")
    }
}

impl Drop for RestoreHardware {
    fn drop(&mut self) {
        for _ in 0..3 {
            match self.restore() {
                Ok(()) => return,
                Err(error) => eprintln!("hardware restoration failed: {error:#}"),
            }
        }
    }
}

struct RecordedDryRun {
    inner: platform::DryRunPower,
    actions: Arc<Mutex<Vec<EndAction>>>,
}

impl Power for RecordedDryRun {
    fn perform(&mut self, action: EndAction) -> Result<()> {
        self.inner.perform(action)?;
        self.actions.lock().unwrap().push(action);
        Ok(())
    }
}

struct Runtime {
    handle: EngineHandle,
    repaints: mpsc::Receiver<()>,
}

impl Runtime {
    fn wait(&self, predicate: impl Fn(&Snapshot) -> bool) -> Result<Snapshot> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let snapshot = self.handle.snapshot();
            if let Some(error) = &snapshot.last_error {
                ensure!(
                    !snapshot.brightness_supported && error.starts_with("setting brightness:"),
                    "native engine error: {snapshot:?}"
                );
            }
            if predicate(&snapshot) {
                return Ok(snapshot);
            }
            ensure!(
                Instant::now() < deadline,
                "worker did not reach expected state: {snapshot:?}"
            );
            let _ = self.repaints.recv_timeout(Duration::from_millis(50));
        }
    }

    fn start(&self) -> Result<()> {
        self.handle.send(Command::Start {
            duration: Duration::from_secs(900),
        })?;
        self.wait(|s| s.state == TimerState::Running && s.remaining == Duration::from_secs(900))?;
        Ok(())
    }
}

fn wait_levels(observer: &mut Backends, expected: Levels) -> Result<Levels> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let actual = Levels::read(observer)?;
        if actual.matches(expected) {
            return Ok(actual);
        }
        ensure!(
            Instant::now() < deadline,
            "native levels {actual:?}, expected {expected:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn assert_faded(observer: &mut Backends, snapshot: &Snapshot, original: Levels) -> Result<Levels> {
    ensure!(snapshot.volume_supported == original.volume.is_some());
    let mut expected = Levels::from_snapshot(snapshot, original);
    if !snapshot.brightness_supported {
        expected.brightness = Levels::read(observer)?.brightness;
    }
    let actual = wait_levels(observer, expected)?;
    for (name, original, actual, tolerance) in [
        (
            "brightness",
            original.brightness,
            actual.brightness,
            BRIGHTNESS_TOLERANCE,
        ),
        ("volume", original.volume, actual.volume, VOLUME_TOLERANCE),
    ] {
        if let (Some(original), Some(actual)) = (original, actual) {
            ensure!(
                actual <= original + tolerance,
                "{name} increased during fade"
            );
            if original > tolerance * 4.0 && (name != "brightness" || snapshot.brightness_supported)
            {
                ensure!(actual < original - tolerance, "{name} did not visibly fade");
            }
        }
    }
    if !snapshot.brightness_supported {
        ensure!(
            snapshot.overlay_alpha > 0.0 && snapshot.overlay_alpha <= 0.9,
            "missing dimming overlay"
        );
        let level = snapshot
            .brightness_percent
            .context("overlay fade has no level")?
            / 100.0;
        let reference = original.brightness.unwrap_or(1.0);
        let relative = if reference > 0.0 {
            level / reference
        } else {
            1.0
        };
        ensure!((snapshot.overlay_alpha - 0.9 * (1.0 - relative)).abs() < 0.0001);
    } else {
        ensure!(
            snapshot.overlay_alpha == 0.0,
            "native brightness unexpectedly used overlay"
        );
    }
    Ok(actual)
}

#[test]
#[ignore = "changes real Windows brightness and volume; run alone; all power actions use DryRunPower"]
fn native_fades_pause_extend_cancel_wake_and_worker_shutdown() -> Result<()> {
    let _apartment = UiApartment::enter()?;
    let mut restore = RestoreHardware::capture()?;
    let original = restore.original;
    println!("original levels: {original:?}");
    let mut observer = platform::backends();
    observer.power = Box::new(platform::DryRunPower);
    ensure!(
        Levels::read(&mut observer)?.matches(original),
        "hardware changed during discovery"
    );
    let mut backends = platform::backends();
    let actions = Arc::new(Mutex::new(Vec::new()));
    backends.power = Box::new(RecordedDryRun {
        inner: platform::DryRunPower,
        actions: Arc::clone(&actions),
    });
    ensure!(backends.brightness.is_some() == original.brightness.is_some());
    ensure!(backends.volume.is_some() == original.volume.is_some());
    let hibernate_supported = backends.hibernate_supported;
    let mut settings = Settings::default();
    settings.dim.target_percent = (original.brightness.unwrap_or(1.0) * 50.0).floor() as u8;
    settings.volume.target_percent = (original.volume.unwrap_or(1.0) * 50.0).floor() as u8;
    let clock = TestClock::new();
    let (sender, repaints) = mpsc::channel();
    let runtime = Runtime {
        handle: Engine::spawn_with_clock(
            settings.clone(),
            backends,
            clock.clone(),
            Box::new(move || {
                let _ = sender.send(());
            }),
        )?,
        repaints,
    };
    runtime.start()?;
    clock.advance(300);
    let faded = runtime.wait(|s| s.remaining == Duration::from_secs(600))?;
    let lowered = assert_faded(&mut observer, &faded, original)?;
    println!(
        "faded levels: {lowered:?}; overlay alpha={:.3}",
        faded.overlay_alpha
    );
    runtime.handle.send(Command::Pause)?;
    let paused = runtime.wait(|s| s.state == TimerState::Paused)?;
    clock.advance(120);
    runtime.handle.send(Command::AddMinutes(15))?;
    let extended = runtime.wait(|s| s.remaining == paused.remaining + Duration::from_secs(900))?;
    ensure!(extended.state == TimerState::Paused);
    ensure!(extended.total == paused.total + Duration::from_secs(900));
    ensure!(extended.brightness_percent == paused.brightness_percent);
    ensure!(extended.volume_percent == paused.volume_percent);
    ensure!(extended.overlay_alpha == paused.overlay_alpha);
    wait_levels(&mut observer, lowered)?;
    runtime.handle.send(Command::Resume)?;
    runtime.wait(|s| s.state == TimerState::Running)?;
    clock.advance(1050);
    let resumed = runtime.wait(|s| s.remaining == Duration::from_secs(450))?;
    assert_faded(&mut observer, &resumed, original)?;
    runtime.handle.send(Command::Cancel)?;
    let cancelled = runtime.wait(|s| s.state == TimerState::Idle)?;
    ensure!(cancelled.overlay_alpha == 0.0);
    wait_levels(&mut observer, original)?;
    ensure!(
        actions.lock().unwrap().is_empty(),
        "cancel performed an end action"
    );
    let mut expected_actions = Vec::new();
    for action in [EndAction::Sleep, EndAction::Hibernate, EndAction::Shutdown] {
        if action == EndAction::Hibernate && !hibernate_supported {
            println!("hibernate unavailable; skipping hibernate completion");
            continue;
        }
        settings.end_action = action;
        runtime
            .handle
            .send(Command::UpdateSettings(settings.clone()))?;
        runtime.start()?;
        clock.advance(900);
        expected_actions.push(action);
        let completed = runtime.wait(|s| {
            s.state == TimerState::Finishing && *actions.lock().unwrap() == expected_actions
        })?;
        ensure!(completed.remaining == Duration::ZERO && completed.ends_at.is_none());
        assert_faded(&mut observer, &completed, original)?;
        runtime.handle.send(Command::SystemWoke)?;
        let woke = runtime.wait(|s| s.state == TimerState::Idle)?;
        ensure!(woke.overlay_alpha == 0.0);
        wait_levels(&mut observer, original)?;
    }
    settings.restore_on_cancel_or_wake = false;
    settings.end_action = EndAction::Sleep;
    runtime
        .handle
        .send(Command::UpdateSettings(settings.clone()))?;
    for wake_after_completion in [false, true] {
        runtime.start()?;
        let seconds = if wake_after_completion { 900 } else { 300 };
        clock.advance(seconds);
        if wake_after_completion {
            expected_actions.push(EndAction::Sleep);
        }
        let faded = runtime.wait(|s| {
            s.remaining == Duration::from_secs(900 - seconds)
                && (!wake_after_completion
                    || (s.state == TimerState::Finishing
                        && *actions.lock().unwrap() == expected_actions))
        })?;
        let retained = assert_faded(&mut observer, &faded, original)?;
        runtime.handle.send(if wake_after_completion {
            Command::SystemWoke
        } else {
            Command::Cancel
        })?;
        let idle = runtime.wait(|s| s.state == TimerState::Idle)?;
        ensure!(
            idle.overlay_alpha == faded.overlay_alpha,
            "restore-off cleared dimming overlay"
        );
        wait_levels(&mut observer, retained)?;
        restore.restore()?;
        wait_levels(&mut observer, original)?;
    }
    settings.restore_on_cancel_or_wake = true;
    runtime.handle.send(Command::UpdateSettings(settings))?;
    runtime.start()?;
    clock.advance(300);
    let faded = runtime.wait(|s| s.remaining == Duration::from_secs(600))?;
    assert_faded(&mut observer, &faded, original)?;
    runtime.handle.shutdown();
    ensure!(
        runtime.handle.snapshot().last_error == faded.last_error,
        "worker reported an error during shutdown: {:?}",
        runtime.handle.snapshot()
    );
    ensure!(
        runtime.handle.snapshot().state == TimerState::Idle,
        "worker did not shut down gracefully"
    );
    ensure!(runtime.handle.snapshot().overlay_alpha == 0.0);
    ensure!(
        runtime.handle.send(Command::Pause).is_err(),
        "worker command receiver still open"
    );
    ensure!(
        *actions.lock().unwrap() == expected_actions,
        "unexpected dry-run power action"
    );
    wait_levels(&mut observer, original)?;
    println!(
        "native fades, pause/resume/extend, cancel, dry-run completion, wake, restore-off and worker shutdown verified"
    );
    Ok(())
}
