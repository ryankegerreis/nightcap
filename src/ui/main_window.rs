//! Main timer window: countdown, duration controls, fade summaries, and timer actions.

use std::time::Duration;

use chrono::Timelike;

use eframe::egui::{self, Align2, FontId, Id, Margin, Pos2, Rect, Sense, Vec2, pos2, vec2};

use crate::engine::{Command, Snapshot, TimerState};
use crate::settings::{EndAction, FadeSettings, Settings};

use super::theme;
use super::widgets::{self, Choice, RoundRole};

pub const PRESETS: [u32; 5] = [15, 30, 45, 60, 90];
pub const MIN_MINUTES: u32 = 1;
pub const MAX_MINUTES: u32 = 720;

const RING_SIZE: f32 = 204.0;
const RING_WRAP: f32 = 236.0;
const BUTTON: f32 = 62.0;

pub struct MainView<'a> {
    pub snapshot: &'a Snapshot,
    pub settings: &'a mut Settings,
    pub duration_min: &'a mut u32,
    pub custom: &'a mut bool,
    pub notice: Option<&'a str>,
}

pub struct MainOutcome {
    pub open_settings: bool,
    pub command: Option<Command>,
    pub settings_changed: bool,
    pub notice: Option<&'static str>,
}

pub fn show(ui: &mut egui::Ui, view: MainView<'_>) -> MainOutcome {
    let MainView {
        snapshot,
        settings,
        duration_min,
        custom,
        notice,
    } = view;
    let mut outcome = MainOutcome {
        open_settings: false,
        command: None,
        settings_changed: false,
        notice: None,
    };
    let idle = snapshot.state == TimerState::Idle;

    if widgets::main_header(ui).clicked() {
        outcome.open_settings = true;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Frame::NONE
                .inner_margin(Margin {
                    left: 16,
                    right: 16,
                    top: 4,
                    bottom: 16,
                })
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    ui.set_min_width(ui.available_width());
                    timer_block(ui, snapshot, settings, *duration_min, notice, &mut outcome);
                    ui.add_space(16.0);
                    duration_control(ui, duration_min, custom, idle);
                    ui.add_space(16.0);
                    options_group(
                        ui,
                        snapshot,
                        settings,
                        duration_min,
                        *custom,
                        idle,
                        &mut outcome,
                    );
                    notes(ui, snapshot);
                });
        });

    outcome
}

fn timer_block(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    settings: &Settings,
    duration_min: u32,
    notice: Option<&str>,
    outcome: &mut MainOutcome,
) {
    let width = ui.available_width();
    let (wrap, _) = ui.allocate_exact_size(vec2(width, RING_WRAP), Sense::hover());
    let ring = Rect::from_center_size(
        pos2(wrap.center().x, wrap.top() + RING_SIZE * 0.5),
        Vec2::splat(RING_SIZE),
    );
    let progress = ring_progress(snapshot, duration_min);
    widgets::paint_progress_ring(ui, Id::new("nightcap-ring"), ring, progress);

    let paused = snapshot.state == TimerState::Paused;
    let paused_mix =
        ui.ctx()
            .animate_bool_with_time(Id::new("nightcap-paused-digits"), paused, 0.2);
    let tokens = theme::tokens(ui.ctx());
    let digit_color = tokens
        .label
        .lerp_to_gamma(tokens.label_secondary, paused_mix);
    let time = displayed_time(snapshot, duration_min);
    let digit_size = if time.len() > 5 { 40.0 } else { 50.0 };
    widgets::paint_tabular_time(
        ui,
        ring.center() + vec2(0.0, -12.0),
        &time,
        digit_size,
        digit_color,
    );
    paint_subtitle(
        ui,
        ring.center() + vec2(0.0, 30.0),
        snapshot,
        settings,
        duration_min,
        notice,
    );

    let cancel_rect = Rect::from_min_size(
        pos2(wrap.left() + 2.0, wrap.bottom() - BUTTON),
        Vec2::splat(BUTTON),
    );
    let start_rect = Rect::from_min_size(
        pos2(wrap.right() - 2.0 - BUTTON, wrap.bottom() - BUTTON),
        Vec2::splat(BUTTON),
    );
    let running = snapshot.state == TimerState::Running;
    let (primary_label, primary_enabled) = match snapshot.state {
        TimerState::Idle => ("Start", true),
        TimerState::Running => ("Pause", true),
        TimerState::Paused => ("Resume", true),
        TimerState::Finishing => ("Start", false),
    };
    if widgets::round_button(
        ui,
        cancel_rect,
        Id::new("nightcap-cancel"),
        "Cancel",
        RoundRole::Neutral,
        running || paused,
    )
    .clicked()
    {
        outcome.command = Some(Command::Cancel);
        outcome.notice = Some(if settings.restore_on_cancel_or_wake {
            "Brightness and volume restored"
        } else {
            "Timer canceled"
        });
    }
    if widgets::round_button(
        ui,
        start_rect,
        Id::new("nightcap-primary"),
        primary_label,
        RoundRole::Accent,
        primary_enabled,
    )
    .clicked()
    {
        outcome.command = Some(match snapshot.state {
            TimerState::Running => Command::Pause,
            TimerState::Paused => Command::Resume,
            _ => Command::Start {
                duration: Duration::from_secs(u64::from(duration_min) * 60),
            },
        });
    }
}

