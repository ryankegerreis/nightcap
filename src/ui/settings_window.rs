//! Separate settings viewport, opened by the gear button or the settings shortcut.

use eframe::egui::{self, Id, Margin, Vec2};

use crate::settings::{BEFORE_MINUTES, FadeSettings, OVER_MINUTES, Settings};
use crate::updates::UpdateStatus;

use super::main_window::{end_action_choices, end_action_label};
use super::widgets::{self, Choice, MeterKind};

/// What Settings shows besides the preferences themselves.
pub struct SettingsView<'a> {
    pub brightness_supported: bool,
    pub overlay_available: bool,
    pub volume_supported: bool,
    pub hibernate_supported: bool,
    pub updates: &'a UpdateStatus,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SettingsOutcome {
    /// A preference changed and should be applied and saved.
    pub changed: bool,
    pub check_now: bool,
    /// The update download the user asked to open.
    pub download: Option<String>,
}

pub fn show(
    ctx: &egui::Context,
    open: &mut bool,
    opening: &mut bool,
    settings: &mut Settings,
    view: SettingsView<'_>,
) -> SettingsOutcome {
    // Register at launch, even while hidden. eframe cannot create an immediate
    // viewport in every event-loop phase used to reopen the root from the tray.
    let requested = *open;
    if requested && !native_window_registered() {
        *opening = true;
    }
    // winit's Visible(true) calls makeKeyAndOrderFront on macOS. Move and
    // configure the precreated window before that command can switch Spaces.
    let prepared = !requested || !*opening || prepare_open();
    // A missing/recreated native window must be created hidden first. Keep the
    // request pending and try preparation again on the next root frame.
    let visible = requested && prepared;
    let mut outcome = SettingsOutcome::default();
    let mut close = false;
    ctx.show_viewport_immediate(
        viewport_id(),
        super::window_builder("Settings")
            .with_visible(visible)
            .with_active(true)
            .with_inner_size([460.0, 700.0])
            .with_min_inner_size([420.0, 480.0]),
        |ui, _class| {
            if !visible {
                return;
            }
            finish_open();
            if ui.input(|input| input.viewport().close_requested())
                || ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::W))
            {
                close = true;
            }
            let tokens = super::theme::tokens(ui.ctx());
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(tokens.window))
                .show(ui, |ui| {
                    widgets::settings_header(ui);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            outcome = settings_body(ui, settings, &view);
                        });
                });
        },
    );
    if prepared {
        *opening = false;
    } else {
        ctx.request_repaint_of(egui::ViewportId::ROOT);
    }
    if close {
        *open = false;
        *opening = false;
    }
    outcome
}

