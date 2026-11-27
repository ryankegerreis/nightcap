//! App routing between the main timer, settings viewport, and dimming overlay.

use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use eframe::egui;

use crate::engine::{Command, EngineHandle, TimerState};
use crate::settings::Settings;
use crate::tray::{Tray, TrayAction};
use crate::updates::{UpdateChecker, UpdateStatus};

use self::main_window::{MAX_MINUTES, MIN_MINUTES, PRESETS};
pub use self::window_lifecycle::Activation;
use self::window_lifecycle::{WindowLifecycle, WindowRequest};

pub mod main_window;
pub mod overlay;
pub mod settings_window;
pub mod theme;
pub mod widgets;
mod window_lifecycle;
#[cfg(target_os = "linux")]
mod x11_overlay;

const SAVE_DELAY: Duration = Duration::from_millis(300);
const NOTICE_DURATION: Duration = Duration::from_millis(4500);

pub struct NightcapApp {
    engine: EngineHandle,
    tray: Option<Tray>,
    updates: UpdateChecker,
    overlay: overlay::Overlay,
    /// What the engine last heard about the overlay.
    overlay_reported: Option<bool>,
    windows: WindowLifecycle,
    /// The last timer start this window has seen (`Snapshot::started`).
    observed_start: u64,
    settings: Settings,
    duration_min: u32,
    custom_duration: bool,
    save_after: Option<Instant>,
    notice: Option<StatusNote>,
    ctx: Option<egui::Context>,
    activations: Option<mpsc::Receiver<Activation>>,
    #[cfg(target_os = "macos")]
    _wake_observer: crate::platform::macos::WakeObserver,
}

struct StatusNote {
    text: String,
    until: Instant,
}

impl NightcapApp {
    pub fn new(
        engine: EngineHandle,
        settings: Settings,
        tray: Option<Tray>,
        updates: UpdateChecker,
    ) -> Self {
        window_lifecycle::set_activation_policy(true);
        let windows = WindowLifecycle::new(tray.is_some());
        let duration_min = settings.last_duration_min.clamp(MIN_MINUTES, MAX_MINUTES);
        let custom_duration = !PRESETS.contains(&duration_min);
        Self {
            #[cfg(target_os = "macos")]
            _wake_observer: crate::platform::macos::WakeObserver::new(engine.clone()),
            engine,
            tray,
            updates,
            overlay: overlay::Overlay::new(),
            overlay_reported: None,
            windows,
            observed_start: 0,
            settings,
            duration_min,
            custom_duration,
            save_after: None,
            notice: None,
            ctx: None,
            activations: None,
        }
    }

    /// Each request, from a later launch of Nightcap, shows and focuses the main
    /// window. Its reply says whether that was requested, which is false while quitting.
    pub fn with_activations(mut self, activations: mpsc::Receiver<Activation>) -> Self {
        self.activations = Some(activations);
        self
    }

    pub fn open_settings(&mut self) {
        let requests = self.windows.open_settings();
        if let Some(ctx) = self.ctx.clone() {
            self.perform(&ctx, requests);
        }
    }

    pub fn engine(&self) -> &EngineHandle {
        &self.engine
    }

    fn perform(&self, ctx: &egui::Context, requests: Vec<WindowRequest>) {
        for request in requests {
            window_lifecycle::perform(ctx, &self.engine, request);
        }
    }

    fn send(&self, command: Command) {
        if let Err(error) = self.engine.send(command) {
            log::warn!("Timer command failed: {error}");
        }
    }

    fn mark_settings_dirty(&mut self) {
        self.updates.set_enabled(self.settings.check_for_updates);
        if let Err(error) = self
            .engine
            .send(Command::UpdateSettings(self.settings.clone()))
        {
            log::warn!("Could not update the timer settings: {error}");
        }
        self.save_after = Some(Instant::now() + SAVE_DELAY);
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint_after(SAVE_DELAY);
        }
    }

    /// Applies what the Settings window asked for, returning whether a preference changed.
    fn settings_outcome(&mut self, outcome: settings_window::SettingsOutcome) -> bool {
        if outcome.check_now {
            self.updates.check_now();
        }
        if let Some(url) = outcome.download {
            crate::updates::open_in_browser(&url);
        }
        outcome.changed
    }

    fn flush_save(&mut self) {
        let Some(deadline) = self.save_after else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        self.save_after = None;
        if let Err(error) = self.settings.save() {
            log::warn!("Could not save settings: {error:#}");
        }
    }
}

