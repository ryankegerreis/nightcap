//! Progress ring, segmented control, toggles, grouped rows, steppers, and round buttons.

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Id, Layout, Margin, Pos2, Rect, Response,
    Sense, Shape, Stroke, StrokeKind, TextEdit, Ui, Vec2, pos2, vec2,
};

use super::theme::{self, ThemeTokens};

const RING_STROKE: f32 = 6.0;
const TOGGLE_W: f32 = 32.0;
const TOGGLE_H: f32 = 19.0;
const ANIM_FAST: f32 = 0.15;
const ANIM_RING: f32 = 0.25;

pub enum RoundRole {
    Neutral,
    Accent,
}

#[derive(Clone, Copy)]
pub enum MeterKind {
    Brightness,
    Volume,
}

pub struct Choice<'a, T> {
    pub label: &'a str,
    pub value: T,
    pub enabled: bool,
}

/// Full-width inset grouped list. Rows stack with hairline separators.
pub struct InsetGroup<'a> {
    ui: &'a mut Ui,
    index: u32,
}

impl InsetGroup<'_> {
    pub fn show(ui: &mut Ui, add: impl FnOnce(&mut InsetGroup<'_>)) {
        let tokens = theme::tokens(ui.ctx());
        let width = ui.available_width();
        egui::Frame::NONE
            .fill(tokens.group)
            .stroke(Stroke::new(1.0, tokens.group_edge))
            .corner_radius(CornerRadius::same(9))
            .inner_margin(Margin::ZERO)
            .show(ui, |ui| {
                ui.set_min_width(width);
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                let mut group = InsetGroup { ui, index: 0 };
                add(&mut group);
            });
    }

    /// `enabled` fades the row and blocks input, matching a dependent settings row.
    pub fn row(&mut self, enabled: bool, add: impl FnOnce(&mut Ui)) {
        let index = self.index;
        self.index += 1;
        let id = self.ui.id().with(("nightcap-row", index));
        let fade = self.ui.ctx().animate_bool_with_time(id, enabled, ANIM_FAST);
        let opacity = 0.4 + 0.6 * fade;
        self.ui.scope(|ui| {
            ui.visuals_mut().disabled_alpha = 1.0;
            ui.set_opacity(opacity);
            if !enabled {
                ui.disable();
            }
            if index > 0 {
                let tokens = theme::tokens(ui.ctx());
                let (rect, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                ui.painter().hline(
                    rect.x_range(),
                    rect.center().y,
                    Stroke::new(1.0, tokens.separator),
                );
            }
            ui.spacing_mut().item_spacing = vec2(8.0, 1.0);
            egui::Frame::NONE
                .inner_margin(Margin::symmetric(12, 7))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.set_min_height(26.0);
                    add(ui);
                });
        });
    }
}

/// One-line label on the left, control on the right.
pub fn split(ui: &mut Ui, left: impl FnOnce(&mut Ui), right: impl FnOnce(&mut Ui)) {
    // Content box is 26px; the row's 7px vertical padding makes a 40px row.
    split_height(ui, 26.0, left, right);
}

/// Title plus caption on the left. Taller clip so the second line is not cut off.
pub fn split_stacked(ui: &mut Ui, left: impl FnOnce(&mut Ui), right: impl FnOnce(&mut Ui)) {
    split_height(ui, 36.0, left, right);
}

fn split_height(
    ui: &mut Ui,
    content_height: f32,
    left: impl FnOnce(&mut Ui),
    right: impl FnOnce(&mut Ui),
) {
    let width = ui.available_width();
    // `horizontal` clips to `interact_size.y`. A right-to-left layout on the
    // parent would otherwise stretch to the rest of the scroll view.
    ui.spacing_mut().interact_size.y = content_height;
    ui.horizontal(|ui| {
        ui.set_min_width(width);
        ui.spacing_mut().item_spacing.x = 12.0;
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            right(ui);
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                left(ui);
            });
        });
    });
}

pub fn body(ui: &mut Ui, text: &str) {
    let color = theme::tokens(ui.ctx()).label;
    ui.label(egui::RichText::new(text).size(13.0).color(color));
}