fn settings_body(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    view: &SettingsView<'_>,
) -> SettingsOutcome {
    let SettingsView {
        brightness_supported,
        overlay_available,
        volume_supported,
        hibernate_supported,
        updates,
    } = *view;
    let mut changed = false;
    let mut check_now = false;
    let mut download = None;
    egui::Frame::NONE
        .inner_margin(Margin {
            left: 20,
            right: 20,
            top: 4,
            bottom: 22,
        })
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            ui.set_min_width(ui.available_width());
            section_title(ui, "Screen brightness");
            changed |= fade_group(
                ui,
                FadeCopy {
                    salt: "screen",
                    toggle: "Dim screen before sleep",
                    before: "Start dimming",
                    over: "Dim over",
                    target: "Dim to",
                },
                &mut settings.dim,
                MeterKind::Brightness,
            );
            if !brightness_supported {
                footnote(ui, super::overlay::fallback_message(overlay_available));
            }
            section_title(ui, "Volume");
            changed |= fade_group(
                ui,
                FadeCopy {
                    salt: "volume",
                    toggle: "Lower volume before sleep",
                    before: "Start lowering",
                    over: "Lower over",
                    target: "Lower to",
                },
                &mut settings.volume,
                MeterKind::Volume,
            );
            if !volume_supported {
                footnote(ui, "Volume control isn’t available on this device.");
            }
            section_title(ui, "When the timer ends");
            widgets::InsetGroup::show(ui, "end-action-settings", |group| {
                group.row(true, |ui| {
                    widgets::split(
                        ui,
                        |ui| widgets::body(ui, "Action"),
                        |ui| {
                            let choices = end_action_choices(hibernate_supported);
                            if let Some(action) = widgets::popup_select(
                                ui,
                                Id::new("settings-end-action"),
                                end_action_label(settings.end_action),
                                settings.end_action,
                                &choices,
                            ) {
                                settings.end_action = action;
                                changed = true;
                            }
                        },
                    );
                });
                group.row(true, |ui| {
                    widgets::split_stacked(
                        ui,
                        |ui| {
                            widgets::body(
                                ui,
                                "Restore brightness and volume after canceling or waking",
                            )
                        },
                        |ui| {
                            if widgets::toggle(
                                ui,
                                Id::new("settings-restore"),
                                settings.restore_on_cancel_or_wake,
                                "Restore brightness and volume after canceling or waking",
                            )
                            .clicked()
                            {
                                settings.restore_on_cancel_or_wake =
                                    !settings.restore_on_cancel_or_wake;
                                changed = true;
                            }
                        },
                    );
                });
            });
            section_title(ui, "Menu bar");
            widgets::InsetGroup::show(ui, "menu-bar-settings", |group| {
                group.row(true, |ui| {
                    widgets::split(
                        ui,
                        |ui| widgets::body(ui, "Show time remaining in the menu bar"),
                        |ui| {
                            if widgets::toggle(
                                ui,
                                Id::new("settings-menu-title"),
                                settings.show_time_in_menu_bar,
                                "Show time remaining in the menu bar",
                            )
                            .clicked()
                            {
                                settings.show_time_in_menu_bar = !settings.show_time_in_menu_bar;
                                changed = true;
                            }
                        },
                    );
                });
            });
            section_title(ui, "Updates");
            widgets::InsetGroup::show(ui, "update-settings", |group| {
                group.row(true, |ui| {
                    widgets::split(
                        ui,
                        |ui| widgets::body(ui, "Check for updates automatically"),
                        |ui| {
                            if widgets::toggle(
                                ui,
                                Id::new("settings-check-updates"),
                                settings.check_for_updates,
                                "Check for updates automatically",
                            )
                            .clicked()
                            {
                                settings.check_for_updates = !settings.check_for_updates;
                                changed = true;
                            }
                        },
                    );
                });
                group.row(true, |ui| {
                    widgets::split_stacked(
                        ui,
                        |ui| {
                            widgets::title_and_caption(
                                ui,
                                &format!("Nightcap {}", updates.current),
                                &updates.caption(),
                            )
                        },
                        |ui| match &updates.available {
                            Some(release) => {
                                if widgets::text_button(
                                    ui,
                                    Id::new("settings-download-update"),
                                    "Download",
                                    true,
                                )
                                .clicked()
                                {
                                    download = Some(release.download_url());
                                }
                            }
                            None => {
                                if widgets::text_button(
                                    ui,
                                    Id::new("settings-check-now"),
                                    "Check Now",
                                    !updates.checking,
                                )
                                .clicked()
                                {
                                    check_now = true;
                                }
                            }
                        },
                    );
                });
            });
        });
    SettingsOutcome {
        changed,
        check_now,
        download,
    }
}

struct FadeCopy<'a> {
    salt: &'a str,
    toggle: &'a str,
    before: &'a str,
    over: &'a str,
    target: &'a str,
}

