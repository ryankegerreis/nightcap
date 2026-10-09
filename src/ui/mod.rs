//! App routing between the main timer, settings viewport, and dimming overlay.

use std::time::Duration;

use eframe::egui;

use crate::engine::{Command, EngineHandle};

pub mod main_window;
pub mod overlay;
pub mod settings_window;
pub mod theme;
pub mod widgets;

pub struct NightcapApp {
    engine: EngineHandle,
}

impl NightcapApp {
    pub fn new(engine: EngineHandle) -> Self {
        Self { engine }
    }

    fn send(&self, command: Command) {
        if let Err(error) = self.engine.send(command) {
            log::warn!("Timer command failed: {error}");
        }
    }
}

impl eframe::App for NightcapApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let snapshot = self.engine.snapshot();
        let seconds =
            snapshot.remaining.as_secs() + u64::from(snapshot.remaining.subsec_nanos() > 0);
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading("Nightcap");
                ui.label(format!("{}:{:02}", seconds / 60, seconds % 60));
                if ui.button("Start 45 min").clicked() {
                    self.send(Command::Start {
                        duration: Duration::from_secs(45 * 60),
                    });
                }
                if ui.button("Cancel").clicked() {
                    self.send(Command::Cancel);
                }
            });
        });
    }
}
