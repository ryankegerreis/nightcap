//! Nightcap: a sleep timer that dims the screen and lowers the volume
//! before putting the computer to sleep.

// Release builds on Windows run without a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use eframe::egui;
use nightcap::{
    engine::{Engine, SpeedClock, SystemClock},
    instance::{self, Launch},
    platform,
    settings::Settings,
    tray::Tray,
    ui::{NightcapApp, RootWaker, theme, window_builder},
};

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    // Before touching devices: a second engine would capture faded levels as originals.
    let mut instance = match instance::acquire() {
        Launch::Primary(instance) => instance,
        Launch::Secondary { shown: true } => {
            log::info!("Nightcap is already running; showed its window");
            return Ok(());
        }
        Launch::Secondary { shown: false } => {
            log::error!("Nightcap is already running but could not be shown");
            std::process::exit(1);
        }
    };
    let instance = &mut instance;
    let mut settings = Settings::load();
    let mut backends = platform::backends();
    if settings.apply_hibernate_availability(backends.hibernate_supported) {
        log::warn!("Hibernate is unavailable on this machine; using Sleep instead");
        if let Err(error) = settings.save() {
            log::warn!("Could not save the fallback end action: {error:#}");
        }
    }
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
    let mut options = eframe::NativeOptions {
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
    prefer_light_graphics(&mut options);

    eframe::run_native(
        "Nightcap",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::ThemePreference::System);
            theme::install_fonts(&cc.egui_ctx);
            nightcap::ui::install_app_icon();
            let waker = RootWaker::new(cc);
            let engine_waker = waker.clone();
            let engine = Engine::spawn_with_clock(
                settings.clone(),
                backends,
                clock,
                // A nonzero delay avoids egui's extra immediate-repaint pass.
                // Hidden roots run App::logic only, refreshing tray/overlay at engine cadence.
                Box::new(move || engine_waker.wake_after(Duration::from_millis(16))),
            )?;
            let tray_waker = waker.clone();
            let tray = Tray::new(engine.clone(), Arc::new(move || tray_waker.wake_now()))
                .map_err(|error| {
                    log::warn!("Tray unavailable; closing the window will quit: {error:#}")
                })
                .ok();
            let (activate, activations) = mpsc::channel();
            // The UI accepts once it has asked the window to show, or refuses while
            // quitting; a refused launch starts its own copy after this one exits.
            // Requests the UI reaches too late expire instead of opening the window.
            instance.on_activate(move |allowed| {
                let (reply, shown) = mpsc::channel();
                if activate.send((reply, Instant::now() + allowed)).is_err() {
                    return false;
                }
                waker.wake_now();
                shown.recv_timeout(allowed).unwrap_or(false)
            });
            Ok(Box::new(
                NightcapApp::new(engine, settings, tray).with_activations(activations),
            ))
        }),
    )
}

/// This window is a flat 2D panel that is usually hidden. Keep a single frame
/// in flight and ask the allocator to stay small.
fn prefer_light_graphics(options: &mut eframe::NativeOptions) {
    #[cfg(target_os = "windows")]
    {
        options.renderer = eframe::Renderer::Glow;
    }
    options.wgpu_options.surface = eframe::egui_wgpu::SurfaceConfig::LOW_LATENCY;
    let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup
    else {
        return;
    };
    let previous = Arc::clone(&setup.device_descriptor);
    setup.device_descriptor = Arc::new(move |adapter| {
        let mut descriptor = previous(adapter);
        descriptor.memory_hints = eframe::wgpu::MemoryHints::MemoryUsage;
        descriptor
    });
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    #[test]
    fn windows_renderer_supports_transparent_hidden_viewports() {
        let mut options = eframe::NativeOptions::default();
        super::prefer_light_graphics(&mut options);
        assert!(matches!(options.renderer, eframe::Renderer::Glow));
    }
}