fn fade_group(
    ui: &mut egui::Ui,
    copy: FadeCopy<'_>,
    fade: &mut FadeSettings,
    meter: MeterKind,
) -> bool {
    let FadeCopy {
        salt,
        toggle: toggle_label,
        before: before_label,
        over: over_label,
        target: target_label,
    } = copy;
    let mut changed = false;
    widgets::InsetGroup::show(ui, salt, |group| {
        group.row(true, |ui| {
            widgets::split(
                ui,
                |ui| widgets::body(ui, toggle_label),
                |ui| {
                    if widgets::toggle(
                        ui,
                        Id::new(("settings-fade", salt)),
                        fade.enabled,
                        toggle_label,
                    )
                    .clicked()
                    {
                        fade.enabled = !fade.enabled;
                        changed = true;
                    }
                },
            );
        });
        group.row(fade.enabled, |ui| {
            widgets::split(
                ui,
                |ui| widgets::body(ui, before_label),
                |ui| {
                    let options =
                        minute_options(&BEFORE_MINUTES, fade.start_before_min, |_| true, true);
                    let choices = choice_refs(&options);
                    if let Some(minutes) = widgets::popup_select(
                        ui,
                        Id::new(("settings-before", salt)),
                        &before_phrase(fade.start_before_min),
                        fade.start_before_min,
                        &choices,
                    ) {
                        fade.start_before_min = minutes;
                        if fade.over_min > fade.start_before_min {
                            fade.over_min = OVER_MINUTES
                                .into_iter()
                                .filter(|minutes| *minutes <= fade.start_before_min)
                                .max()
                                .unwrap_or(1);
                        }
                        changed = true;
                    }
                },
            );
        });
        group.row(fade.enabled, |ui| {
            widgets::split(
                ui,
                |ui| widgets::body(ui, over_label),
                |ui| {
                    let limit = fade.start_before_min;
                    let options = minute_options(
                        &OVER_MINUTES,
                        fade.over_min,
                        |minutes| minutes <= limit,
                        false,
                    );
                    let choices = choice_refs(&options);
                    if let Some(minutes) = widgets::popup_select(
                        ui,
                        Id::new(("settings-over", salt)),
                        &over_phrase(fade.over_min),
                        fade.over_min,
                        &choices,
                    ) {
                        fade.over_min = minutes;
                        changed = true;
                    }
                },
            );
        });
        group.row(fade.enabled, |ui| {
            widgets::split(
                ui,
                |ui| widgets::body(ui, target_label),
                |ui| {
                    if widgets::meter_slider(
                        ui,
                        Id::new(("settings-target", salt)),
                        &mut fade.target_percent,
                        meter,
                    ) {
                        fade.target_percent = fade.target_percent.min(90);
                        changed = true;
                    }
                },
            );
        });
    });
    changed
}

struct MinuteOption {
    label: String,
    minutes: u32,
    enabled: bool,
}

fn minute_options(
    presets: &[u32],
    current: u32,
    allow: impl Fn(u32) -> bool,
    before: bool,
) -> Vec<MinuteOption> {
    let mut values: Vec<u32> = presets.to_vec();
    if !values.contains(&current) {
        values.push(current);
        values.sort_unstable();
    }
    values
        .into_iter()
        .map(|minutes| MinuteOption {
            label: if before {
                before_phrase(minutes)
            } else {
                over_phrase(minutes)
            },
            minutes,
            enabled: allow(minutes),
        })
        .collect()
}

fn choice_refs(options: &[MinuteOption]) -> Vec<Choice<'_, u32>> {
    options
        .iter()
        .map(|option| Choice {
            label: &option.label,
            value: option.minutes,
            enabled: option.enabled,
        })
        .collect()
}

fn before_phrase(minutes: u32) -> String {
    format!("{minutes} minutes before")
}

fn over_phrase(minutes: u32) -> String {
    if minutes == 1 {
        "1 minute".to_owned()
    } else {
        format!("{minutes} minutes")
    }
}

fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.add_space(16.0);
    let color = super::theme::tokens(ui.ctx()).label;
    ui.label(egui::RichText::new(text).size(13.0).color(color));
    ui.add_space(7.0);
}

fn footnote(ui: &mut egui::Ui, text: &str) {
    ui.add_space(6.0);
    widgets::caption(ui, text);
}

pub fn viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("nightcap-settings")
}

/// Prepare the existing native settings window before it is shown or focused.
fn prepare_open() -> bool {
    #[cfg(target_os = "macos")]
    {
        use objc2::MainThreadMarker;
        use objc2_app_kit::{
            NSApplication, NSScreen, NSWindowCollectionBehavior, NSWindowOrderingMode,
        };

        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };
        // eframe exposes only the root's native handle, so find our two unique
        // window titles in this process. The dimming overlay is never selected.
        let windows = NSApplication::sharedApplication(mtm).windows();
        let Some(settings) = windows
            .iter()
            .find(|window| window.title().to_string() == "Settings")
        else {
            return false;
        };
        let Some(main) = windows
            .iter()
            .find(|window| window.title().to_string() == "Nightcap")
        else {
            return false;
        };
        let mut behavior = settings.collectionBehavior();
        // Target Nightcap's Space, not whichever Space happens to be active
        // when a tray request is received. A temporary parent link places it.
        behavior.remove(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::MoveToActiveSpace,
        );
        settings.setCollectionBehavior(behavior);
        // SAFETY: distinct, live AppKit windows on the main thread. Settings is
        // attached only to the root, so no parent/child cycle can be introduced.
        // finish_open detaches it as soon as it is ordered in, before dragging.
        if settings.parentWindow().as_deref() != Some(&*main) {
            unsafe { main.addChildWindow_ordered(&settings, NSWindowOrderingMode::Above) };
        }
        // Screen lookup only affects placement, never the Space relationship.
        // Ordered-out/offscreen windows can lack a screen; use their saved frame.
        let screen = main.screen().or_else(|| {
            NSScreen::screens(mtm).iter().min_by(|a, b| {
                screen_distance_squared(main.frame(), a.frame())
                    .total_cmp(&screen_distance_squared(main.frame(), b.frame()))
            })
        });
        if let Some(screen) = screen {
            settings.setFrameOrigin(opening_origin(
                main.frame(),
                settings.frame().size,
                screen.visibleFrame(),
            ));
        }
        true
    }
    #[cfg(not(target_os = "macos"))]
    true
}