impl eframe::App for NightcapApp {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        theme::install(ctx);
        self.ctx = Some(ctx.clone());
        self.engine.set_window_visible(self.windows.root_visible());
        self.flush_save();
        let snapshot = self.engine.snapshot();
        if let Some(minutes) = started_minutes(&mut self.observed_start, &snapshot) {
            self.duration_min = minutes;
            self.custom_duration = !PRESETS.contains(&self.duration_min);
            if self.settings.last_duration_min != self.duration_min {
                self.settings.last_duration_min = self.duration_min;
                self.mark_settings_dirty();
            }
        }
        let updates = self.updates.status();
        // Settings still reports a manual check while automatic checks are off.
        let offered = updates
            .available
            .as_ref()
            .filter(|_| self.settings.check_for_updates);
        let actions = if let Some(tray) = &mut self.tray {
            tray.update(&snapshot, self.settings.show_time_in_menu_bar, offered);
            tray.poll()
        } else {
            Vec::new()
        };
        for action in actions {
            let requests = match action {
                TrayAction::OpenWindow => self.windows.show_root(true),
                TrayAction::OpenSettings => self.windows.open_settings_from_tray(),
                TrayAction::Quit => self.windows.quit(),
            };
            self.perform(ctx, requests);
        }
        // Hidden input retains the last UI's key events: only consume shortcuts
        // while visible. Native app-menu shortcuts use the tray queue above.
        if self.windows.root_visible()
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::Q))
        {
            let requests = self.windows.quit();
            self.perform(ctx, requests);
        }
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        let requests = self.windows.close(close_requested, || {
            ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::W))
        });
        self.perform(ctx, requests);
        // After this pass's Quit and close handling, so a quitting app refuses.
        if let Some(activations) = &self.activations {
            let engine = &self.engine;
            self.windows
                .answer_activations(activations, Instant::now(), |requests| {
                    for request in requests {
                        window_lifecycle::perform(ctx, engine, request);
                    }
                });
        }
        let overlay_available = self.overlay.update(
            ctx,
            frame,
            snapshot.dimming.overlay_alpha,
            self.windows.root_visible(),
        );
        if self.overlay_reported != Some(overlay_available) {
            self.overlay_reported = Some(overlay_available);
            self.send(Command::OverlayAvailable(overlay_available));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        theme::install(ui.ctx());
        self.ctx = Some(ui.ctx().clone());
        self.engine.set_window_visible(self.windows.root_visible());
        self.flush_save();
        if self
            .notice
            .as_ref()
            .is_some_and(|note| Instant::now() >= note.until)
        {
            self.notice = None;
        }

        if ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
            self.open_settings();
        }

        let snapshot = self.engine.snapshot();
        let updates = self.updates.status();
        let tokens = theme::tokens(ui.ctx());
        ui.painter().rect_filled(ui.max_rect(), 0.0, tokens.window);
        self.overlay.register(ui.ctx());
        if !self.windows.root_visible() {
            let (open, opening) = self.windows.settings_flags();
            let outcome = settings_window::show(
                ui.ctx(),
                open,
                opening,
                &mut self.settings,
                settings_view(&snapshot, &updates),
            );
            if self.settings_outcome(outcome) {
                self.mark_settings_dirty();
            }
            return;
        }
        if let Some(note) = &self.notice {
            ui.ctx()
                .request_repaint_after(note.until.saturating_duration_since(Instant::now()));
        }

        let tokens = theme::tokens(ui.ctx());
        let notice = self.notice.as_ref().map(|note| note.text.clone());
        let mut outcome = main_window::MainOutcome {
            open_settings: false,
            command: None,
            settings_changed: false,
            notice: None,
        };
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(tokens.window))
            .show(ui, |ui| {
                outcome = main_window::show(
                    ui,
                    main_window::MainView {
                        snapshot: &snapshot,
                        settings: &mut self.settings,
                        duration_min: &mut self.duration_min,
                        custom: &mut self.custom_duration,
                        notice: notice.as_deref(),
                    },
                );
            });

        if outcome.open_settings {
            self.open_settings();
        }
        if let Some(text) = outcome.notice {
            self.notice = Some(StatusNote {
                text: text.to_owned(),
                until: Instant::now() + NOTICE_DURATION,
            });
            ui.ctx().request_repaint_after(NOTICE_DURATION);
        }
        let mut settings_changed = outcome.settings_changed;
        if let Some(command) = outcome.command {
            if let Command::Start { .. } = command {
                self.notice = None;
                if self.settings.last_duration_min != self.duration_min {
                    self.settings.last_duration_min = self.duration_min;
                    settings_changed = true;
                }
            }
            if settings_changed {
                self.mark_settings_dirty();
                settings_changed = false;
            }
            self.send(command);
        }
        let (open, opening) = self.windows.settings_flags();
        let outcome = settings_window::show(
            ui.ctx(),
            open,
            opening,
            &mut self.settings,
            settings_view(&snapshot, &updates),
        );
        if self.settings_outcome(outcome) {
            settings_changed = true;
        }
        let requests = self.windows.settings_frame_shown();
        self.perform(ui.ctx(), requests);
        if settings_changed {
            self.mark_settings_dirty();
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn on_exit(&mut self, #[cfg(target_os = "windows")] _gl: Option<&eframe::glow::Context>) {
        self.engine.shutdown();
        // Drop all tray callbacks before the last EngineHandle joins its worker.
        self.tray.take();
        if self.save_after.is_some()
            && let Err(error) = self.settings.save()
        {
            log::warn!("Could not save settings: {error:#}");
        }
    }
}