pub fn caption(ui: &mut Ui, text: &str) {
    let color = theme::tokens(ui.ctx()).label_secondary;
    ui.label(egui::RichText::new(text).size(11.0).color(color));
}

pub fn title_and_caption(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        body(ui, title);
        caption(ui, subtitle);
    });
}

pub fn toggle(ui: &mut Ui, id: Id, on: bool) -> Response {
    let tokens = theme::tokens(ui.ctx());
    let (rect, response) = ui.allocate_exact_size(vec2(TOGGLE_W, TOGGLE_H), Sense::click());
    let t = ui.ctx().animate_bool_with_time(id, on, ANIM_FAST);
    let bg = tokens.fill_strong.lerp_to_gamma(tokens.accent, t);
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(10), bg);
    let travel = TOGGLE_W - 16.0 - 3.0;
    let knob_center = pos2(rect.left() + 1.5 + 8.0 + travel * t, rect.center().y);
    painter.circle_filled(
        knob_center + vec2(0.0, 0.6),
        8.0,
        Color32::from_black_alpha(36),
    );
    painter.circle_filled(knob_center, 8.0, tokens.knob);
    painter.circle_stroke(
        knob_center,
        8.0,
        Stroke::new(0.5, Color32::from_black_alpha(20)),
    );
    response
}

pub fn paint_progress_ring(ui: &Ui, id: Id, rect: Rect, progress: f32) {
    let tokens = theme::tokens(ui.ctx());
    let shown = ui
        .ctx()
        .animate_value_with_time(id, progress.clamp(0.0, 1.0), ANIM_RING);
    let center = rect.center();
    let radius = rect.width().min(rect.height()) * 0.5 - RING_STROKE * 0.5 - 1.0;
    let painter = ui.painter();
    painter.circle_stroke(center, radius, Stroke::new(RING_STROKE, tokens.fill));
    if shown >= 0.999 {
        painter.circle_stroke(center, radius, Stroke::new(RING_STROKE, tokens.accent));
        return;
    }
    if shown <= 0.001 {
        return;
    }
    let start = -std::f32::consts::FRAC_PI_2 + (1.0 - shown) * std::f32::consts::TAU;
    let sweep = shown * std::f32::consts::TAU;
    let points = arc_points(center, radius, start, sweep);
    painter.line(points.clone(), Stroke::new(RING_STROKE, tokens.accent));
    let cap = RING_STROKE * 0.5;
    if let (Some(first), Some(last)) = (points.first().copied(), points.last().copied()) {
        painter.circle_filled(first, cap, tokens.accent);
        painter.circle_filled(last, cap, tokens.accent);
    }
}

/// Fixed-width digit cells so the countdown does not jitter as digits change.
pub fn paint_tabular_time(ui: &Ui, center: Pos2, text: &str, size: f32, color: Color32) -> Rect {
    let font = FontId::new(size, egui::FontFamily::Name("countdown".into()));
    let digit_width = ui.fonts_mut(|fonts| {
        ('0'..='9')
            .map(|digit| fonts.glyph_width(&font, digit))
            .fold(0.0_f32, f32::max)
    });
    let mut widths = Vec::with_capacity(text.chars().count());
    let mut total = 0.0;
    for ch in text.chars() {
        let width = if ch.is_ascii_digit() {
            digit_width
        } else {
            ui.fonts_mut(|fonts| fonts.glyph_width(&font, ch))
        };
        widths.push(width);
        total += width;
    }
    let height = size;
    let mut x = center.x - total * 0.5;
    let painter = ui.painter();
    for (ch, width) in text.chars().zip(widths) {
        painter.text(
            pos2(x + width * 0.5, center.y),
            Align2::CENTER_CENTER,
            ch.to_string(),
            font.clone(),
            color,
        );
        x += width;
    }
    Rect::from_center_size(center, vec2(total, height))
}

pub fn paint_moon(painter: &egui::Painter, center: Pos2, color: Color32, background: Color32) {
    let radius = 5.6;
    painter.circle_filled(center, radius, color);
    painter.circle_filled(center + vec2(3.1, -0.7), 4.7, background);
}