fn duration_control(ui: &mut egui::Ui, duration_min: &mut u32, custom: &mut bool, idle: bool) {
    let labels = ["15 min", "30 min", "45 min", "60 min", "90 min", "Custom"];
    let selected = if *custom {
        5
    } else {
        PRESETS
            .iter()
            .position(|minutes| *minutes == *duration_min)
            .unwrap_or(5)
    };
    if let Some(index) =
        widgets::segmented(ui, Id::new("nightcap-presets"), &labels, selected, idle)
    {
        if index == 5 {
            *custom = true;
        } else {
            *custom = false;
            *duration_min = PRESETS[index];
        }
    }
}

fn options_group(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    settings: &mut Settings,
    duration_min: &mut u32,
    custom: bool,
    idle: bool,
    outcome: &mut MainOutcome,
) {
    widgets::InsetGroup::show(ui, |group| {
        if custom {
            group.row(idle, |ui| {
                widgets::split(
                    ui,
                    |ui| widgets::body(ui, "Length"),
                    |ui| {
                        let _ = widgets::stepper(
                            ui,
                            Id::new("nightcap-custom-minutes"),
                            duration_min,
                            MIN_MINUTES..=MAX_MINUTES,
                        );
                    },
                );
            });
        }
        group.row(true, |ui| {
            widgets::split(
                ui,
                |ui| widgets::body(ui, "When finished"),
                |ui| {
                    let choices = end_action_choices();
                    if let Some(action) = widgets::popup_select(
                        ui,
                        Id::new("nightcap-end-action"),
                        end_action_label(settings.end_action),
                        settings.end_action,
                        &choices,
                    ) {
                        settings.end_action = action;
                        outcome.settings_changed = true;
                    }
                },
            );
        });
        group.row(true, |ui| {
            let subtitle = fade_caption(&settings.dim, snapshot, snapshot.brightness_percent);
            widgets::split_stacked(
                ui,
                |ui| widgets::title_and_caption(ui, "Dim screen", &subtitle),
                |ui| {
                    if widgets::toggle(ui, Id::new("nightcap-dim"), settings.dim.enabled).clicked()
                    {
                        settings.dim.enabled = !settings.dim.enabled;
                        outcome.settings_changed = true;
                    }
                },
            );
        });
        group.row(true, |ui| {
            let subtitle = fade_caption(&settings.volume, snapshot, snapshot.volume_percent);
            widgets::split_stacked(
                ui,
                |ui| widgets::title_and_caption(ui, "Lower volume", &subtitle),
                |ui| {
                    if widgets::toggle(ui, Id::new("nightcap-volume"), settings.volume.enabled)
                        .clicked()
                    {
                        settings.volume.enabled = !settings.volume.enabled;
                        outcome.settings_changed = true;
                    }
                },
            );
        });
    });
}

fn notes(ui: &mut egui::Ui, snapshot: &Snapshot) {
    let mut lines = Vec::new();
    if !snapshot.brightness_supported {
        lines.push(
            "This display doesn’t support brightness control. Nightcap will dim it with an overlay instead."
                .to_owned(),
        );
    }
    if !snapshot.volume_supported {
        lines.push("Volume control isn’t available on this device.".to_owned());
    }
    if let Some(error) = &snapshot.last_error {
        lines.push(error.replace('\n', " "));
    }
    if lines.is_empty() {
        return;
    }
    ui.add_space(10.0);
    for line in lines {
        widgets::caption(ui, &line);
        ui.add_space(4.0);
    }
}

fn paint_subtitle(
    ui: &egui::Ui,
    center: Pos2,
    snapshot: &Snapshot,
    settings: &Settings,
    duration_min: u32,
    notice: Option<&str>,
) {
    let tokens = theme::tokens(ui.ctx());
    let text = match snapshot.state {
        TimerState::Finishing => end_action_progress(settings.end_action).to_owned(),
        TimerState::Paused => "Paused".to_owned(),
        TimerState::Idle if notice.is_some() => notice.unwrap_or("").to_owned(),
        TimerState::Idle | TimerState::Running => String::new(),
    };
    if !text.is_empty() {
        ui.painter().text(
            center,
            Align2::CENTER_CENTER,
            text,
            FontId::proportional(13.0),
            tokens.label_secondary,
        );
        return;
    }
    let when = snapshot.ends_at.unwrap_or_else(|| {
        chrono::Local::now() + chrono::Duration::minutes(i64::from(duration_min))
    });
    let clock = format_clock(when);
    let clock_width = ui.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(
                clock.clone(),
                FontId::proportional(13.0),
                tokens.label_secondary,
            )
            .size()
            .x
    });
    let moon = center - vec2((clock_width + 5.0) * 0.5, 0.0);
    widgets::paint_moon(ui.painter(), moon, tokens.label_secondary, tokens.window);
    ui.painter().text(
        center + vec2(6.0, 0.0),
        Align2::CENTER_CENTER,
        clock,
        FontId::proportional(13.0),
        tokens.label_secondary,
    );
}

