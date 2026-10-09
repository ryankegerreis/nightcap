//! App routing between the main timer, settings viewport, and dimming overlay.

use std::time::{Duration, Instant};

use eframe::egui;

use crate::engine::{Command, EngineHandle, TimerState};
use crate::settings::Settings;
use crate::tray::{Tray, TrayAction};

use self::main_window::{MAX_MINUTES, MIN_MINUTES, PRESETS};

pub mod main_window;
pub mod overlay;
pub mod settings_window;
pub mod theme;
pub mod widgets;

const SAVE_DELAY: Duration = Duration::from_millis(300);
const NOTICE_DURATION: Duration = Duration::from_millis(4500);

pub struct NightcapApp {
    engine: EngineHandle,
    tray: Option<Tray>,
    overlay: overlay::Overlay,
    root_visible: bool,
    quitting: bool,
    close_request_handled: bool,
    observed_total: Duration,
    settings: Settings,
    duration_min: u32,
    custom_duration: bool,
    settings_open: bool,
    settings_focus_pending: u8,
    save_after: Option<Instant>,
    notice: Option<StatusNote>,
    ctx: Option<egui::Context>,
    #[cfg(target_os = "macos")]
    _wake_observer: crate::platform::macos::WakeObserver,
}

struct StatusNote {
    text: String,
    until: Instant,
}

impl NightcapApp {
    pub fn new(engine: EngineHandle, settings: Settings, tray: Option<Tray>) -> Self {
        set_activation_policy(true);
        let duration_min = settings.last_duration_min.clamp(MIN_MINUTES, MAX_MINUTES);
        let custom_duration = !PRESETS.contains(&duration_min);
        Self {
            #[cfg(target_os = "macos")]
            _wake_observer: crate::platform::macos::WakeObserver::new(engine.clone()),
            engine,
            tray,
            overlay: overlay::Overlay::new(),
            root_visible: true,
            quitting: false,
            close_request_handled: false,
            observed_total: Duration::ZERO,
            settings,
            duration_min,
            custom_duration,
            settings_open: false,
            settings_focus_pending: 0,
            save_after: None,
            notice: None,
            ctx: None,
        }
    }

