//! System light/dark colors, typography, and spacing tokens from the design mockup.

use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{self, Color32, CornerRadius, FontId, Margin, Shadow, Stroke, TextStyle, Theme};

/// Colors taken from `docs/design/mockup.html` (`.light` / `.dark`).
#[derive(Clone, Copy, Debug)]
pub struct ThemeTokens {
    pub window: Color32,
    pub group: Color32,
    pub group_edge: Color32,
    pub label: Color32,
    pub label_secondary: Color32,
    pub label_tertiary: Color32,
    pub separator: Color32,
    pub fill: Color32,
    pub fill_strong: Color32,
    pub segment_selected: Color32,
    pub knob: Color32,
    pub accent: Color32,
    pub accent_tint: Color32,
    pub field: Color32,
    pub menu: Color32,
    pub dark: bool,
}

impl ThemeTokens {
    pub fn light() -> Self {
        Self {
            window: Color32::from_rgb(0xF4, 0xF4, 0xF4),
            group: Color32::WHITE,
            group_edge: black_alpha(0.07),
            label: black_alpha(0.85),
            label_secondary: black_alpha(0.50),
            label_tertiary: black_alpha(0.25),
            separator: black_alpha(0.08),
            fill: black_alpha(0.055),
            fill_strong: black_alpha(0.09),
            segment_selected: Color32::WHITE,
            knob: Color32::WHITE,
            accent: Color32::from_rgb(0x58, 0x56, 0xD6),
            accent_tint: Color32::from_rgba_unmultiplied(0x58, 0x56, 0xD6, alpha(0.13)),
            field: Color32::WHITE,
            menu: Color32::from_rgba_unmultiplied(246, 246, 246, alpha(0.97)),
            dark: false,
        }
    }

    pub fn dark() -> Self {
        Self {
            window: Color32::from_rgb(0x1F, 0x1F, 0x1F),
            group: Color32::from_rgb(0x2A, 0x2A, 0x2A),
            group_edge: white_alpha(0.06),
            label: white_alpha(0.88),
            label_secondary: white_alpha(0.55),
            label_tertiary: white_alpha(0.25),
            separator: white_alpha(0.08),
            fill: white_alpha(0.08),
            fill_strong: white_alpha(0.14),
            segment_selected: Color32::from_rgb(0x64, 0x64, 0x66),
            knob: Color32::from_rgb(0xE8, 0xE8, 0xE8),
            accent: Color32::from_rgb(0x5E, 0x5C, 0xE6),
            accent_tint: Color32::from_rgba_unmultiplied(0x5E, 0x5C, 0xE6, alpha(0.24)),
            field: white_alpha(0.06),
            menu: Color32::from_rgba_unmultiplied(44, 44, 46, alpha(0.97)),
            dark: true,
        }
    }

    pub fn for_theme(theme: Theme) -> Self {
        match theme {
            Theme::Dark => Self::dark(),
            Theme::Light => Self::light(),
        }
    }
}

/// Tokens for the theme egui is currently painting with.
pub fn tokens(ctx: &egui::Context) -> ThemeTokens {
    ThemeTokens::for_theme(ctx.theme())
}

/// Install both palettes once. [`egui::ThemePreference::System`] then picks between them.
pub fn install(ctx: &egui::Context) {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::Relaxed) {
        return;
    }
    ctx.style_mut_of(Theme::Light, |style| apply(&ThemeTokens::light(), style));
    ctx.style_mut_of(Theme::Dark, |style| apply(&ThemeTokens::dark(), style));
}

fn apply(tokens: &ThemeTokens, style: &mut egui::Style) {
    let visuals = &mut style.visuals;
    visuals.dark_mode = tokens.dark;
    visuals.override_text_color = Some(tokens.label);
    visuals.weak_text_color = Some(tokens.label_secondary);
    visuals.hyperlink_color = tokens.accent;
    visuals.faint_bg_color = tokens.fill;
    visuals.extreme_bg_color = tokens.field;
    visuals.text_edit_bg_color = Some(tokens.field);
    visuals.code_bg_color = tokens.fill;
    visuals.panel_fill = tokens.window;
    visuals.window_fill = tokens.window;
    visuals.window_stroke = Stroke::NONE;
    visuals.window_shadow = Shadow::NONE;
    visuals.window_highlight_topmost = false;
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.menu_corner_radius = CornerRadius::same(9);
    visuals.popup_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(if tokens.dark { 90 } else { 40 }),
    };
    visuals.selection.bg_fill = tokens.accent_tint;
    visuals.selection.stroke = Stroke::new(1.0, tokens.accent);
    visuals.warn_fg_color = tokens.label_secondary;
    visuals.error_fg_color = tokens.label_secondary;
    visuals.disabled_alpha = 0.45;
    visuals.button_frame = false;
    visuals.collapsing_header_frame = false;
    visuals.indent_has_left_vline = false;
    visuals.striped = false;
    visuals.interact_cursor = None;
    visuals.slider_trailing_fill = true;
    visuals.handle_shape = egui::style::HandleShape::Circle;

    let rounding = CornerRadius::same(6);
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = rounding;
        widget.expansion = 0.0;
        widget.fg_stroke.color = tokens.label;
        widget.bg_stroke = Stroke::NONE;
    }
    visuals.widgets.noninteractive.bg_fill = tokens.window;
    visuals.widgets.noninteractive.weak_bg_fill = Color32::TRANSPARENT;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, tokens.label);
    visuals.widgets.inactive.bg_fill = tokens.fill;
    visuals.widgets.inactive.weak_bg_fill = tokens.fill;
    visuals.widgets.hovered.bg_fill = tokens.fill_strong;
    visuals.widgets.hovered.weak_bg_fill = tokens.fill;
    visuals.widgets.hovered.fg_stroke.color = tokens.label;
    visuals.widgets.active.bg_fill = tokens.fill_strong;
    visuals.widgets.active.weak_bg_fill = tokens.fill_strong;
    visuals.widgets.open.bg_fill = tokens.fill_strong;
    visuals.widgets.open.weak_bg_fill = tokens.fill;

    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(8.0, 2.0);
    style.spacing.window_margin = Margin::ZERO;
    style.spacing.menu_margin = Margin::same(5);
    style.spacing.interact_size = egui::vec2(32.0, 19.0);
    style.spacing.scroll.floating = true;
    style.spacing.scroll.bar_width = 6.0;
    style.spacing.scroll.content_margin = Margin::ZERO;
    style.animation_time = 0.15;
    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(13.0));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(13.0));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(11.0));
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::proportional(13.0));
    style
        .text_styles
        .insert(TextStyle::Monospace, FontId::monospace(12.0));
}

fn alpha(opacity: f32) -> u8 {
    (opacity.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn black_alpha(opacity: f32) -> Color32 {
    Color32::from_black_alpha(alpha(opacity))
}

fn white_alpha(opacity: f32) -> Color32 {
    Color32::from_white_alpha(alpha(opacity))
}