/// The length of a timer started since the last pass, wherever it was started, for
/// the duration picker and the next launch. "+15 Minutes" extends that timer without
/// changing the length the user chose, so this reads the start length, not the total.
fn started_minutes(observed: &mut u64, snapshot: &crate::engine::Snapshot) -> Option<u32> {
    let started = snapshot.started != std::mem::replace(observed, snapshot.started);
    (started && snapshot.state != TimerState::Idle && !snapshot.length.is_zero()).then(|| {
        (snapshot.length.as_secs() / 60).clamp(u64::from(MIN_MINUTES), u64::from(MAX_MINUTES))
            as u32
    })
}

fn settings_view<'a>(
    snapshot: &crate::engine::Snapshot,
    updates: &'a UpdateStatus,
) -> settings_window::SettingsView<'a> {
    settings_window::SettingsView {
        dimming: snapshot.dimming.method,
        volume_supported: snapshot.volume_supported,
        hibernate_supported: snapshot.hibernate_supported,
        updates,
    }
}

/// Requests root repaints from the engine, tray, and later launches, on any thread.
///
/// On Windows, a redraw requested as the tray menu's modal loop starts can be
/// lost. egui still counts that repaint as pending and ignores later requests,
/// so a visible window would stop updating and leave tray actions queued until
/// the pointer moves over it. A native paint request recovers it.
#[derive(Clone)]
pub struct RootWaker {
    ctx: egui::Context,
    last_pass: Arc<AtomicU64>,
    #[cfg(target_os = "windows")]
    hwnd: Option<isize>,
}

impl RootWaker {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        #[cfg(target_os = "windows")]
        let hwnd = {
            use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
            match cc.window_handle().map(|handle| handle.as_raw()) {
                Ok(RawWindowHandle::Win32(handle)) => Some(handle.hwnd.get()),
                _ => None,
            }
        };
        Self {
            ctx: cc.egui_ctx.clone(),
            last_pass: Arc::new(AtomicU64::new(u64::MAX)),
            #[cfg(target_os = "windows")]
            hwnd,
        }
    }

    /// Frequent engine updates. Recover only when no root pass ran since the last one.
    pub fn wake_after(&self, delay: Duration) {
        self.ctx
            .request_repaint_after_for(delay, egui::ViewportId::ROOT);
        let pass = self.ctx.cumulative_pass_nr_for(egui::ViewportId::ROOT);
        if self.last_pass.swap(pass, Ordering::Relaxed) == pass {
            self.paint_natively();
        }
    }

    /// User requests: the tray menu that delivers them is what loses redraws.
    pub fn wake_now(&self) {
        self.ctx.request_repaint_of(egui::ViewportId::ROOT);
        self.last_pass.store(
            self.ctx.cumulative_pass_nr_for(egui::ViewportId::ROOT),
            Ordering::Relaxed,
        );
        self.paint_natively();
    }

    fn paint_natively(&self) {
        #[cfg(target_os = "windows")]
        if let Some(hwnd) = self.hwnd {
            use windows::Win32::{
                Foundation::HWND,
                Graphics::Gdi::{RDW_INTERNALPAINT, RedrawWindow},
                UI::WindowsAndMessaging::{IsIconic, IsWindowVisible},
            };
            let window = HWND(hwnd as _);
            // SAFETY: plain Win32 calls; a stale handle fails harmlessly. Hidden and
            // minimized roots get no WM_PAINT, and eframe already paints them directly.
            unsafe {
                if IsWindowVisible(window).as_bool() && !IsIconic(window).as_bool() {
                    let _ = RedrawWindow(Some(window), None, None, RDW_INTERNALPAINT);
                }
            }
        }
    }
}