    pub fn open_settings(&mut self) {
        self.settings_open = true;
        self.settings_focus_pending = 2;
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint_of(egui::ViewportId::ROOT);
        }
    }

    pub fn engine(&self) -> &EngineHandle {
        &self.engine
    }

    fn open_window(&mut self, ctx: &egui::Context, focus: bool) {
        self.root_visible = true;
        set_activation_policy(true);
        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd_to(
            egui::ViewportId::ROOT,
            egui::ViewportCommand::Minimized(false),
        );
        if focus {
            ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
        }
        ctx.request_repaint_of(egui::ViewportId::ROOT);
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        self.root_visible = false;
        if self.settings_open {
            ctx.send_viewport_cmd_to(
                settings_window::viewport_id(),
                egui::ViewportCommand::Visible(false),
            );
            self.settings_open = false;
        }
        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::CancelClose);
        ctx.send_viewport_cmd_to(
            egui::ViewportId::ROOT,
            egui::ViewportCommand::Visible(false),
        );
        set_activation_policy(false);
    }

    fn quit(&mut self, ctx: &egui::Context) {
        if !self.quitting {
            self.quitting = true;
            self.send(Command::Shutdown);
        }
        ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
    }

    fn send(&self, command: Command) {
        if let Err(error) = self.engine.send(command) {
            log::warn!("Timer command failed: {error}");
        }
    }

    fn mark_settings_dirty(&mut self) {
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
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        theme::install(ctx);
        self.ctx = Some(ctx.clone());
        self.flush_save();
        let snapshot = self.engine.snapshot();
        if snapshot.state != TimerState::Idle && snapshot.total != self.observed_total {
            self.duration_min = (snapshot.total.as_secs() / 60)
                .clamp(u64::from(MIN_MINUTES), u64::from(MAX_MINUTES))
                as u32;
            self.custom_duration = !PRESETS.contains(&self.duration_min);
            if self.settings.last_duration_min != self.duration_min {
                self.settings.last_duration_min = self.duration_min;
                self.mark_settings_dirty();
            }
        }
        self.observed_total = snapshot.total;
        let actions = if let Some(tray) = &mut self.tray {
            tray.update(&snapshot, self.settings.show_time_in_menu_bar);
            tray.poll()
        } else {
            Vec::new()
        };
        for action in actions {
            match action {
                TrayAction::OpenWindow => self.open_window(ctx, true),
                TrayAction::OpenSettings => {
                    if !self.root_visible {
                        self.open_window(ctx, false);
                    }
                    self.open_settings();
                }
                TrayAction::Quit => self.quit(ctx),
            }
        }
        // Hidden input retains the last UI's key events: only consume shortcuts
        // while visible. Native app-menu shortcuts use the tray queue above.
        if self.root_visible
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::Q))
        {
            self.quit(ctx);
        }
        // Logic-only passes retain pending input until the root renders again.
        // Handle each native close once so its stale event cannot hide a tray reopen.
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        let new_close = close_edge(close_requested, &mut self.close_request_handled);
        if new_close
            || (self.root_visible
                && ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::W)))
        {
            if self.tray.is_some() && !self.quitting {
                self.hide_window(ctx);
            } else {
                self.quit(ctx);
            }
        }
        self.overlay.sync(ctx, snapshot.overlay_alpha);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        theme::install(ui.ctx());
        self.ctx = Some(ui.ctx().clone());
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
        let tokens = theme::tokens(ui.ctx());
        ui.painter().rect_filled(ui.max_rect(), 0.0, tokens.window);
        self.overlay.show(ui.ctx(), snapshot.overlay_alpha);
        if !self.root_visible {
            settings_window::show(
                ui.ctx(),
                &mut self.settings_open,
                &mut self.settings,
                snapshot.brightness_supported,
                snapshot.volume_supported,
            );
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
        if settings_window::show(
            ui.ctx(),
            &mut self.settings_open,
            &mut self.settings,
            snapshot.brightness_supported,
            snapshot.volume_supported,
        ) {
            settings_changed = true;
        }
        if self.settings_focus_pending > 0 && self.settings_open {
            // The immediate child's creation frame can still carry root window
            // commands. Focus on the following frame, once those have drained.
            self.settings_focus_pending -= 1;
            if self.settings_focus_pending == 0 {
                ui.ctx().send_viewport_cmd_to(
                    settings_window::viewport_id(),
                    egui::ViewportCommand::Focus,
                );
            }
            ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
        }
        if settings_changed {
            self.mark_settings_dirty();
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn on_exit(&mut self) {
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

fn close_edge(requested: bool, handled: &mut bool) -> bool {
    let edge = requested && !*handled;
    *handled = requested;
    edge
}

/// Keep native decorations on Windows/Linux; macOS paints its unified header.
pub fn window_builder(title: &str) -> egui::ViewportBuilder {
    let icon = app_icon();
    let builder = egui::ViewportBuilder::default()
        .with_title(title)
        .with_icon(icon);
    #[cfg(target_os = "macos")]
    let builder = builder
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false);
    builder
}

fn set_activation_policy(visible: bool) {
    #[cfg(target_os = "macos")]
    if let Some(mtm) = objc2::MainThreadMarker::new() {
        use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
        let app = NSApplication::sharedApplication(mtm);
        let policy = if visible {
            NSApplicationActivationPolicy::Regular
        } else {
            NSApplicationActivationPolicy::Accessory
        };
        if app.activationPolicy() != policy {
            app.setActivationPolicy(policy);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = visible;
}

fn app_icon() -> egui::IconData {
    let mut rgba = crate::tray::moon_rgba(128);
    for pixel in rgba.chunks_exact_mut(4) {
        pixel[..3].copy_from_slice(&[88, 86, 214]);
    }
    egui::IconData {
        rgba,
        width: 128,
        height: 128,
    }
}

/// macOS ignores winit's window icon; use the same crescent for the Dock.
pub fn install_app_icon() {
    #[cfg(target_os = "macos")]
    if let Some(mtm) = objc2::MainThreadMarker::new() {
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
    fn retained_close_event_is_handled_once_and_a_new_close_is_handled() {
        let mut handled = false;
        assert!(close_edge(true, &mut handled));
        for _ in 0..100 {
            assert!(!close_edge(true, &mut handled));
        }
        assert!(!close_edge(false, &mut handled));
        assert!(close_edge(true, &mut handled));
    }
}
