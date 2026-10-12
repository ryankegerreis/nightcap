//! Clock-driven timer state machine and its independent worker thread.

use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use crate::{fade::FadePlan, platform::Backends, settings::Settings};

const TICK_INTERVAL: Duration = Duration::from_millis(250);
const WAKE_THRESHOLD: Duration = Duration::from_secs(30);
const FINISHING_WAKE_THRESHOLD: Duration = Duration::from_secs(5);
const RESTORE_RETRY: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
pub struct ClockReading {
    pub monotonic: Duration,
    pub wall: SystemTime,
}

pub trait Clock: Send {
    fn now(&self) -> ClockReading;

    /// Convert timer time to real time for the displayed end time.
    fn wall_duration(&self, duration: Duration) -> Duration {
        duration
    }
}

pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> ClockReading {
        ClockReading {
            monotonic: self.origin.elapsed(),
            wall: SystemTime::now(),
        }
    }
}

/// Speeds up timer time without speeding up wall time (and falsely detecting wake).
pub struct SpeedClock<C> {
    clock: C,
    speed: f64,
}

impl<C: Clock> SpeedClock<C> {
    pub fn new(clock: C, speed: f64) -> anyhow::Result<Self> {
        anyhow::ensure!(
            speed.is_finite() && speed > 0.0,
            "speed must be finite and positive"
        );
        Ok(Self { clock, speed })
    }
}

impl<C: Clock> Clock for SpeedClock<C> {
    fn now(&self) -> ClockReading {
        let mut reading = self.clock.now();
        reading.monotonic =
            Duration::try_from_secs_f64(reading.monotonic.as_secs_f64() * self.speed)
                .unwrap_or(Duration::MAX);
        reading
    }

    fn wall_duration(&self, duration: Duration) -> Duration {
        self.clock.wall_duration(
            Duration::try_from_secs_f64(duration.as_secs_f64() / self.speed)
                .unwrap_or(Duration::MAX),
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimerState {
    #[default]
    Idle,
    Running,
    Paused,
    Finishing,
}

#[derive(Clone, Debug)]
pub enum Command {
    Start { duration: Duration },
    Pause,
    Resume,
    Cancel,
    AddMinutes(u32),
    UpdateSettings(Settings),
    SystemWoke,
    Shutdown,
}

/// Percent fields use `0.0..=100.0`; `None` means no level has been applied yet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub state: TimerState,
    pub total: Duration,
    pub remaining: Duration,
    pub ends_at: Option<chrono::DateTime<chrono::Local>>,
    pub brightness_percent: Option<f32>,
    pub volume_percent: Option<f32>,
    pub overlay_alpha: f32,
    pub brightness_supported: bool,
    pub volume_supported: bool,
    pub hibernate_supported: bool,
    pub last_error: Option<String>,
}

impl Snapshot {
    /// Whole seconds shown in the menu bar. A partial second rounds up so the
    /// title reaches zero only when the timer does.
    pub(crate) fn menu_seconds(&self) -> u64 {
        self.remaining
            .as_secs()
            .saturating_add(u64::from(self.remaining.subsec_nanos() != 0))
    }
}

/// Cloning retains the worker; dropping the last handle shuts it down and joins it.
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<HandleInner>,
}

struct HandleInner {
    sender: mpsc::Sender<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
    window_visible: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl EngineHandle {
    pub fn send(&self, command: Command) -> Result<(), mpsc::SendError<Command>> {
        self.inner.sender.send(command)
    }

    /// Stop the worker and wait for level restoration, even when other handles exist.
    pub fn shutdown(&self) {
        let _ = self.send(Command::Shutdown);
        if let Some(worker) = self
            .inner
            .worker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// The root window's visibility. Hidden timers still run; the worker just
    /// skips UI wakes that would not change the menu bar or the dimming overlay.
    pub fn set_window_visible(&self, visible: bool) {
        self.inner.window_visible.store(visible, Ordering::Relaxed);
    }
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Shutdown);
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }
}

#[derive(Default)]
struct FadeTrack {
    original: Option<f32>,
    from: Option<f32>,
    last: Option<f32>,
    overlay: bool,
    unavailable: bool,
}

impl FadeTrack {
    fn reset(&self, keep_original: bool) -> Self {
        Self {
            original: self.original.filter(|_| keep_original),
            last: self
                .last
                .filter(|_| keep_original && self.original.is_some()),
            ..Self::default()
        }
    }