fn native_window_registered() -> bool {
    #[cfg(target_os = "macos")]
    {
        native_settings_window().is_some()
    }
    #[cfg(not(target_os = "macos"))]
    true
}

#[cfg(target_os = "macos")]
fn native_settings_window() -> Option<objc2::rc::Retained<objc2_app_kit::NSWindow>> {
    let mtm = objc2::MainThreadMarker::new()?;
    objc2_app_kit::NSApplication::sharedApplication(mtm)
        .windows()
        .iter()
        .find(|window| window.title().to_string() == "Settings")
}

fn finish_open() {
    #[cfg(target_os = "macos")]
    if let Some(settings) = native_settings_window()
        && settings.isVisible()
        && let Some(parent) = settings.parentWindow()
    {
        // Existing viewport visibility changes are applied after its callback.
        // Keep the link until the next callback observes it actually ordered in.
        parent.removeChildWindow(&settings);
    }
}

#[cfg(target_os = "macos")]
fn screen_distance_squared(
    root: objc2_foundation::NSRect,
    screen: objc2_foundation::NSRect,
) -> f64 {
    let x = root.origin.x + root.size.width / 2.0;
    let y = root.origin.y + root.size.height / 2.0;
    let dx = x - x.clamp(screen.origin.x, screen.origin.x + screen.size.width);
    let dy = y - y.clamp(screen.origin.y, screen.origin.y + screen.size.height);
    dx * dx + dy * dy
}