fn ring_progress(snapshot: &Snapshot, duration_min: u32) -> f32 {
    match snapshot.state {
        TimerState::Idle => 1.0,
        TimerState::Finishing => 0.0,
        TimerState::Running | TimerState::Paused => {
            let total = snapshot
                .total
                .as_secs_f32()
                .max(duration_min as f32 * 60.0)
                .max(1.0);
            (snapshot.remaining.as_secs_f32() / total).clamp(0.0, 1.0)
        }
    }
}

fn displayed_time(snapshot: &Snapshot, duration_min: u32) -> String {
    match snapshot.state {
        TimerState::Idle => format_timer(Duration::from_secs(u64::from(duration_min) * 60)),
        TimerState::Running | TimerState::Paused | TimerState::Finishing => {
            format_timer(snapshot.remaining)
        }
    }
}

pub fn format_timer(duration: Duration) -> String {
    let seconds = duration.as_secs() + u64::from(duration.subsec_nanos() > 0);
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

pub fn format_clock(time: chrono::DateTime<chrono::Local>) -> String {
    let (pm, hour) = time.hour12();
    format!(
        "{}:{:02} {}",
        hour,
        time.minute(),
        if pm { "PM" } else { "AM" }
    )
}

pub fn fade_caption(fade: &FadeSettings, snapshot: &Snapshot, current: Option<f32>) -> String {
    if !fade.enabled {
        return "Off".to_owned();
    }
    let active = matches!(
        snapshot.state,
        TimerState::Running | TimerState::Paused | TimerState::Finishing
    );
    if active
        && let Some(now) = current
        && now < 99.5
    {
        return format!(
            "Now {}%, going to {}%",
            now.round() as u32,
            fade.target_percent
        );
    }
    format!(
        "To {}%, starting {} min before",
        fade.target_percent, fade.start_before_min
    )
}

pub fn end_action_label(action: EndAction) -> &'static str {
    match action {
        EndAction::Sleep => "Sleep",
        EndAction::Hibernate => "Hibernate",
        EndAction::Shutdown => "Shut Down",
    }
}

pub fn end_action_progress(action: EndAction) -> &'static str {
    match action {
        EndAction::Sleep => "Going to sleep…",
        EndAction::Hibernate => "Hibernating…",
        EndAction::Shutdown => "Shutting down…",
    }
}

fn end_action_choices() -> [Choice<'static, EndAction>; 3] {
    [
        Choice {
            label: "Sleep",
            value: EndAction::Sleep,
            enabled: true,
        },
        Choice {
            label: "Hibernate",
            value: EndAction::Hibernate,
            enabled: true,
        },
        Choice {
            label: "Shut Down",
            value: EndAction::Shutdown,
            enabled: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn timer_text_uses_minutes_until_an_hour() {
        assert_eq!(format_timer(Duration::from_secs(45 * 60)), "45:00");
        assert_eq!(format_timer(Duration::from_secs(90)), "01:30");
        assert_eq!(format_timer(Duration::from_secs(3600 + 65)), "1:01:05");
        assert_eq!(
            format_timer(Duration::from_secs(10) + Duration::from_millis(1)),
            "00:11"
        );
    }

    #[test]
    fn clock_uses_a_12_hour_face() {
        let evening = chrono::Local
            .with_ymd_and_hms(2026, 10, 9, 22, 20, 0)
            .single()
            .expect("local time");
        assert_eq!(format_clock(evening), "10:20 PM");
        let midnight = chrono::Local
            .with_ymd_and_hms(2026, 10, 9, 0, 5, 0)
            .single()
            .expect("local time");
        assert_eq!(format_clock(midnight), "12:05 AM");
    }

    #[test]
    fn fade_caption_matches_the_mockup_states() {
        let snapshot = Snapshot::default();
        let fade = FadeSettings::default();
        assert_eq!(
            fade_caption(&fade, &snapshot, None),
            "To 10%, starting 20 min before"
        );
        let mut off = fade;
        off.enabled = false;
        assert_eq!(fade_caption(&off, &snapshot, None), "Off");
        let mut running = snapshot.clone();
        running.state = TimerState::Running;
        assert_eq!(
            fade_caption(&fade, &running, Some(42.2)),
            "Now 42%, going to 10%"
        );
    }
}