    fn level(&self, plan: FadePlan, elapsed: Duration, target: u8) -> f32 {
        let from = self.from.unwrap_or(1.0);
        plan.level_at(elapsed, from, f32::from(target) / 100.0)
            .min(self.last.unwrap_or(from))
    }
}

pub struct Engine<C: Clock = SystemClock> {
    clock: C,
    backends: Backends,
    settings: Settings,
    snapshot: Snapshot,
    shared: Arc<Mutex<Snapshot>>,
    window_visible: Arc<AtomicBool>,
    repaint: Box<dyn Fn() + Send>,
    last_time: ClockReading,
    brightness: FadeTrack,
    volume: FadeTrack,
    restore_after: Option<SystemTime>,
}

impl Engine<SystemClock> {
    pub fn spawn(
        settings: Settings,
        backends: Backends,
        repaint: Box<dyn Fn() + Send>,
    ) -> io::Result<EngineHandle> {
        Self::spawn_with_clock(settings, backends, SystemClock::default(), repaint)
    }
}

impl<C: Clock + 'static> Engine<C> {
    pub fn spawn_with_clock(
        settings: Settings,
        backends: Backends,
        clock: C,
        repaint: Box<dyn Fn() + Send>,
    ) -> io::Result<EngineHandle> {
        let mut engine = Self::new(settings, backends, clock);
        engine.repaint = repaint;
        let snapshot = Arc::clone(&engine.shared);
        let window_visible = Arc::clone(&engine.window_visible);
        let (sender, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("nightcap-timer".into())
            .spawn(move || {
                loop {
                    // An idle timer has nothing to advance and no wake to detect.
                    // Block until a command arrives instead of polling four times a second.
                    let incoming = if engine.needs_tick() {
                        match receiver.recv_timeout(TICK_INTERVAL) {
                            Ok(command) => Some(command),
                            Err(mpsc::RecvTimeoutError::Timeout) => None,
                            Err(mpsc::RecvTimeoutError::Disconnected) => {
                                engine.command(Command::Shutdown);
                                break;
                            }
                        }
                    } else {
                        match receiver.recv() {
                            Ok(command) => Some(command),
                            Err(mpsc::RecvError) => {
                                engine.command(Command::Shutdown);
                                break;
                            }
                        }
                    };
                    match incoming {
                        Some(command) => {
                            if !engine.command(command) {
                                break;
                            }
                        }
                        None => engine.tick(),
                    }
                }
            })?;
        Ok(EngineHandle {
            inner: Arc::new(HandleInner {
                sender,
                snapshot,
                window_visible,
                worker: Mutex::new(Some(worker)),
            }),
        })
    }
}

impl<C: Clock> Engine<C> {
    pub fn new(mut settings: Settings, backends: Backends, clock: C) -> Self {
        settings.apply_hibernate_availability(backends.hibernate_supported);
        let snapshot = Snapshot {
            brightness_supported: backends.brightness.is_some(),
            volume_supported: backends.volume.is_some(),
            hibernate_supported: backends.hibernate_supported,
            ..Snapshot::default()
        };
        let last_time = clock.now();
        Self {
            clock,
            backends,
            settings,
            shared: Arc::new(Mutex::new(snapshot.clone())),
            window_visible: Arc::new(AtomicBool::new(true)),
            snapshot,
            repaint: Box::new(|| {}),
            last_time,
            brightness: FadeTrack::default(),
            volume: FadeTrack::default(),
            restore_after: None,
        }
    }

    /// Applies a command immediately; returns false when the worker should stop.
    pub fn command(&mut self, command: Command) -> bool {
        self.advance_time();
        match command {
            Command::Start { duration } => {
                self.cancel();
                // The new fade owns any pending originals until its next cancel/wake.
                self.restore_after = None;
                self.snapshot.state = TimerState::Running;
                self.snapshot.total = duration;
                self.snapshot.remaining = duration;
                self.snapshot.last_error = None;
                self.snapshot.brightness_supported = self.backends.brightness.is_some();
                self.snapshot.volume_supported = self.backends.volume.is_some();
            }
            Command::Pause
                if self.snapshot.state == TimerState::Running
                    && !self.snapshot.remaining.is_zero() =>
            {
                self.apply_fades();
                self.snapshot.state = TimerState::Paused;
            }
            Command::Resume if self.snapshot.state == TimerState::Paused => {
                self.snapshot.state = TimerState::Running;
            }
            Command::Cancel => self.cancel(),
            Command::SystemWoke if self.snapshot.state != TimerState::Idle => self.cancel(),
            Command::AddMinutes(minutes)
                if matches!(
                    self.snapshot.state,
                    TimerState::Running | TimerState::Paused
                ) =>
            {
                let extra = Duration::from_secs(u64::from(minutes) * 60);
                self.snapshot.total = self.snapshot.total.saturating_add(extra);
                self.snapshot.remaining = self.snapshot.remaining.saturating_add(extra);
            }
            // Retain captured originals, and never raise an already faded level.
            Command::UpdateSettings(mut settings) => {
                settings.apply_hibernate_availability(self.snapshot.hibernate_supported);
                self.settings = settings;
            }
            Command::Shutdown => {
                self.cancel();
                self.publish();
                return false;
            }
            _ => {}
        }
        self.evaluate();
        true
    }

    pub fn tick(&mut self) {
        self.advance_time();
        self.evaluate();
    }

    /// Running, paused, and finishing timers watch the clock. So does a failed
    /// restore, which retries on a later tick. Idle otherwise waits for a command.
    fn needs_tick(&self) -> bool {
        self.snapshot.state != TimerState::Idle || self.restore_after.is_some()
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }

    fn advance_time(&mut self) {
        let now = self.clock.now();
        let elapsed = now.monotonic.saturating_sub(self.last_time.monotonic);
        let wall_gap = now
            .wall
            .duration_since(self.last_time.wall)
            .unwrap_or_default();
        self.last_time = now;
        let threshold = if self.snapshot.state == TimerState::Finishing {
            FINISHING_WAKE_THRESHOLD
        } else {
            WAKE_THRESHOLD
        };
        if self.snapshot.state != TimerState::Idle && wall_gap > threshold {
            // Suspend may stop the monotonic clock on some operating systems.
            self.cancel();
        } else if self.snapshot.state == TimerState::Running {
            self.snapshot.remaining = self.snapshot.remaining.saturating_sub(elapsed);
        }
    }

