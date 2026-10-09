//! App routing between the main timer, settings viewport, and dimming overlay.

use std::time::{Duration, Instant};

use eframe::egui;

use crate::engine::{Command, EngineHandle, TimerState};
use crate::settings::Settings;

use self::main_window::{MAX_MINUTES, MIN_MINUTES, PRESETS};

pub mod main_window;
pub mod overlay;
pub mod settings_window;
pub mod theme;
pub mod widgets;

const SAVE_DELAY: Duration = Duration::from_millis(300);
const RUNNING_REPAINT: Duration = Duration::from_millis(250);
const NOTICE_DURATION: Duration = Duration::from_millis(4500);

pub struct NightcapApp {
    engine: EngineHandle,
    settings: Settings,
    duration_min: u32,
    custom_duration: bool,
    settings_open: bool,
    save_after: Option<Instant>,
    notice: Option<StatusNote>,
    ctx: Option<egui::Context>,
}

struct StatusNote {
    text: String,
    until: Instant,
}

impl NightcapApp {
    pub fn new(engine: EngineHandle) -> Self {
        let settings = Settings::load();
        let duration_min = settings.last_duration_min.clamp(MIN_MINUTES, MAX_MINUTES);
        let custom_duration = !PRESETS.contains(&duration_min);
        Self {
            engine,
            settings,
            duration_min,
            custom_duration,
            settings_open: false,
            save_after: None,
            notice: None,
            ctx: None,
        }
    }

    pub fn open_settings(&mut self) {
        self.settings_open = true;
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint();
        }
    }

    pub fn engine(&self) -> &EngineHandle {
        &self.engine
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
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Tray / overlay integration (another module owns `tray` and `ui::overlay`):
        // - Poll `self.engine().snapshot()` for the menu-bar title and `overlay_alpha`.
        // - Call `self.open_settings()` from the tray Settings item.
        // - Render `crate::ui::overlay` while `snapshot.overlay_alpha` is above zero.
        // - On the main window's close button, hide it with `ViewportCommand::Visible(false)`
        //   and `ViewportCommand::CancelClose` instead of quitting, so the tray keeps
        //   the process alive.

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
            self.settings_open = true;
        }

        let snapshot = self.engine.snapshot();
        if snapshot.state == TimerState::Running {
            ui.ctx().request_repaint_after(RUNNING_REPAINT);
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
            self.settings_open = true;
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
        if settings_changed {
            self.mark_settings_dirty();
        }
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn on_exit(&mut self) {
        if self.save_after.is_some()
            && let Err(error) = self.settings.save()
        {
            log::warn!("Could not save settings: {error:#}");
        }
    }
}
