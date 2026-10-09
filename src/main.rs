//! Nightcap: a sleep timer that dims the screen and lowers the volume
//! before putting the computer to sleep.

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Nightcap")
            .with_inner_size([340.0, 560.0])
            .with_min_inner_size([340.0, 480.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Nightcap",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_theme(egui::ThemePreference::System);
            Ok(Box::new(Nightcap::default()))
        }),
    )
}

#[derive(Default)]
struct Nightcap {}

impl eframe::App for Nightcap {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.heading("Nightcap");
                ui.label("Sleep timer coming soon.");
            });
        });
    }
}