pub fn round_button(
    ui: &mut Ui,
    rect: Rect,
    id: Id,
    label: &str,
    role: RoundRole,
    enabled: bool,
) -> Response {
    let tokens = theme::tokens(ui.ctx());
    let response = ui.interact(
        rect,
        id,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let pressed = enabled && response.is_pointer_button_down_on();
    let press = ui
        .ctx()
        .animate_bool_with_time(id.with("press"), pressed, 0.08);
    let (fill, text) = match role {
        RoundRole::Neutral => (tokens.fill_strong, tokens.label),
        RoundRole::Accent => (tokens.accent_tint, tokens.accent),
    };
    let mut fill = fill.lerp_to_gamma(fill.gamma_multiply(0.82), press);
    let mut text = text;
    if !enabled {
        fill = fill.gamma_multiply(0.4);
        text = text.gamma_multiply(0.4);
    }
    let painter = ui.painter();
    painter.circle_filled(rect.center(), rect.width() * 0.5, fill);
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(13.0),
        text,
    );
    response
}

pub fn segmented(
    ui: &mut Ui,
    id: Id,
    labels: &[&str],
    selected: usize,
    enabled: bool,
) -> Option<usize> {
    let mut clicked = None;
    ui.scope(|ui| {
        if !enabled {
            ui.visuals_mut().disabled_alpha = 1.0;
            ui.set_opacity(0.45);
            ui.disable();
        }
        let tokens = theme::tokens(ui.ctx());
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(7), tokens.fill);
        let inner = rect.shrink(2.0);
        let count = labels.len().max(1) as f32;
        let cell = inner.width() / count;
        let index = ui.ctx().animate_value_with_time(
            id.with("pill"),
            selected.min(labels.len().saturating_sub(1)) as f32,
            ANIM_FAST,
        );
        let pill = Rect::from_min_size(
            pos2(inner.left() + index * cell, inner.top()),
            vec2(cell, inner.height()),
        );
        painter.rect_filled(
            pill.translate(vec2(0.0, 1.0)),
            CornerRadius::same(5),
            Color32::from_black_alpha(28),
        );
        ui.painter().rect(
            pill,
            CornerRadius::same(5),
            tokens.segment_selected,
            Stroke::new(0.5, Color32::from_black_alpha(20)),
            StrokeKind::Inside,
        );
        for (i, label) in labels.iter().enumerate() {
            let cell_rect = Rect::from_min_size(
                pos2(inner.left() + i as f32 * cell, inner.top()),
                vec2(cell, inner.height()),
            );
            let response = ui.interact(cell_rect, id.with(i), Sense::click());
            ui.painter().text(
                cell_rect.center(),
                Align2::CENTER_CENTER,
                *label,
                FontId::proportional(12.0),
                tokens.label,
            );
            if response.clicked() {
                clicked = Some(i);
            }
        }
    });
    clicked
}

pub fn popup_select<T: Copy + PartialEq>(
    ui: &mut Ui,
    id: Id,
    current_label: &str,
    current: T,
    choices: &[Choice<'_, T>],
) -> Option<T> {
    let tokens = theme::tokens(ui.ctx());
    let font = FontId::proportional(13.0);
    let text_width = ui.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(
                current_label.to_owned(),
                font.clone(),
                tokens.label_secondary,
            )
            .size()
            .x
    });
    let width = text_width + 8.0 + 14.0 + 8.0;
    let (rect, response) = ui.allocate_exact_size(vec2(width.max(72.0), 24.0), Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), response.id.with("popup"));
    let hover =
        ui.ctx()
            .animate_bool_with_time(id.with("hover"), response.hovered() || open, ANIM_FAST);
    let painter = ui.painter();
    if hover > 0.01 {
        painter.rect_filled(
            rect,
            CornerRadius::same(5),
            tokens.fill.gamma_multiply(hover),
        );
    }
    painter.text(
        pos2(rect.left() + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        current_label,
        font,
        tokens.label_secondary,
    );
    paint_up_down(
        painter,
        pos2(rect.right() - 10.0, rect.center().y),
        tokens.label_secondary,
    );

    let mut picked = None;
    egui::Popup::menu(&response)
        .id(response.id.with("popup"))
        .align(egui::RectAlign::BOTTOM_END)
        .gap(4.0)
        .width(196.0)
        .frame(popup_frame(&tokens))
        .show(|ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 1.0);
            ui.set_min_width(176.0);
            for choice in choices {
                let item = menu_item(ui, choice.label, choice.value == current, choice.enabled);
                if item.clicked() && choice.enabled {
                    picked = Some(choice.value);
                    ui.close();
                }
            }
        });
    picked
}

