//! The one palette both views draw from, plus the egui style that matches it.

pub const BACKGROUND: egui::Color32 = egui::Color32::from_rgb(0x0c, 0x11, 0x14);
/// A shade back from [`BACKGROUND`], used for chrome that frames the content.
pub const SURFACE: egui::Color32 = egui::Color32::from_rgb(0x09, 0x0d, 0x10);
pub const CARD_BACKGROUND: egui::Color32 = egui::Color32::from_rgb(0x12, 0x1a, 0x1e);
/// Buttons need to stand off [`CARD_BACKGROUND`], or they read as plain text.
pub const BUTTON: egui::Color32 = egui::Color32::from_rgb(0x1e, 0x2a, 0x30);
pub const CARD_SELECTED: egui::Color32 = egui::Color32::from_rgb(0x15, 0x2a, 0x30);
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x4a, 0xd6, 0xb0);
/// Backs selected query text; dark enough that the glyphs on top stay legible.
pub const SELECTION: egui::Color32 = egui::Color32::from_rgb(0x1d, 0x53, 0x49);
pub const TEXT: egui::Color32 = egui::Color32::from_rgb(0x9a, 0xac, 0xb2);
pub const TEXT_SELECTED: egui::Color32 = egui::Color32::from_rgb(0xe6, 0xf2, 0xf5);
pub const DANGER: egui::Color32 = egui::Color32::from_rgb(0xe8, 0x6a, 0x6a);

/// Repaints egui's stock dark theme in the palette above.
///
/// The picker paints its own frames and needs almost none of this, but the
/// control centre uses real widgets, and egui's defaults are a much lighter
/// grey that would read as a different application.
pub fn install(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = BACKGROUND;
    visuals.extreme_bg_color = egui::Color32::from_rgb(0x08, 0x0c, 0x0f);
    visuals.faint_bg_color = CARD_BACKGROUND;
    visuals.override_text_color = Some(TEXT);
    visuals.hyperlink_color = ACCENT;
    visuals.selection.bg_fill = CARD_SELECTED;
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, ACCENT);

    visuals.widgets.noninteractive.bg_fill = CARD_BACKGROUND;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, CARD_BACKGROUND);
    visuals.widgets.inactive.bg_fill = BUTTON;
    visuals.widgets.inactive.weak_bg_fill = BUTTON;
    visuals.widgets.hovered.bg_fill = CARD_SELECTED;
    visuals.widgets.hovered.weak_bg_fill = CARD_SELECTED;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.active.bg_fill = CARD_SELECTED;
    visuals.widgets.active.weak_bg_fill = CARD_SELECTED;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, ACCENT);

    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::same(4);
    }

    ctx.set_visuals(visuals);
}
