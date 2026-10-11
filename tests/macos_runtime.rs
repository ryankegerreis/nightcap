//! Opt-in real hardware QA. Run alone; end actions are always replaced by DryRunPower.
#![cfg(target_os = "macos")]

use anyhow::{Result, ensure};
use nightcap::{
    engine::{Clock, ClockReading, Command, Engine, TimerState},
    platform::{self, Backends},
    settings::{EndAction, Settings},
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

#[derive(Clone)]
struct TestClock(Arc<Mutex<ClockReading>>);
impl Clock for TestClock {
    fn now(&self) -> ClockReading {
        *self.0.lock().unwrap()
    }
}
impl TestClock {
    fn advance(&self, seconds: u64) {
        let mut time = self.0.lock().unwrap();
        time.monotonic += Duration::from_secs(seconds);
        // Keep wake detection separate from accelerated timer time.
        time.wall += Duration::from_millis(250);
    }
}
struct RestoreHardware {
    controls: Backends,
    brightness: f32,
    volume: f32,
}
impl RestoreHardware {
    fn read(&mut self) -> Result<(f32, f32)> {
        Ok((
            self.controls.brightness.as_mut().unwrap().get()?,
            self.controls.volume.as_mut().unwrap().get()?,
        ))
    }
    fn assert_restored(&mut self) -> Result<()> {
        let (brightness, volume) = self.read()?;
        ensure!(
            (brightness - self.brightness).abs() < 0.003,
            "brightness not restored: {brightness}"
        );
        ensure!(
            (volume - self.volume).abs() < 0.003,
            "volume not restored: {volume}"
        );
        Ok(())
    }
}
impl Drop for RestoreHardware {
    fn drop(&mut self) {
        let _ = self
            .controls
            .brightness
            .as_mut()
            .unwrap()
            .set(self.brightness);
        let _ = self.controls.volume.as_mut().unwrap().set(self.volume);
    }
}

#[test]
#[ignore = "changes real macOS brightness and volume; run alone; never requests sleep or shutdown"]
fn native_fades_pause_extend_cancel_wake_and_quit() -> Result<()> {
    let mut controls = platform::backends();
    let brightness = controls
        .brightness
        .as_mut()
        .expect("requires a controllable display")
        .get()?;
    let volume = controls
        .volume
        .as_mut()
        .expect("requires software output volume")
        .get()?;
    let mut restore = RestoreHardware {
        controls,
        brightness,
        volume,
    };
    println!("original brightness={brightness:.6}, volume={volume:.6}");
    let clock = TestClock(Arc::new(Mutex::new(ClockReading {
        monotonic: Duration::ZERO,
        wall: SystemTime::now(),
    })));
    let mut settings = Settings::default();
    ensure!(
        brightness > 0.01 && volume > 0.01,
        "requires nonzero starting levels to observe a fade"
    );
    settings.dim.target_percent = (brightness * 50.0).floor() as u8;
    settings.volume.target_percent = (volume * 50.0).floor() as u8;
    let mut backends = platform::backends();
    backends.power = Box::new(platform::DryRunPower);
    let mut engine = Engine::new(settings.clone(), backends, clock.clone());
    let start = || Command::Start {
        duration: Duration::from_secs(900),
    };
    engine.command(start());
    clock.advance(300);
    engine.tick();
    let (dimmed, lowered) = restore.read()?;
    ensure!(
        dimmed < brightness && lowered < volume,
        "both native fades must lower levels"
    );
    println!("faded brightness={dimmed:.6}, volume={lowered:.6}");
    engine.command(Command::Pause);
    let paused = engine.snapshot();
    clock.advance(120);
    engine.tick();
    ensure!(
        engine.snapshot().remaining == paused.remaining,
        "pause changed remaining time"
    );
    let frozen = restore.read()?;
    ensure!(
        (frozen.0 - dimmed).abs() < 0.003 && (frozen.1 - lowered).abs() < 0.003,
        "pause changed levels"
    );
    engine.command(Command::AddMinutes(15));
    ensure!(
        engine.snapshot().remaining == paused.remaining + Duration::from_secs(900),
        "extension lost time"
    );
    engine.command(Command::Resume);
    engine.command(Command::Cancel);
    restore.assert_restored()?;
    for action in [EndAction::Sleep, EndAction::Shutdown] {
        settings.end_action = action;
        engine.command(Command::UpdateSettings(settings.clone()));
        engine.command(start());
        clock.advance(900);
        engine.tick();
        ensure!(
            engine.snapshot().state == TimerState::Finishing,
            "end action did not finish"
        );
        engine.command(Command::SystemWoke);
        ensure!(
            engine.snapshot().state == TimerState::Idle,
            "wake did not end timer"
        );
        restore.assert_restored()?;
    }
    settings.restore_on_cancel_or_wake = false;
    engine.command(Command::UpdateSettings(settings.clone()));
    engine.command(start());
    clock.advance(300);
    engine.tick();
    engine.command(Command::Cancel);
    let retained = restore.read()?;
    ensure!(
        retained.0 < brightness && retained.1 < volume,
        "restore-off cancel raised levels"
    );
    restore
        .controls
        .brightness
        .as_mut()
        .unwrap()
        .set(brightness)?;
    restore.controls.volume.as_mut().unwrap().set(volume)?;
    settings.restore_on_cancel_or_wake = true;
    engine.command(Command::UpdateSettings(settings));
    engine.command(start());
    clock.advance(300);
    engine.tick();
    ensure!(
        !engine.command(Command::Shutdown),
        "quit did not stop worker"
    );
    restore.assert_restored()?;
    println!("cancel, wake, restore-off and quit verified; original levels restored");
    Ok(())
}