fn menu_item(ui: &mut Ui, label: &str, selected: bool, enabled: bool) -> Response {
    let tokens = theme::tokens(ui.ctx());
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(
        vec2(width, 24.0),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let hover = ui
        .ctx()
        .animate_bool_with_time(response.id, response.hovered() && enabled, 0.08);
    let painter = ui.painter();
    if hover > 0.01 {
        painter.rect_filled(
            rect,
            CornerRadius::same(5),
            tokens.accent.gamma_multiply(hover),
        );
    } else if selected {
        painter.rect_filled(rect, CornerRadius::same(5), tokens.accent_tint);
    }
    let color = if !enabled {
        tokens.label_tertiary
    } else if hover > 0.6 {
        Color32::WHITE
    } else if selected {
        tokens.accent
    } else {
        tokens.label
    };
    painter.text(
        pos2(rect.left() + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.0),
        color,
    );
    response
}

fn popup_frame(tokens: &ThemeTokens) -> egui::Frame {
    egui::Frame::NONE
        .fill(tokens.menu)
        .stroke(Stroke::new(1.0, tokens.group_edge))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::same(5))
        .shadow(egui::Shadow {
            offset: [0, 8],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(if tokens.dark { 100 } else { 48 }),
        })
}

/// Minutes field with stepper buttons. Commits typed text when focus leaves.
pub fn stepper(ui: &mut Ui, id: Id, value: &mut u32, range: std::ops::RangeInclusive<u32>) -> bool {
    let tokens = theme::tokens(ui.ctx());
    let mut changed = false;
    // Parent rows are right-to-left; keep the field, unit, and stepper in reading order.
    ui.allocate_ui_with_layout(
        vec2(130.0, 22.0),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            style_stepper_field(ui, &tokens);
            let text_id = id.with("text");
            let mut text = ui
                .data(|data| data.get_temp::<String>(text_id))
                .filter(|_| ui.memory(|memory| memory.has_focus(text_id)))
                .unwrap_or_else(|| value.to_string());
            let response = TextEdit::singleline(&mut text)
                .id(text_id)
                .desired_width(52.0)
                .horizontal_align(Align::Max)
                .font(FontId::proportional(13.0))
                .margin(Margin::symmetric(6, 2))
                .char_limit(3)
                .show(ui)
                .response;
            if response.lost_focus() {
                let parsed = text.parse::<u32>().unwrap_or(*value);
                let next = parsed.clamp(*range.start(), *range.end());
                if next != *value {
                    *value = next;
                    changed = true;
                }
                text = value.to_string();
            }
            ui.data_mut(|data| data.insert_temp(text_id, text));
            caption(ui, "min");
            let (up, down) = step_pair(ui, id.with("steps"));
            if up {
                *value = value.saturating_add(1).min(*range.end());
                changed = true;
            } else if down {
                *value = value.saturating_sub(1).max(*range.start());
                changed = true;
            }
        },
    );
    changed
}

fn style_stepper_field(ui: &mut Ui, tokens: &ThemeTokens) {
    let visuals = ui.visuals_mut();
    visuals.extreme_bg_color = tokens.field;
    visuals.text_edit_bg_color = Some(tokens.field);
    visuals.widgets.inactive.bg_stroke = Stroke::new(0.5, tokens.label_tertiary);
    visuals.widgets.hovered.bg_stroke = Stroke::new(0.5, tokens.label_tertiary);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, tokens.accent);
    visuals.widgets.inactive.corner_radius = CornerRadius::same(5);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(5);
    visuals.widgets.active.corner_radius = CornerRadius::same(5);
}

