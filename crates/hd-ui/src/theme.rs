//! Shared HD desktop visual language.
//!
//! These tokens intentionally avoid platform-branded chrome. Windows, Linux, and macOS use the
//! same spacing, contrast, radius, and interaction hierarchy while retaining their system font.

#![allow(dead_code)] // Each binary consumes a different subset of the shared tokens.

use eframe::egui;

pub const APP: egui::Color32 = egui::Color32::from_rgb(13, 17, 23);
pub const PANEL: egui::Color32 = egui::Color32::from_rgb(18, 24, 32);
pub const SURFACE: egui::Color32 = egui::Color32::from_rgb(23, 31, 42);
pub const SURFACE_HOVER: egui::Color32 = egui::Color32::from_rgb(31, 42, 56);
pub const BORDER: egui::Color32 = egui::Color32::from_rgb(45, 57, 73);
pub const TEXT: egui::Color32 = egui::Color32::from_rgb(232, 238, 247);
pub const TEXT_MUTED: egui::Color32 = egui::Color32::from_rgb(161, 174, 193);
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(82, 139, 232);
pub const ACCENT_SOFT: egui::Color32 = egui::Color32::from_rgb(31, 54, 88);
pub const SUCCESS: egui::Color32 = egui::Color32::from_rgb(91, 190, 139);
pub const WARNING: egui::Color32 = egui::Color32::from_rgb(224, 177, 88);
pub const DANGER: egui::Color32 = egui::Color32::from_rgb(229, 106, 116);

// Player chrome is a neutral, translucent material. The root window stays transparent, so the
// material blends with the desktop behind it; the Android surface itself remains an opaque black
// floating layer and is still rendered directly by gfxstream/crosvm.
pub const PLAYER_APP: egui::Color32 = egui::Color32::TRANSPARENT;
pub const PLAYER_PANEL: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(246, 248, 250, 218);
pub const PLAYER_SURFACE: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 184);
pub const PLAYER_TOOL_HOVER: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(255, 255, 255, 102);
pub const PLAYER_TOOL_ACTIVE: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(222, 228, 234, 172);
pub const PLAYER_BORDER: egui::Color32 =
    egui::Color32::from_rgba_unmultiplied_const(84, 96, 110, 42);
pub const PLAYER_TEXT: egui::Color32 = egui::Color32::from_rgb(30, 38, 50);
pub const PLAYER_TEXT_MUTED: egui::Color32 = egui::Color32::from_rgb(98, 108, 124);
pub const PLAYER_ICON: egui::Color32 = egui::Color32::from_rgb(50, 61, 77);
pub const PLAYER_ICON_DISABLED: egui::Color32 = egui::Color32::from_rgb(174, 181, 191);
pub const PLAYER_ACCENT: egui::Color32 = egui::Color32::from_rgb(47, 111, 237);
pub const PLAYER_SUCCESS: egui::Color32 = egui::Color32::from_rgb(28, 139, 91);
pub const PLAYER_WARNING: egui::Color32 = egui::Color32::from_rgb(173, 105, 19);
pub const PLAYER_DANGER: egui::Color32 = egui::Color32::from_rgb(201, 55, 61);
pub const PLAYER_DISPLAY_WELL: egui::Color32 = egui::Color32::BLACK;
pub const PLAYER_DISPLAY_TEXT: egui::Color32 = egui::Color32::from_rgb(242, 245, 249);
pub const PLAYER_DISPLAY_TEXT_MUTED: egui::Color32 = egui::Color32::from_rgb(171, 181, 195);

pub fn install(context: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = APP;
    visuals.window_fill = PANEL;
    visuals.window_stroke = egui::Stroke::new(1.0, BORDER);
    visuals.faint_bg_color = SURFACE;
    visuals.extreme_bg_color = APP;
    visuals.selection.bg_fill = ACCENT_SOFT;
    visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    visuals.hyperlink_color = ACCENT;
    visuals.warn_fg_color = WARNING;
    visuals.error_fg_color = DANGER;
    visuals.widgets.noninteractive.bg_fill = PANEL;
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, TEXT_MUTED);
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.bg_fill = SURFACE;
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, TEXT);
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    visuals.widgets.hovered.bg_fill = SURFACE_HOVER;
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, TEXT);
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    visuals.widgets.active.bg_fill = ACCENT_SOFT;
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, TEXT);
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    visuals.widgets.open.bg_fill = ACCENT_SOFT;
    visuals.widgets.open.fg_stroke = egui::Stroke::new(1.0, TEXT);
    visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    context.set_visuals(visuals);

    context.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(14.0, 9.0);
        style.spacing.interact_size = egui::vec2(44.0, 44.0);
        style.spacing.window_margin = egui::Margin::same(16);
        style.visuals.menu_corner_radius = egui::CornerRadius::same(10);
        style.visuals.window_corner_radius = egui::CornerRadius::same(12);
        style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(8);
        style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(8);
        style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(8);
        style.visuals.widgets.open.corner_radius = egui::CornerRadius::same(8);
    });
}

pub fn install_player(context: &egui::Context) {
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = PLAYER_APP;
    visuals.window_fill = PLAYER_PANEL;
    visuals.window_stroke = egui::Stroke::new(1.0, PLAYER_BORDER);
    visuals.faint_bg_color = PLAYER_SURFACE;
    visuals.extreme_bg_color = PLAYER_SURFACE;
    visuals.selection.bg_fill = egui::Color32::from_rgb(219, 231, 253);
    visuals.selection.stroke = egui::Stroke::new(1.0, PLAYER_ACCENT);
    visuals.hyperlink_color = PLAYER_ACCENT;
    visuals.warn_fg_color = PLAYER_WARNING;
    visuals.error_fg_color = PLAYER_DANGER;
    visuals.widgets.noninteractive.bg_fill = PLAYER_PANEL;
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, PLAYER_TEXT_MUTED);
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, PLAYER_BORDER);
    visuals.widgets.inactive.bg_fill = PLAYER_SURFACE;
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, PLAYER_TEXT);
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, PLAYER_BORDER);
    visuals.widgets.hovered.bg_fill = PLAYER_TOOL_HOVER;
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, PLAYER_TEXT);
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, PLAYER_ACCENT);
    visuals.widgets.active.bg_fill = PLAYER_TOOL_ACTIVE;
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, PLAYER_TEXT);
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, PLAYER_ACCENT);
    visuals.widgets.open.bg_fill = PLAYER_TOOL_HOVER;
    visuals.widgets.open.fg_stroke = egui::Stroke::new(1.0, PLAYER_TEXT);
    visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, PLAYER_ACCENT);
    context.set_visuals(visuals);

    context.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(6.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        style.spacing.interact_size = egui::vec2(32.0, 32.0);
        style.spacing.window_margin = egui::Margin::same(12);
        style.visuals.menu_corner_radius = egui::CornerRadius::same(6);
        style.visuals.window_corner_radius = egui::CornerRadius::same(8);
        style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(5);
        style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(5);
        style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(5);
        style.visuals.widgets.open.corner_radius = egui::CornerRadius::same(5);
    });
}

pub fn panel_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(egui::Stroke::new(1.0, BORDER))
        .inner_margin(egui::Margin::same(12))
}

pub fn player_chrome_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(PLAYER_PANEL)
        .stroke(egui::Stroke::new(1.0, PLAYER_BORDER))
        .inner_margin(egui::Margin::ZERO)
}

pub fn player_content_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(PLAYER_APP)
        .inner_margin(egui::Margin::ZERO)
}
