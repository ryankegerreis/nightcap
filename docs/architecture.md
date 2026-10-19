# Nightcap architecture

Nightcap is a single Rust binary (eframe/egui 0.36). The design source of truth is
`docs/design/mockup.html` (open it, read its CSS/JS) plus `docs/design/*.png`.
Follow the system light/dark appearance, as the mockup does.
Windows selects eframe's OpenGL renderer for transparent and hidden viewports.

## Modules (one owner per file during parallel work)

```
src/
  main.rs            app entry: builds NativeOptions, wires engine + tray + UI
  settings.rs        Settings struct (serde), load/save JSON in the OS config dir
  fade.rs            pure fade math: level at time t for a fade plan (unit tested)
  engine.rs          timer engine thread: state machine, drives fades, fires the end action
  platform/
    mod.rs           traits Brightness, Volume, Power + `fn backends() -> Backends`
    macos.rs         macOS impls
    windows.rs       Windows impls
    linux.rs         Linux impls
    fallback.rs      no-op / unsupported impls used when a backend is unavailable
  tray.rs            system tray / menu bar icon + menu
  ui/
    mod.rs           NightcapApp: eframe::App, routes between main window and settings
    theme.rs         colors, fonts, spacing tokens taken from the mockup (light + dark)
    widgets.rs       custom widgets: progress ring, segmented control, toggle switch,
                     grouped list rows, stepper/select rows, round Start/Cancel buttons
    main_window.rs   the main timer window
    settings_window.rs  Settings window (separate viewport, opened by gear icon / Cmd+,)
    overlay.rs       fallback dimming overlay: fullscreen, borderless, transparent,
                     always-on-top, mouse-passthrough black viewport with variable alpha
```

## Core types (in `settings.rs` / `engine.rs`)

- `Settings { dim: FadeSettings, volume: FadeSettings, end_action: EndAction,
  restore_on_cancel_or_wake: bool, show_time_in_menu_bar: bool, last_duration_min: u32 }`
- `FadeSettings { enabled: bool, start_before_min: u32, over_min: u32, target_percent: u8 }`
  Defaults: dim = enabled, 20 min before, over 15 min, to 10 %; volume = enabled,
  15 min before, over 10 min, to 20 %.
- `EndAction { Sleep, Hibernate, Shutdown }` (default Sleep).
- Fades start at `end - start_before` and run for `min(over, start_before)`, then hold at
  target until the end. If the timer is shorter than `start_before`, the fade starts
  immediately and its length is scaled to fit. Interpolation is linear in perceived
  level from the level captured when the fade begins down to the target.

## Engine

`Engine` runs on its own thread so fades and the end action happen even when the
window is hidden and egui is not repainting. It owns the platform backends.

- Commands (via `std::sync::mpsc`): `Start { duration }`, `Pause`, `Resume`, `Cancel`,
  `AddMinutes(u32)`, `UpdateSettings(Settings)`, `SystemWoke`, `Shutdown`.
- Shared snapshot (`Arc<Mutex<Snapshot>>`): `state: Idle | Running | Paused | Finishing`,
  `total`, `remaining`, `ends_at` (wall clock, for "10:20 PM"), current brightness /
  volume percent applied, `overlay_alpha: f32` (0 = no overlay), backend capability flags
  (`brightness_supported`, `volume_supported`, `hibernate_supported`), last error string.
  Hibernate availability is checked once at startup and includes permission to request it.
- Ticks about every 250 ms. Uses `Instant` for remaining time; captures original
  brightness/volume when a fade begins and restores them on Cancel (if the restore
  setting is on), and after the end action when the machine wakes (detect a wall-clock
  gap over 5 s while Finishing (30 s while Running/Paused), plus native macOS workspace wake notifications).
- If hardware brightness is unsupported, the dim fade drives `overlay_alpha` instead
  (max alpha 0.9 at target 0 %, mapped linearly).
  A failed native write rebases overlay opacity against the last successful native
  level, avoiding an abrupt absolute-brightness jump while retaining restoration.
  A `platform::Partial` error means some displays took the level, so native fading
  continues on those displays instead.
- A failed restore keeps its original and retries every 5 s, for up to 24 attempts.
  A `platform::Disconnected` error drops that original, so the next fade captures
  the new device. A timer started while a restore is pending fades from the original.
- Calls a `repaint: Box<dyn Fn() + Send>` hook after each snapshot change so the UI and
  tray refresh.

## Platform traits (`platform/mod.rs`)

```rust
pub trait Brightness: Send { fn get(&mut self) -> Result<f32>; fn set(&mut self, level: f32) -> Result<()>; }
pub trait Volume: Send { fn get(&mut self) -> Result<f32>; fn set(&mut self, level: f32) -> Result<()>; }
pub trait Power: Send { fn perform(&mut self, action: EndAction) -> Result<()>; }
```
Levels are `0.0..=1.0`. `Result` is `anyhow::Result`. Native OS APIs are preferred;
well-known OS command-line tools are an acceptable fallback (e.g. `pmset sleepnow`,
`systemctl suspend`, `wpctl`/`pactl`/`amixer`, `/sys/class/backlight`).

## Tray

`tray-icon` on macOS and Windows. On Linux use `ksni` (StatusNotifierItem over D-Bus)
so no GTK is required. Menu matches the mockup: "Start Timer" header, 15/30/45 Minutes,
1 Hour, 90 Minutes; while running also Pause/Resume, +15 Minutes, Cancel; then
Open Nightcap, Settings…, Quit Nightcap. Title shows remaining time (macOS menu bar)
when `show_time_in_menu_bar` is on. Closing the main window hides it; the app keeps
running in the tray.