fn step_pair(ui: &mut Ui, id: Id) -> (bool, bool) {
    let tokens = theme::tokens(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(vec2(15.0, 22.0), Sense::hover());
    ui.painter().rect(
        rect,
        CornerRadius::same(5),
        tokens.field,
        Stroke::new(0.5, tokens.label_tertiary),
        StrokeKind::Inside,
    );
    let up = Rect::from_min_size(rect.min, vec2(rect.width(), rect.height() * 0.5));
    let down = Rect::from_min_max(pos2(rect.left(), up.bottom()), rect.max);
    ui.painter().hline(
        up.x_range().shrink(1.0),
        up.bottom(),
        Stroke::new(0.5, tokens.separator),
    );
    let up_response = ui.interact(up, id.with("up"), Sense::click());
    let down_response = ui.interact(down, id.with("down"), Sense::click());
    let painter = ui.painter();
    if up_response.is_pointer_button_down_on() {
        painter.rect_filled(up, CornerRadius::ZERO, tokens.fill_strong);
    }
    if down_response.is_pointer_button_down_on() {
        painter.rect_filled(down, CornerRadius::ZERO, tokens.fill_strong);
    }
    paint_chevron(
        painter,
        up.center() + vec2(0.0, 1.0),
        true,
        tokens.label_secondary,
    );
    paint_chevron(
        painter,
        down.center() + vec2(0.0, -1.0),
        false,
        tokens.label_secondary,
    );
    (up_response.clicked(), down_response.clicked())
}

pub fn meter_slider(ui: &mut Ui, id: Id, value: &mut u8, kind: MeterKind) -> bool {
    let tokens = theme::tokens(ui.ctx());
    let mut changed = false;
    // The row places this control with a right-to-left layout. Force left-to-right
    // so the small icon, track, large icon, and percent stay in that order.
    let width = 13.0 + 8.0 + 150.0 + 8.0 + 17.0 + 8.0 + 34.0;
    ui.allocate_ui_with_layout(
        vec2(width, 22.0),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            paint_meter_icon(ui, kind, false);
            changed = slider_track(ui, id, value, 0, 90, 5);
            paint_meter_icon(ui, kind, true);
            let percent = format!("{value}%");
            let (rect, _) = ui.allocate_exact_size(vec2(34.0, 18.0), Sense::hover());
            ui.painter().text(
                rect.right_center(),
                Align2::RIGHT_CENTER,
                percent,
                FontId::proportional(13.0),
                tokens.label_secondary,
            );
        },
    );
    changed
}

