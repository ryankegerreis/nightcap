//! Nightcap: a sleep timer that dims the screen and lowers the volume
//! before putting the computer to sleep.

use std::sync::Arc;

use eframe::egui;
use nightcap::{
    engine::{Engine, SpeedClock, SystemClock},
    platform,
    settings::Settings,
    tray::Tray,
    ui::{NightcapApp, theme, window_builder},
};

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let settings = Settings::load();
    let mut backends = platform::backends();
    if std::env::var("NIGHTCAP_DRY_RUN").as_deref() == Ok("1") {
        log::warn!("Dry run enabled: brightness/volume fades are real, power actions are disabled");
        backends.power = Box::new(platform::DryRunPower);
    }
    let speed = std::env::var("NIGHTCAP_SPEED")
        .ok()
        .and_then(|value| match value.parse::<f64>() {
            Ok(speed) if speed.is_finite() && speed > 0.0 => Some(speed),
            _ => {
                log::warn!("Invalid NIGHTCAP_SPEED={value:?}; using real time");
                None
            }
        })
        .unwrap_or(1.0);
    let clock = SpeedClock::new(SystemClock::default(), speed).expect("validated timer speed");
    let options = eframe::NativeOptions {
        viewport: window_builder("Nightcap")
            .with_inner_size([340.0, 560.0])
            .with_min_inner_size([340.0, 480.0])
            .with_transparent(true),
        #[cfg(target_os = "macos")]
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
            // Keep winit from replacing the graceful Quit/Settings app menu.
            builder
                .with_default_menu(false)
                .with_activation_policy(ActivationPolicy::Regular);
        })),
        ..Default::default()
    };

    eframe::run_native(
        "Nightcap",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::ThemePreference::System);
            theme::install_fonts(&cc.egui_ctx);
            nightcap::ui::install_app_icon();
            let context = cc.egui_ctx.clone();
            let engine = Engine::spawn_with_clock(
                settings.clone(),
                backends,
                clock,
                Box::new(move || context.request_repaint_of(egui::ViewportId::ROOT)),
            )?;
            let context = cc.egui_ctx.clone();
            let tray = Tray::new(
                engine.clone(),
                Arc::new(move || context.request_repaint_of(egui::ViewportId::ROOT)),
            )
            .map_err(|error| {
                log::warn!("Tray unavailable; closing the window will quit: {error:#}")
            })
            .ok();
            Ok(Box::new(NightcapApp::new(engine, settings, tray)))
        }),
    )
}