    fn evaluate(&mut self) {
        if self
            .restore_after
            .is_some_and(|deadline| self.last_time.wall >= deadline)
        {
            self.restore();
        }
        if self.snapshot.state == TimerState::Running {
            self.apply_fades();
            if self.snapshot.remaining.is_zero() {
                self.snapshot.state = TimerState::Finishing;
                self.snapshot.ends_at = None;
                self.publish();
                if let Err(error) = self.backends.power.perform(self.settings.end_action) {
                    self.report_error("performing end action", error);
                    self.cancel();
                }
            }
        }
        self.snapshot.ends_at = if self.snapshot.state == TimerState::Running {
            chrono::TimeDelta::from_std(self.clock.wall_duration(self.snapshot.remaining))
                .ok()
                .and_then(|remaining| {
                    chrono::DateTime::<chrono::Local>::from(self.last_time.wall)
                        .checked_add_signed(remaining)
                })
        } else {
            None
        };
        self.publish();
    }

    fn apply_fades(&mut self) {
        let elapsed = self.snapshot.total.saturating_sub(self.snapshot.remaining);
        let dim = self.settings.dim;
        let dim_plan = FadePlan::new(self.snapshot.total, &dim);
        if dim.enabled && elapsed >= dim_plan.start_offset {
            self.apply_brightness(dim_plan, elapsed, dim.target_percent);
        }
        let volume = self.settings.volume;
        let volume_plan = FadePlan::new(self.snapshot.total, &volume);
        if volume.enabled && elapsed >= volume_plan.start_offset && !self.volume.unavailable {
            self.apply_volume(volume_plan, elapsed, volume.target_percent);
        }
    }

    fn apply_brightness(&mut self, plan: FadePlan, elapsed: Duration, target: u8) {
        if self.brightness.from.is_none() {
            let original = if let Some(level) = self.brightness.original {
                Some(Ok(self.brightness.last.unwrap_or(level)))
            } else {
                self.backends
                    .brightness
                    .as_mut()
                    .map(|backend| backend.get().and_then(valid_level))
            };
            match original {
                Some(Ok(level)) => {
                    self.brightness.original.get_or_insert(level);
                    self.brightness.from = Some(level);
                    self.brightness.last = Some(level);
                    self.snapshot.brightness_percent = Some(level * 100.0);
                }
                result => {
                    if let Some(Err(error)) = result {
                        self.report_error("reading brightness", error);
                    }
                    self.brightness.overlay = true;
                    self.brightness.from = Some(1.0 - self.snapshot.overlay_alpha / 0.9);
                    self.snapshot.brightness_supported = false;
                }
            }
        }
        let level = self.brightness.level(plan, elapsed, target);
        if !self.brightness.overlay
            && self.brightness.last != Some(level)
            && let Some(backend) = self.backends.brightness.as_mut()
            && let Err(error) = backend.set(level)
        {
            self.report_error("setting brightness", error);
            self.snapshot.brightness_supported = false;
            self.brightness.overlay = true;
        }
        if self.brightness.overlay {
            self.snapshot.overlay_alpha = 0.9 * (1.0 - level);
        }
        self.brightness.last = Some(level);
        self.snapshot.brightness_percent = Some(level * 100.0);
    }

    fn apply_volume(&mut self, plan: FadePlan, elapsed: Duration, target: u8) {
        if self.volume.from.is_none() {
            let original = if let Some(level) = self.volume.original {
                Some(Ok(self.volume.last.unwrap_or(level)))
            } else {
                self.backends
                    .volume
                    .as_mut()
                    .map(|backend| backend.get().and_then(valid_level))
            };
            match original {
                Some(Ok(level)) => {
                    self.volume.original.get_or_insert(level);
                    self.volume.from = Some(level);
                    self.volume.last = Some(level);
                    self.snapshot.volume_percent = Some(level * 100.0);
                }
                result => {
                    if let Some(Err(error)) = result {
                        self.report_error("reading volume", error);
                    }
                    self.volume.unavailable = true;
                    self.snapshot.volume_supported = false;
                    return;
                }
            }
        }
        let level = self.volume.level(plan, elapsed, target);
        if self.volume.last != Some(level)
            && let Some(backend) = self.backends.volume.as_mut()
            && let Err(error) = backend.set(level)
        {
            self.report_error("setting volume", error);
            self.volume.unavailable = true;
            self.snapshot.volume_supported = false;
            return;
        }
        self.volume.last = Some(level);
        self.snapshot.volume_percent = Some(level * 100.0);
    }

    fn restore(&mut self) {
        if let Some(level) = self.brightness.original
            && let Some(backend) = self.backends.brightness.as_mut()
        {
            match backend.set(level) {
                Ok(()) => {
                    self.brightness.original = None;
                    self.snapshot.brightness_percent = Some(level * 100.0);
                }
                Err(error) => self.report_error("restoring brightness", error),
            }
        }
        if let Some(level) = self.volume.original
            && let Some(backend) = self.backends.volume.as_mut()
        {
            match backend.set(level) {
                Ok(()) => {
                    self.volume.original = None;
                    self.snapshot.volume_percent = Some(level * 100.0);
                }
                Err(error) => self.report_error("restoring volume", error),
            }
        }
        self.restore_after = (self.brightness.original.is_some() || self.volume.original.is_some())
            .then(|| self.last_time.wall + RESTORE_RETRY);
        self.snapshot.overlay_alpha = 0.0;
        if self.brightness.overlay {
            self.snapshot.brightness_percent = None;
        }
    }