/// Keep native decorations on Windows/Linux; macOS paints its unified header.
pub fn window_builder(title: &str) -> egui::ViewportBuilder {
    let builder = egui::ViewportBuilder::default()
        .with_title(title)
        .with_icon(window_icon());
    #[cfg(target_os = "macos")]
    let builder = builder
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false);
    builder
}

fn in_app_bundle() -> bool {
    cfg!(target_os = "macos")
        && std::env::current_exe()
            .is_ok_and(|path| path.to_string_lossy().contains(".app/Contents/MacOS/"))
}

/// Settings rebuilds its viewport every root pass. egui compares icons by pointer,
/// so share one: a fresh icon would be rendered and re-sent to the OS each frame.
fn window_icon() -> Arc<egui::IconData> {
    static ICON: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    Arc::clone(ICON.get_or_init(|| {
        // An empty icon tells eframe to leave the bundle's own icon alone.
        Arc::new(if in_app_bundle() {
            egui::IconData::default()
        } else {
            app_icon()
        })
    }))
}

fn app_icon() -> egui::IconData {
    let mut rgba = crate::tray::moon_rgba(128);
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel[..3].copy_from_slice(&[88, 86, 214]);
    }
    egui::IconData {
        rgba,
        width: 128,
        height: 128,
    }
}

/// macOS ignores winit's window icon; use the same crescent for the Dock.
/// A bundled Nightcap.app keeps the icon from its bundle instead.
pub fn install_app_icon() {
    #[cfg(target_os = "macos")]
    if let Some(mtm) = objc2::MainThreadMarker::new()
        && !in_app_bundle()
    {
        use image::ImageEncoder as _;
        use objc2::AnyThread;
        use objc2_app_kit::{NSApplication, NSImage};
        use objc2_foundation::NSData;
        let icon = app_icon();
        let mut png = Vec::new();
        if image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                &icon.rgba,
                icon.width,
                icon.height,
                image::ExtendedColorType::Rgba8,
            )
            .is_ok()
            && let Some(image) = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(&png))
        {
            // SAFETY: live AppKit objects, on the main thread; NSApplication retains the image.
            unsafe {
                NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tray_timer_sets_the_length_but_extending_it_does_not() {
        use crate::engine::Snapshot;
        let minutes = |value: u64| Duration::from_secs(value * 60);
        let timer = |started, state, length, total| Snapshot {
            started,
            state,
            length: minutes(length),
            total: minutes(total),
            ..Snapshot::default()
        };
        let mut observed = 0;
        assert_eq!(started_minutes(&mut observed, &Snapshot::default()), None);
        // Started from the tray (or the window).
        let started = timer(1, TimerState::Running, 30, 30);
        assert_eq!(started_minutes(&mut observed, &started), Some(30));
        assert_eq!(started_minutes(&mut observed, &started), None, "seen once");
        // +15 Minutes, running or paused.
        let extended = timer(1, TimerState::Paused, 30, 45);
        assert_eq!(started_minutes(&mut observed, &extended), None);
        // Canceled and restarted longer before the window looked again.
        let restarted = timer(2, TimerState::Running, 90, 90);
        assert_eq!(started_minutes(&mut observed, &restarted), Some(90));
        // Started and extended before the window looked: the start length counts.
        let extended_early = timer(3, TimerState::Running, 45, 60);
        assert_eq!(started_minutes(&mut observed, &extended_early), Some(45));
        // Started and canceled unseen, or seen only once it was idle.
        let canceled = timer(4, TimerState::Idle, 0, 0);
        assert_eq!(started_minutes(&mut observed, &canceled), None);
        // Bounded like the picker.
        let long = timer(5, TimerState::Running, 900, 900);
        assert_eq!(started_minutes(&mut observed, &long), Some(720));
    }

    #[test]
    fn rebuilt_window_builders_send_no_repeated_icon_commands() {
        let mut current = window_builder("Settings");
        for _ in 0..3 {
            let (commands, recreate) = current.patch(window_builder("Settings"));
            assert!(commands.is_empty(), "{commands:?}");
            assert!(!recreate);
        }
    }
}