fn slider_track(ui: &mut Ui, id: Id, value: &mut u8, min: u8, max: u8, step: u8) -> bool {
    let tokens = theme::tokens(ui.ctx());
    let (rect, response) = ui.allocate_exact_size(vec2(150.0, 22.0), Sense::click_and_drag());
    let mut changed = false;
    if let Some(pointer) = response.interact_pointer_pos() {
        let t = ((pointer.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let raw = f32::from(min) + t * f32::from(max - min);
        let stepped = ((raw / f32::from(step)).round() as u8).saturating_mul(step);
        let next = stepped.clamp(min, max);
        if next != *value {
            *value = next;
            changed = true;
        }
    }
    let clamped = (*value).clamp(min, max);
    let target = (f32::from(clamped) - f32::from(min)) / f32::from(max - min);
    let shown = if response.dragged() {
        ui.ctx().animate_value_with_time(id, target, 0.0);
        target
    } else {
        ui.ctx().animate_value_with_time(id, target, 0.12)
    };
    let track = Rect::from_center_size(rect.center(), vec2(rect.width(), 4.0));
    let split_x = track.left() + track.width() * shown;
    let painter = ui.painter();
    painter.rect_filled(track, CornerRadius::same(2), tokens.fill_strong);
    let filled = Rect::from_min_max(track.min, pos2(split_x.max(track.left()), track.bottom()));
    if filled.width() > 0.5 {
        painter.rect_filled(filled, CornerRadius::same(2), tokens.accent);
    }
    let knob = pos2(split_x, rect.center().y);
    painter.circle_filled(knob + vec2(0.0, 0.6), 9.0, Color32::from_black_alpha(40));
    painter.circle_filled(knob, 9.0, tokens.knob);
    painter.circle_stroke(knob, 9.0, Stroke::new(0.5, Color32::from_black_alpha(24)));
    changed
}

fn paint_meter_icon(ui: &mut Ui, kind: MeterKind, large: bool) {
    let tokens = theme::tokens(ui.ctx());
    let size = if large { 15.0 } else { 11.0 };
    let (rect, _) = ui.allocate_exact_size(vec2(size + 2.0, 18.0), Sense::hover());
    let painter = ui.painter();
    match kind {
        MeterKind::Brightness => paint_sun(painter, rect.center(), large, tokens.label_secondary),
        MeterKind::Volume => paint_speaker(painter, rect.center(), large, tokens.label_secondary),
    }
}

/// Toolbar: centered "Nightcap" title and the sliders button that opens Settings.
pub fn main_header(ui: &mut Ui) -> Response {
    let tokens = theme::tokens(ui.ctx());
    let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::hover());
    if cfg!(target_os = "macos") {
        paint_header_title(ui, bar, "Nightcap");
    }
    let drag = Rect::from_min_max(
        pos2(bar.left() + 80.0, bar.top()),
        pos2(bar.right() - 45.0, bar.bottom()),
    );
    header_drag(ui, drag);
    let button = Rect::from_center_size(pos2(bar.right() - 25.0, bar.center().y), vec2(30.0, 24.0));
    let response = ui.interact(button, Id::new("nightcap-settings-button"), Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(response.id, response.hovered(), ANIM_FAST);
    if hover > 0.01 {
        ui.painter().rect_filled(
            button,
            CornerRadius::same(6),
            tokens.fill.gamma_multiply(hover),
        );
    }
    paint_sliders_icon(ui.painter(), button.center(), tokens.label_secondary);
    response.on_hover_text("Settings")
}

pub fn paint_sliders_icon(painter: &egui::Painter, center: Pos2, color: Color32) {
    let origin = center - vec2(8.0, 8.0);
    let stroke = Stroke::new(1.3, color);
    let y1 = origin.y + 4.5;
    let y2 = origin.y + 11.5;
    painter.line_segment([pos2(origin.x + 2.0, y1), pos2(origin.x + 9.0, y1)], stroke);
    painter.line_segment(
        [pos2(origin.x + 12.0, y1), pos2(origin.x + 14.0, y1)],
        stroke,
    );
    painter.line_segment([pos2(origin.x + 2.0, y2), pos2(origin.x + 4.0, y2)], stroke);
    painter.line_segment(
        [pos2(origin.x + 7.0, y2), pos2(origin.x + 14.0, y2)],
        stroke,
    );
    painter.circle_stroke(pos2(origin.x + 10.5, y1), 1.6, stroke);
    painter.circle_stroke(pos2(origin.x + 5.5, y2), 1.6, stroke);
}

fn paint_sun(painter: &egui::Painter, center: Pos2, large: bool, color: Color32) {
    let (radius, ray, rays) = if large { (3.1, 2.3, 8) } else { (2.0, 1.5, 4) };
    let stroke = Stroke::new(if large { 1.3 } else { 1.1 }, color);
    painter.circle_stroke(center, radius, stroke);
    for i in 0..rays {
        let angle = i as f32 * std::f32::consts::TAU / rays as f32 - std::f32::consts::FRAC_PI_2;
        let dir = Vec2::angled(angle);
        painter.line_segment(
            [
                center + dir * (radius + 1.3),
                center + dir * (radius + 1.3 + ray),
            ],
            stroke,
        );
    }
}

fn paint_speaker(painter: &egui::Painter, center: Pos2, large: bool, color: Color32) {
    if large {
        // viewBox 0 0 18 16, drawn at 17×15.
        let scale = 15.0 / 16.0;
        let origin = center - vec2(9.0, 8.0) * scale;
        let p = |x: f32, y: f32| origin + vec2(x, y) * scale;
        painter.add(Shape::convex_polygon(
            vec![
                p(1.5, 5.5),
                p(4.1, 5.5),
                p(7.7, 2.5),
                p(7.7, 13.5),
                p(4.1, 10.5),
                p(1.5, 10.5),
            ],
            color,
            Stroke::NONE,
        ));
        let stroke = Stroke::new(1.3 * scale, color);
        paint_bulge_right(painter, p(10.5, 5.5), p(10.5, 10.5), 3.5 * scale, stroke);
        paint_bulge_right(painter, p(12.8, 3.3), p(12.8, 12.7), 6.6 * scale, stroke);
    } else {
        // viewBox 0 0 12 12, drawn at 11×11. Body only, no sound waves.
        let scale = 11.0 / 12.0;
        let origin = center - vec2(6.0, 6.0) * scale;
        let p = |x: f32, y: f32| origin + vec2(x, y) * scale;
        painter.add(Shape::convex_polygon(
            vec![
                p(1.5, 4.3),
                p(3.5, 4.3),
                p(6.5, 1.8),
                p(6.5, 10.2),
                p(3.5, 7.7),
                p(1.5, 7.7),
            ],
            color,
            Stroke::NONE,
        ));
    }
}

/// Short arc bulging to the right of a vertical chord, matching the volume SVG.
fn paint_bulge_right(painter: &egui::Painter, start: Pos2, end: Pos2, radius: f32, stroke: Stroke) {
    let mid = pos2((start.x + end.x) * 0.5, (start.y + end.y) * 0.5);
    let half = start.distance(end) * 0.5;
    let dist = (radius * radius - half * half).max(0.0).sqrt();
    let center = pos2(mid.x - dist, mid.y);
    let start_angle = (start - center).angle();
    let mut sweep = (end - center).angle() - start_angle;
    if sweep < 0.0 {
        sweep += std::f32::consts::TAU;
    }
    if sweep > std::f32::consts::PI {
        sweep -= std::f32::consts::TAU;
    }
    painter.line(arc_points(center, radius, start_angle, sweep), stroke);
}

fn paint_chevron(painter: &egui::Painter, center: Pos2, up: bool, color: Color32) {
    // Negative Y is up. `up` puts the point above the wings.
    let dir = if up { -1.0 } else { 1.0 };
    painter.line(
        vec![
            center + vec2(-2.7, -dir * 1.05),
            center + vec2(0.0, dir * 1.45),
            center + vec2(2.7, -dir * 1.05),
        ],
        Stroke::new(1.15, color),
    );
}

fn paint_up_down(painter: &egui::Painter, center: Pos2, color: Color32) {
    paint_chevron(painter, center + vec2(0.0, -3.6), true, color);
    paint_chevron(painter, center + vec2(0.0, 3.6), false, color);
}

fn arc_points(center: Pos2, radius: f32, start: f32, sweep: f32) -> Vec<Pos2> {
    let steps = ((sweep.abs() / std::f32::consts::TAU) * 96.0)
        .ceil()
        .max(2.0) as usize;
    (0..=steps)
        .map(|step| {
            let t = step as f32 / steps as f32;
            center + Vec2::angled(start + sweep * t) * radius
        })
        .collect()
}

/// A unified macOS header; other platforms retain their native window title.
pub fn settings_header(ui: &mut Ui) {
    if cfg!(target_os = "macos") {
        let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::hover());
        paint_header_title(ui, bar, "Settings");
        header_drag(
            ui,
            Rect::from_min_max(pos2(bar.left() + 80.0, bar.top()), bar.max),
        );
    }
}

fn paint_header_title(ui: &Ui, bar: Rect, title: &str) {
    ui.painter().text(
        bar.center(),
        Align2::CENTER_CENTER,
        title,
        FontId::proportional(13.0),
        theme::tokens(ui.ctx()).label,
    );
}

fn header_drag(ui: &Ui, rect: Rect) {
    if ui
        .interact(rect, ui.id().with("header-drag"), Sense::drag())
        .drag_started()
    {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
}