    fn cancel(&mut self) {
        if self.settings.restore_on_cancel_or_wake {
            self.restore();
        }
        let keep = self.settings.restore_on_cancel_or_wake;
        self.brightness = self.brightness.reset(keep);
        self.volume = self.volume.reset(keep);
        if !keep {
            self.restore_after = None;
        }
        self.snapshot.state = TimerState::Idle;
        self.snapshot.total = Duration::ZERO;
        self.snapshot.remaining = Duration::ZERO;
        self.snapshot.ends_at = None;
    }

    fn report_error(&mut self, context: &str, error: anyhow::Error) {
        let message = format!("{context}: {error:#}");
        log::warn!("{message}");
        self.snapshot.last_error = Some(message);
    }

    fn publish(&self) {
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *shared != self.snapshot {
            let wake = self.should_wake_ui(&shared);
            *shared = self.snapshot.clone();
            drop(shared);
            if wake {
                (self.repaint)();
            }
        }
    }

    /// A visible window tracks every snapshot change, including the progress ring.
    /// A hidden one only needs a frame when the menu-bar title or overlay changes.
    fn should_wake_ui(&self, previous: &Snapshot) -> bool {
        if self.window_visible.load(Ordering::Relaxed) {
            return true;
        }
        previous.state != self.snapshot.state
            || previous.menu_seconds() != self.snapshot.menu_seconds()
            || previous.overlay_alpha.to_bits() != self.snapshot.overlay_alpha.to_bits()
    }
}

fn valid_level(level: f32) -> anyhow::Result<f32> {
    anyhow::ensure!(level.is_finite(), "backend returned a non-finite level");
    Ok(level.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        platform::fallback::{MockBrightness, MockPower, MockVolume},
        settings::{EndAction, FadeSettings},
    };
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct FakeClock(Arc<Mutex<ClockReading>>);

    impl FakeClock {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(ClockReading {
                monotonic: Duration::ZERO,
                wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            })))
        }

        fn advance(&self, delta: Duration) {
            let mut reading = self.0.lock().unwrap();
            reading.monotonic += delta;
            reading.wall += delta;
        }

        fn jump_wall(&self, seconds: u64) {
            self.0.lock().unwrap().wall += Duration::from_secs(seconds);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> ClockReading {
            *self.0.lock().unwrap()
        }
    }

    struct Harness {
        engine: Engine<FakeClock>,
        clock: FakeClock,
        brightness: MockBrightness,
        volume: MockVolume,
        power: MockPower,
    }

    fn preferences() -> Settings {
        Settings {
            dim: FadeSettings {
                enabled: true,
                start_before_min: 2,
                over_min: 1,
                target_percent: 20,
            },
            volume: FadeSettings {
                enabled: true,
                start_before_min: 1,
                over_min: 1,
                target_percent: 20,
            },
            ..Settings::default()
        }
    }

    #[test]
    fn unavailable_saved_hibernate_falls_back_to_sleep() {
        let mut settings = preferences();
        settings.end_action = EndAction::Hibernate;
        let mut h = Harness::new(settings, true);
        assert!(!h.engine.snapshot().hibernate_supported);
        h.start();
        h.advance(180);
        assert_eq!(h.power.state.lock().unwrap().calls, vec![EndAction::Sleep]);
    }

    #[test]
    fn unavailable_hibernate_cannot_be_reintroduced_by_settings_updates() {
        let mut h = Harness::new(preferences(), true);
        let mut settings = preferences();
        settings.end_action = EndAction::Hibernate;
        h.engine.command(Command::UpdateSettings(settings));
        h.start();
        h.advance(180);
        assert_eq!(h.power.state.lock().unwrap().calls, vec![EndAction::Sleep]);
    }

    #[test]
    fn available_saved_hibernate_remains_selected_and_executes() {
        let mut settings = preferences();
        settings.end_action = EndAction::Hibernate;
        let mut h = Harness::with_hibernate(settings, true, true);
        assert!(h.engine.snapshot().hibernate_supported);
        h.start();
        h.advance(180);
        assert_eq!(
            h.power.state.lock().unwrap().calls,
            vec![EndAction::Hibernate]
        );
    }

    impl Harness {
        fn new(settings: Settings, brightness_supported: bool) -> Self {
            Self::with_hibernate(settings, brightness_supported, false)
        }

        fn with_hibernate(
            settings: Settings,
            brightness_supported: bool,
            hibernate_supported: bool,
        ) -> Self {
            let clock = FakeClock::new();
            let brightness = MockBrightness::new(0.8);
            let volume = MockVolume::new(0.6);
            let power = MockPower::default();
            let backends = Backends {
                brightness: brightness_supported
                    .then(|| Box::new(brightness.clone()) as Box<dyn crate::platform::Brightness>),
                volume: Some(Box::new(volume.clone())),
                power: Box::new(power.clone()),
                hibernate_supported,
            };
            let engine = Engine::new(settings, backends, clock.clone());
            Self {
                engine,
                clock,
                brightness,
                volume,
                power,
            }
        }

        fn start(&mut self) {
            self.engine.command(Command::Start {
                duration: Duration::from_secs(180),
            });
        }

        fn advance(&mut self, seconds: u64) {
            let mut remaining = seconds;
            while remaining > 0 {
                let step = remaining.min(1);
                self.clock.advance(Duration::from_secs(step));
                self.engine.tick();
                remaining -= step;
            }
        }

        fn brightness(&self) -> f32 {
            self.brightness.state.lock().unwrap().level
        }
        fn volume(&self) -> f32 {
            self.volume.state.lock().unwrap().level
        }
    }

    fn close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 0.0001, "{actual} != {expected}");
    }

    #[test]
    fn captures_at_each_fade_start_and_finishes_once() {
        let mut h = Harness::new(preferences(), true);
        h.start();
        h.advance(59);
        assert_eq!(h.brightness.state.lock().unwrap().get_calls, 0);
        // Capture a user change made before fading, rather than the launch-time value.
        h.brightness.state.lock().unwrap().level = 1.0;
        h.advance(1);
        assert_eq!(h.brightness.state.lock().unwrap().get_calls, 1);
        assert_eq!(h.volume.state.lock().unwrap().get_calls, 0);
        h.advance(30);
        close(h.brightness(), 0.6);
        h.advance(30);
        close(h.brightness(), 0.2);
        assert_eq!(h.volume.state.lock().unwrap().get_calls, 1);
        h.advance(30);
        close(h.volume(), 0.4);
        h.advance(30);
        close(h.volume(), 0.2);
        assert_eq!(h.engine.snapshot().state, TimerState::Finishing);
        assert_eq!(h.engine.snapshot().remaining, Duration::ZERO);
        h.advance(10);
        assert_eq!(h.power.state.lock().unwrap().calls, vec![EndAction::Sleep]);
    }

    #[test]
    fn cancel_restores_captured_originals_and_clears_timer() {
        let mut h = Harness::new(preferences(), true);
        h.start();
        h.advance(150);
        h.engine.command(Command::Cancel);
        close(h.brightness(), 0.8);
        close(h.volume(), 0.6);
        assert_eq!(h.engine.snapshot().state, TimerState::Idle);
        assert_eq!(h.engine.snapshot().remaining, Duration::ZERO);
        assert_eq!(h.engine.snapshot().ends_at, None);
        assert!(h.power.state.lock().unwrap().calls.is_empty());
    }

    #[test]
    fn pause_freezes_countdown_and_fades_until_resume() {
        let mut h = Harness::new(preferences(), true);
        h.start();
        h.advance(90);
        h.engine.command(Command::Pause);
        assert_eq!(h.engine.snapshot().ends_at, None);
        h.advance(120);
        assert_eq!(h.engine.snapshot().remaining, Duration::from_secs(90));
        close(h.brightness(), 0.5);
        assert_eq!(h.engine.snapshot().state, TimerState::Paused);
        h.engine.command(Command::Resume);
        assert!(h.engine.snapshot().ends_at.is_some());
        h.advance(30);
        assert_eq!(h.engine.snapshot().remaining, Duration::from_secs(60));
        close(h.brightness(), 0.2);
    }

    #[test]
    fn adding_minutes_moves_deadline_without_restarting_or_raising_fade() {
        let mut h = Harness::new(preferences(), true);
        h.start();
        h.advance(90);
        let old_end = h.engine.snapshot().ends_at.unwrap();
        h.engine.command(Command::AddMinutes(1));
        assert_eq!(h.engine.snapshot().total, Duration::from_secs(240));
        assert_eq!(h.engine.snapshot().remaining, Duration::from_secs(150));
        assert_eq!(
            h.engine.snapshot().ends_at.unwrap() - old_end,
            chrono::TimeDelta::minutes(1)
        );
        h.advance(30);
        close(h.brightness(), 0.5);
        h.advance(60);
        close(h.brightness(), 0.2);
        h.engine.command(Command::Cancel);
        close(h.brightness(), 0.8);
    }

    #[test]
    fn adding_minutes_while_paused_keeps_it_paused() {
        let mut h = Harness::new(preferences(), true);
        h.start();
        h.advance(30);
        h.engine.command(Command::Pause);
        h.engine.command(Command::AddMinutes(2));
        h.advance(10);
        assert_eq!(h.engine.snapshot().state, TimerState::Paused);
        assert_eq!(h.engine.snapshot().remaining, Duration::from_secs(270));
        assert_eq!(h.engine.snapshot().ends_at, None);
    }

    #[test]
    fn settings_changes_apply_without_losing_originals_or_raising_levels() {
        let mut h = Harness::with_hibernate(preferences(), true, true);
        h.start();
        h.advance(90);
        let mut settings = preferences();
        settings.dim.target_percent = 0;
        settings.end_action = EndAction::Hibernate;
        h.engine.command(Command::UpdateSettings(settings.clone()));
        close(h.brightness(), 0.4);
        settings.dim.target_percent = 90;
        h.engine.command(Command::UpdateSettings(settings));
        close(h.brightness(), 0.4);
        h.advance(90);
        assert_eq!(
            h.power.state.lock().unwrap().calls,
            vec![EndAction::Hibernate]
        );
        h.engine.command(Command::Cancel);
        close(h.brightness(), 0.8);
    }

    #[test]
    fn missing_brightness_drives_overlay_and_cancel_removes_it() {
        let mut h = Harness::new(preferences(), false);
        h.start();
        h.advance(60);
        close(h.engine.snapshot().overlay_alpha, 0.0);
        h.advance(30);
        close(h.engine.snapshot().overlay_alpha, 0.36);
        assert!(!h.engine.snapshot().brightness_supported);
        h.advance(30);
        close(h.engine.snapshot().overlay_alpha, 0.72);
        h.engine.command(Command::Cancel);
        close(h.engine.snapshot().overlay_alpha, 0.0);
    }

    #[test]
    fn brightness_get_failure_uses_overlay_and_reports_error() {
        let mut h = Harness::new(preferences(), true);
        h.brightness.state.lock().unwrap().fail_get = true;
        h.start();
        h.advance(90);
        close(h.engine.snapshot().overlay_alpha, 0.36);
        assert!(!h.engine.snapshot().brightness_supported);
        assert!(h.engine.snapshot().last_error.is_some());
        assert_eq!(h.brightness.state.lock().unwrap().get_calls, 1);
    }

    #[test]
    fn short_sleep_after_finishing_restores() {
        let mut h = Harness::new(preferences(), true);
        h.start();
        h.advance(180);
        h.clock.jump_wall(6);
        h.engine.tick();
        assert_eq!(h.engine.snapshot().state, TimerState::Idle);
        close(h.brightness(), 0.8);
        close(h.volume(), 0.6);
    }

    #[test]
    fn sub_five_second_gap_while_finishing_keeps_waiting() {
        let mut h = Harness::new(preferences(), true);
        h.start();
        h.advance(180);
        h.clock.jump_wall(4);
        h.engine.tick();
        assert_eq!(h.engine.snapshot().state, TimerState::Finishing);
        close(h.brightness(), 0.2);
        close(h.volume(), 0.2);
        assert_eq!(h.power.state.lock().unwrap().calls.len(), 1);
    }

    #[test]
    fn short_wall_gap_while_running_or_paused_is_not_a_wake() {
        for pause in [false, true] {
            let mut h = Harness::new(preferences(), true);
            h.start();
            h.advance(90);
            if pause {
                h.engine.command(Command::Pause);
            }
            let sets = h.brightness.state.lock().unwrap().set_calls.len();
            h.clock.jump_wall(20);
            h.engine.tick();
            assert_eq!(
                h.engine.snapshot().state,
                if pause {
                    TimerState::Paused
                } else {
                    TimerState::Running
                }
            );
            assert_eq!(h.brightness.state.lock().unwrap().set_calls.len(), sets);
            assert!(h.power.state.lock().unwrap().calls.is_empty());
        }
    }

    #[test]
    fn native_wake_ends_running_or_finishing_and_duplicate_detection_is_harmless() {
        for elapsed in [90, 180] {
            let mut h = Harness::new(preferences(), true);
            h.start();
            h.advance(elapsed);
            h.engine.command(Command::SystemWoke);
            assert_eq!(h.engine.snapshot().state, TimerState::Idle);
            close(h.brightness(), 0.8);
            close(h.volume(), 0.6);
            let calls = h.brightness.state.lock().unwrap().set_calls.len();
            h.clock.jump_wall(6);
            h.engine.command(Command::SystemWoke);
            h.engine.tick();
            assert_eq!(h.brightness.state.lock().unwrap().set_calls.len(), calls);
            assert_eq!(
                h.power.state.lock().unwrap().calls.len(),
                usize::from(elapsed == 180)
            );
        }
    }

    #[test]
    fn wake_restore_failure_is_retried() {
        let mut h = Harness::new(preferences(), true);
        h.start();
        h.advance(180);
        h.volume.state.lock().unwrap().fail_set = true;
        h.engine.command(Command::SystemWoke);
        h.volume.state.lock().unwrap().fail_set = false;
        h.clock.advance(Duration::from_secs(5));
        h.engine.tick();
        close(h.volume(), 0.6);
    }

    #[test]
    fn failed_restores_retry_with_backoff_and_keep_each_original() {
        let mut h = Harness::new(preferences(), true);
        h.start();
        h.advance(150);
        h.brightness.state.lock().unwrap().fail_set = true;
        h.volume.state.lock().unwrap().fail_set = true;
        h.engine.command(Command::Cancel);
        let calls = h.brightness.state.lock().unwrap().set_calls.len();
        for _ in 0..16 {
            h.clock.advance(Duration::from_millis(250));
            h.engine.tick();
        }
        assert_eq!(h.brightness.state.lock().unwrap().set_calls.len(), calls);
        h.brightness.state.lock().unwrap().fail_set = false;
        h.clock.advance(Duration::from_secs(1));
        h.engine.tick();
        close(h.brightness(), 0.8);
        assert_eq!(h.engine.brightness.original, None);
        assert_eq!(h.engine.volume.original, Some(0.6));
        h.volume.state.lock().unwrap().fail_set = false;
        h.clock.advance(Duration::from_secs(5));
        h.engine.tick();
        close(h.volume(), 0.6);
        assert_eq!(h.engine.volume.original, None);
    }

    #[test]
    fn restart_during_failed_restore_preserves_originals_and_baselines() {
        let mut h = Harness::new(preferences(), true);
        h.start();
        h.advance(150);
        h.brightness.state.lock().unwrap().fail_set = true;
        h.volume.state.lock().unwrap().fail_set = true;
        h.engine.command(Command::Cancel);
        h.start();
        let brightness_gets = h.brightness.state.lock().unwrap().get_calls;
        let volume_gets = h.volume.state.lock().unwrap().get_calls;
        h.brightness.state.lock().unwrap().fail_set = false;
        h.volume.state.lock().unwrap().fail_set = false;
        h.advance(150);
        assert_eq!(
            h.brightness.state.lock().unwrap().get_calls,
            brightness_gets
        );
        assert_eq!(h.volume.state.lock().unwrap().get_calls, volume_gets);
        h.engine.command(Command::Cancel);
        close(h.brightness(), 0.8);
        close(h.volume(), 0.6);
    }

    #[test]
    fn shutdown_retries_pending_restoration_immediately() {
        let mut h = Harness::new(preferences(), true);
        h.start();
        h.advance(150);
        h.brightness.state.lock().unwrap().fail_set = true;
        h.engine.command(Command::Cancel);
        h.brightness.state.lock().unwrap().fail_set = false;
        assert!(!h.engine.command(Command::Shutdown));
        close(h.brightness(), 0.8);
    }

    #[test]
    fn wake_after_end_action_restores_and_does_not_repeat_action() {
        let mut h = Harness::new(preferences(), true);
        h.start();
        h.advance(180);
        h.clock.jump_wall(3600);
        h.engine.tick();
        assert_eq!(h.engine.snapshot().state, TimerState::Idle);
        close(h.brightness(), 0.8);
        close(h.volume(), 0.6);
        h.advance(10);
        assert_eq!(h.power.state.lock().unwrap().calls.len(), 1);
    }

    #[test]
    fn wake_during_run_restores_and_stops_timer() {
        let mut h = Harness::new(preferences(), true);
        h.start();
        h.advance(90);
        h.clock.jump_wall(31);
        h.engine.tick();
        assert_eq!(h.engine.snapshot().state, TimerState::Idle);
        close(h.brightness(), 0.8);
        assert!(h.power.state.lock().unwrap().calls.is_empty());
    }

    #[test]
    fn restoration_setting_is_honored_on_cancel_and_wake() {
        for wake in [false, true] {
            let mut settings = preferences();
            settings.restore_on_cancel_or_wake = false;
            let mut h = Harness::new(settings, true);
            h.start();
            h.advance(150);
            if wake {
                h.clock.jump_wall(60);
                h.engine.tick();
            } else {
                h.engine.command(Command::Cancel);
            }
            close(h.brightness(), 0.2);
            close(h.volume(), 0.4);
            assert_eq!(h.engine.snapshot().state, TimerState::Idle);
        }
    }

    #[test]
    fn power_failure_restores_and_reports_error_instead_of_staying_finishing() {
        let mut h = Harness::new(preferences(), true);
        h.power.state.lock().unwrap().fail = true;
        h.start();
        h.advance(180);
        assert_eq!(h.engine.snapshot().state, TimerState::Idle);
        assert!(h.engine.snapshot().last_error.is_some());
        close(h.brightness(), 0.8);
        close(h.volume(), 0.6);
        h.advance(10);
        assert_eq!(h.power.state.lock().unwrap().calls.len(), 1);
    }

    #[test]
    fn zero_duration_finishes_once_and_shutdown_restores() {
        let mut h = Harness::new(preferences(), true);
        h.engine.command(Command::Start {
            duration: Duration::ZERO,
        });
        h.engine.tick();
        assert_eq!(h.power.state.lock().unwrap().calls.len(), 1);
        assert!(!h.engine.command(Command::Shutdown));
        close(h.brightness(), 0.8);
        close(h.volume(), 0.6);
    }

    #[test]
    fn pause_arriving_at_zero_cannot_prevent_end_action() {
        let mut h = Harness::new(preferences(), true);
        h.engine.command(Command::Start {
            duration: Duration::from_secs(10),
        });
        h.clock.advance(Duration::from_secs(10));
        h.engine.command(Command::Pause);
        assert_eq!(h.engine.snapshot().state, TimerState::Finishing);
        assert_eq!(h.power.state.lock().unwrap().calls, vec![EndAction::Sleep]);
    }

    #[test]
    fn disabled_fades_do_not_read_or_write_levels() {
        let mut settings = preferences();
        settings.dim.enabled = false;
        settings.volume.enabled = false;
        let mut h = Harness::new(settings, true);
        h.start();
        h.advance(180);
        assert_eq!(h.brightness.state.lock().unwrap().get_calls, 0);
        assert_eq!(h.volume.state.lock().unwrap().get_calls, 0);
        assert!(h.brightness.state.lock().unwrap().set_calls.is_empty());
        assert!(h.volume.state.lock().unwrap().set_calls.is_empty());
        assert_eq!(h.power.state.lock().unwrap().calls.len(), 1);
    }

    #[test]
    fn target_above_original_never_raises_hardware_levels() {
        let mut settings = preferences();
        settings.dim.target_percent = 90;
        settings.volume.target_percent = 90;
        let mut h = Harness::new(settings, true);
        h.start();
        h.advance(180);
        close(h.brightness(), 0.8);
        close(h.volume(), 0.6);
        assert!(h.brightness.state.lock().unwrap().set_calls.is_empty());
        assert!(h.volume.state.lock().unwrap().set_calls.is_empty());
    }

    #[test]
    fn failed_volume_read_is_not_retried_each_tick_or_restored() {
        let mut h = Harness::new(preferences(), true);
        h.volume.state.lock().unwrap().fail_get = true;
        h.start();
        h.advance(150);
        assert!(!h.engine.snapshot().volume_supported);
        assert_eq!(h.engine.snapshot().volume_percent, None);
        assert_eq!(h.volume.state.lock().unwrap().get_calls, 1);
        h.engine.command(Command::Cancel);
        assert!(h.volume.state.lock().unwrap().set_calls.is_empty());
        close(h.brightness(), 0.8);
    }

    #[test]
    fn brightness_write_failure_switches_to_overlay_and_retains_original() {
        let mut h = Harness::new(preferences(), true);
        h.brightness.state.lock().unwrap().fail_set = true;
        h.start();
        h.advance(90);
        assert!(!h.engine.snapshot().brightness_supported);
        assert!(h.engine.snapshot().overlay_alpha > 0.0);
        h.brightness.state.lock().unwrap().fail_set = false;
        h.engine.command(Command::Cancel);
        close(h.brightness(), 0.8);
        close(h.engine.snapshot().overlay_alpha, 0.0);
    }

    #[test]
    fn overlay_never_exceeds_point_nine_at_zero_target() {
        let mut settings = preferences();
        settings.dim.target_percent = 0;
        let mut h = Harness::new(settings, false);
        h.start();
        h.advance(120);
        close(h.engine.snapshot().overlay_alpha, 0.9);
    }

    #[test]
    fn backwards_wall_clock_does_not_change_monotonic_countdown() {
        let mut h = Harness::new(preferences(), true);
        h.start();
        h.clock.advance(Duration::from_secs(10));
        h.clock.0.lock().unwrap().wall -= Duration::from_secs(3600);
        h.engine.tick();
        assert_eq!(h.engine.snapshot().remaining, Duration::from_secs(170));
        assert_eq!(h.engine.snapshot().state, TimerState::Running);
    }

    #[test]
    fn worker_processes_commands_and_last_handle_drop_restores() {
        let brightness = MockBrightness::new(0.8);
        let volume = MockVolume::new(0.6);
        let power = MockPower::default();
        let (repaint_sender, repaint_receiver) = mpsc::channel();
        let handle = Engine::spawn(
            preferences(),
            Backends {
                brightness: Some(Box::new(brightness.clone())),
                volume: Some(Box::new(volume.clone())),
                power: Box::new(power.clone()),
                hibernate_supported: false,
            },
            Box::new(move || {
                let _ = repaint_sender.send(());
            }),
        )
        .unwrap();
        let clone = handle.clone();
        drop(handle);
        clone
            .send(Command::Start {
                duration: Duration::ZERO,
            })
            .unwrap();
        repaint_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(clone.snapshot().state, TimerState::Finishing);
        drop(clone);
        assert_eq!(power.state.lock().unwrap().calls, vec![EndAction::Sleep]);
        close(brightness.state.lock().unwrap().level, 0.8);
        close(volume.state.lock().unwrap().level, 0.6);
    }
    #[test]
    fn accelerated_clock_uses_real_end_time_and_real_wake_detection() {
        let clock = FakeClock::new();
        let wall = clock.now().wall;
        let power = MockPower::default();
        let mut engine = Engine::new(
            preferences(),
            Backends {
                brightness: None,
                volume: None,
                power: Box::new(power.clone()),
                hibernate_supported: false,
            },
            SpeedClock::new(clock.clone(), 60.0).unwrap(),
        );
        engine.command(Command::Start {
            duration: Duration::from_secs(900),
        });
        assert_eq!(
            SystemTime::from(engine.snapshot().ends_at.unwrap()),
            wall + Duration::from_secs(15)
        );
        clock.advance(Duration::from_secs(5));
        engine.tick();
        assert_eq!(engine.snapshot().remaining, Duration::from_secs(600));
        assert_eq!(engine.snapshot().state, TimerState::Running);
        clock.advance(Duration::from_secs(10));
        engine.tick();
        assert_eq!(engine.snapshot().state, TimerState::Finishing);
        assert_eq!(power.state.lock().unwrap().calls, vec![EndAction::Sleep]);
        clock.jump_wall(31);
        engine.tick();
        assert_eq!(engine.snapshot().state, TimerState::Idle);
        for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(SpeedClock::new(clock.clone(), invalid).is_err());
        }
    }

    #[test]
    fn idle_timer_waits_for_a_command_and_a_running_timer_keeps_ticking() {
        let mut engine = Engine::new(
            preferences(),
            Backends {
                brightness: None,
                volume: None,
                power: Box::new(MockPower::default()),
                hibernate_supported: false,
            },
            SystemClock::default(),
        );
        assert!(!engine.needs_tick());
        assert!(engine.command(Command::Start {
            duration: Duration::from_secs(60),
        }));
        assert!(engine.needs_tick());
        assert!(engine.command(Command::Pause));
        assert!(engine.needs_tick());
        assert!(engine.command(Command::Cancel));
        assert!(!engine.needs_tick());
    }

    #[test]
    fn hidden_window_skips_subsecond_updates_and_wakes_for_the_menu_bar_and_overlay() {
        let mut engine = Engine::new(
            preferences(),
            Backends {
                brightness: None,
                volume: None,
                power: Box::new(MockPower::default()),
                hibernate_supported: false,
            },
            SystemClock::default(),
        );
        engine
            .window_visible
            .store(false, std::sync::atomic::Ordering::Relaxed);
        engine.snapshot.state = TimerState::Running;
        engine.snapshot.remaining = Duration::from_millis(10_500);
        let previous = engine.snapshot.clone();

        engine.snapshot.remaining = Duration::from_millis(10_200);
        assert!(!engine.should_wake_ui(&previous));

        engine.snapshot.remaining = Duration::from_millis(10_000);
        assert!(engine.should_wake_ui(&previous));

        engine.snapshot.remaining = previous.remaining;
        engine.snapshot.overlay_alpha = 0.25;
        assert!(engine.should_wake_ui(&previous));

        engine.snapshot.overlay_alpha = previous.overlay_alpha;
        engine.snapshot.brightness_percent = Some(40.0);
        assert!(!engine.should_wake_ui(&previous));

        engine
            .window_visible
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(engine.should_wake_ui(&previous));
    }

    #[test]
    fn shutdown_during_fade_restores_without_performing_power_action() {
        let mut h = Harness::new(preferences(), true);
        h.start();
        h.advance(150);
        assert!(!h.engine.command(Command::Shutdown));
        close(h.brightness(), 0.8);
        close(h.volume(), 0.6);
        assert!(h.power.state.lock().unwrap().calls.is_empty());
    }
}
