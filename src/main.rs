//! Nightcap: a sleep timer that dims the screen and lowers the volume
//! before putting the computer to sleep.

use eframe::egui;
use nightcap::{engine::Engine, platform, settings::Settings, ui::NightcapApp};

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let settings = Settings::load();
    let backends = platform::backends();
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
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::ThemePreference::System);
            let context = cc.egui_ctx.clone();
            let engine = Engine::spawn(
                settings,
                backends,
                Box::new(move || context.request_repaint()),
            )?;
            Ok(Box::new(NightcapApp::new(engine)))
        }),
    )
}