#[cfg(target_os = "macos")]
fn opening_origin(
    root: objc2_foundation::NSRect,
    size: objc2_foundation::NSSize,
    visible: objc2_foundation::NSRect,
) -> objc2_foundation::NSPoint {
    let x = (root.origin.x + (root.size.width - size.width) / 2.0).clamp(
        visible.origin.x,
        visible.origin.x + (visible.size.width - size.width).max(0.0),
    );
    // Keep the title bar reachable even if Settings is taller than the screen.
    let top_aligned_y = visible.origin.y + visible.size.height - size.height;
    let y = (root.origin.y + (root.size.height - size.height) / 2.0)
        .clamp(visible.origin.y.min(top_aligned_y), top_aligned_y);
    objc2_foundation::NSPoint::new(x, y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::EndAction;

    #[cfg(target_os = "macos")]
    #[test]
    fn settings_placement_uses_the_main_display_coordinates_and_keeps_the_title_bar_reachable() {
        use objc2_foundation::{NSPoint, NSRect, NSSize};
        for (root_origin, visible, settings_size, expected) in [
            (
                NSPoint::new(-1800.0, 150.0),
                NSRect::new(NSPoint::new(-1920.0, 0.0), NSSize::new(1920.0, 1050.0)),
                NSSize::new(460.0, 700.0),
                NSPoint::new(-1860.0, 80.0),
            ),
            (
                NSPoint::new(100.0, 1300.0),
                NSRect::new(NSPoint::new(0.0, 1080.0), NSSize::new(1920.0, 1000.0)),
                NSSize::new(460.0, 700.0),
                NSPoint::new(40.0, 1230.0),
            ),
            (
                NSPoint::new(20.0, 20.0),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(800.0, 500.0)),
                NSSize::new(900.0, 700.0),
                NSPoint::new(0.0, -200.0),
            ),
        ] {
            assert_eq!(
                opening_origin(
                    NSRect::new(root_origin, NSSize::new(340.0, 560.0)),
                    settings_size,
                    visible
                ),
                expected
            );
        }
    }

    /// Renders the settings body headlessly, returning its outcome and each text's rect.
    fn render(
        ctx: &egui::Context,
        updates: &UpdateStatus,
        events: Vec<egui::Event>,
        dimming_available: bool,
    ) -> (SettingsOutcome, Vec<(String, egui::Rect)>) {
        fn texts(shape: &egui::Shape, found: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Text(text) => found.push((
                    text.galley.text().to_owned(),
                    text.galley.rect.translate(text.pos.to_vec2()),
                )),
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| texts(shape, found)),
                _ => {}
            }
        }
        let mut settings = Settings::default();
        let mut outcome = SettingsOutcome::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(460.0, 1200.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let view = SettingsView {
                brightness_supported: dimming_available,
                overlay_available: dimming_available,
                volume_supported: true,
                hibernate_supported: false,
                updates,
            };
            outcome = settings_body(ui, &mut settings, &view);
        });
        // Nothing uploads textures here; drop the delta as a renderer would.
        output.textures_delta.clear();
        let mut found = Vec::new();
        for clipped in &output.shapes {
            texts(&clipped.shape, &mut found);
        }
        (outcome, found)
    }

    fn click(ctx: &egui::Context, updates: &UpdateStatus, label: &str) -> SettingsOutcome {
        let (_, texts) = render(ctx, updates, Vec::new(), true);
        let (_, rect) = texts
            .iter()
            .find(|(text, _)| text == label)
            .unwrap_or_else(|| panic!("{label} not shown: {texts:?}"));
        let pos = rect.center();
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        render(ctx, updates, vec![egui::Event::PointerMoved(pos)], true);
        render(ctx, updates, vec![button(true)], true);
        render(ctx, updates, vec![button(false)], true).0
    }

    #[test]
    fn settings_reports_when_screen_dimming_is_unavailable() {
        let updates = UpdateStatus::new(semver::Version::new(0, 1, 2));
        let (_, texts) = render(&egui::Context::default(), &updates, Vec::new(), false);
        assert!(
            texts
                .iter()
                .any(|(text, _)| text == "Screen dimming is unavailable on this desktop.")
        );
    }

    #[test]
    fn updates_section_shows_the_version_and_offers_the_right_button() {
        use crate::updates::Release;
        let ctx = egui::Context::default();
        let mut updates = UpdateStatus::new(semver::Version::new(0, 1, 2));
        let shown = |updates: &UpdateStatus| {
            render(&egui::Context::default(), updates, Vec::new(), true)
                .1
                .into_iter()
                .map(|(text, _)| text)
                .collect::<Vec<_>>()
        };
        let texts = shown(&updates);
        for expected in [
            "Updates",
            "Check for updates automatically",
            "Nightcap 0.1.2",
            "Not checked yet",
            "Check Now",
        ] {
            assert!(
                texts.iter().any(|text| text == expected),
                "{expected}: {texts:?}"
            );
        }
        let outcome = click(&ctx, &updates, "Check Now");
        assert!(outcome.check_now && !outcome.changed);
        assert_eq!(outcome.download, None);

        // Disabled while a check runs.
        updates.checking = true;
        assert!(shown(&updates).iter().any(|text| text == "Checking…"));
        assert!(!click(&ctx, &updates, "Check Now").check_now);

        updates.checking = false;
        let release = Release {
            version: semver::Version::new(0, 2, 0),
            page_url: format!("{}/releases/tag/v0.2.0", crate::updates::REPOSITORY),
        };
        updates.available = Some(release.clone());
        let texts = shown(&updates);
        assert!(
            texts
                .iter()
                .any(|text| text == "Version 0.2.0 is available")
        );
        assert!(!texts.iter().any(|text| text == "Check Now"));
        let outcome = click(&ctx, &updates, "Download");
        assert_eq!(outcome.download, Some(release.download_url()));
        assert!(!outcome.check_now);
    }

    #[test]
    fn end_action_menu_only_offers_hibernate_when_available() {
        for (supported, expected) in [
            (false, vec![EndAction::Sleep, EndAction::Shutdown]),
            (
                true,
                vec![EndAction::Sleep, EndAction::Hibernate, EndAction::Shutdown],
            ),
        ] {
            let choices = end_action_choices(supported);
            assert_eq!(
                choices
                    .iter()
                    .map(|choice| choice.value)
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(choices.iter().all(|choice| choice.enabled));
        }
    }
}
