use std::{path::Path, time::Duration};

use eframe::egui::{self, Color32, CornerRadius, Frame, Margin, RichText, Stroke, Vec2};

pub(super) const BACKGROUND: Color32 = Color32::from_rgb(12, 17, 20);
pub(super) const SURFACE: Color32 = Color32::from_rgb(20, 28, 32);
pub(super) const BORDER: Color32 = Color32::from_rgb(44, 63, 70);
pub(super) const CYAN: Color32 = Color32::from_rgb(35, 200, 226);
pub(super) const MUTED: Color32 = Color32::from_rgb(137, 154, 160);
pub(super) const ERROR: Color32 = Color32::from_rgb(242, 119, 119);
pub(super) const SUCCESS: Color32 = Color32::from_rgb(105, 219, 160);
pub(super) const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);
pub(super) const HISTORY_GRID_GAP: f32 = 8.0;
pub(super) const QUICK_HISTORY_CARD_HEIGHT: f32 = 124.0;
pub(super) const MANAGEMENT_HISTORY_CARD_HEIGHT: f32 = 158.0;
pub(super) const HISTORY_METADATA_HEIGHT: f32 = 42.0;
pub(super) const HISTORY_ACTIONS_HEIGHT: f32 = 30.0;
pub(super) const HISTORY_SELECTION_HEIGHT: f32 = 18.0;
pub(super) const NARROW_NAVIGATION_THRESHOLD: f32 = 620.0;
pub(super) const QUICK_HISTORY_FOOTER_HEIGHT: f32 = 34.0;
pub(super) const HISTORY_SEARCH_ROW_COUNT: usize = 1;
pub(super) const HISTORY_FOCUSED_POLL: Duration = Duration::from_secs(1);
pub(super) const HISTORY_UNFOCUSED_POLL: Duration = Duration::from_secs(5);
pub(super) const HISTORY_MAX_BACKOFF: Duration = Duration::from_secs(30);
pub(super) const MAX_UI_SIGNAL_BYTES: u64 = 32;
pub(super) const MAX_UI_IPC_CONCURRENCY: usize = 8;
pub(super) const UI_IPC_QUEUE_CAPACITY: usize = 32;
pub(super) const MAX_IMAGE_PREVIEW_WIDTH: u32 = 320;
pub(super) const MAX_IMAGE_PREVIEW_HEIGHT: u32 = 180;
pub(super) const APP_ID: &str = "clip-sync-switcher";
pub(super) const WINDOW_TITLE: &str = "ClipSync";
pub(super) fn decode_brand_icon() -> Result<egui::IconData, String> {
    let image = image::load_from_memory(include_bytes!("../../assets/icon-64.png"))
        .map_err(|error| format!("could not decode embedded application icon: {error}"))?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Ok(egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    })
}

pub(super) fn load_brand_texture(context: &egui::Context) -> Result<egui::TextureHandle, String> {
    let image = image::load_from_memory(include_bytes!("../../assets/icon-64.png"))
        .map_err(|error| format!("could not decode embedded brand icon: {error}"))?
        .into_rgba8();
    let size = [
        usize::try_from(image.width()).map_err(|_| "brand icon width does not fit usize")?,
        usize::try_from(image.height()).map_err(|_| "brand icon height does not fit usize")?,
    ];
    Ok(context.load_texture(
        "clip-sync-brand-icon",
        egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw()),
        egui::TextureOptions::LINEAR,
    ))
}

pub(super) fn configure_style(context: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = BACKGROUND;
    visuals.extreme_bg_color = SURFACE;
    visuals.faint_bg_color = SURFACE;
    visuals.selection.bg_fill = CYAN;
    visuals.selection.stroke = Stroke::new(1.0, Color32::BLACK);
    visuals.widgets.inactive.bg_fill = SURFACE;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, CYAN);
    visuals.widgets.active.bg_fill = CYAN;
    visuals.window_corner_radius = CornerRadius::same(10);
    context.set_visuals(visuals);

    let mut style = (*context.style_of(egui::Theme::Dark)).clone();
    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.spacing.button_padding = Vec2::new(12.0, 8.0);
    context.set_style_of(egui::Theme::Dark, style);
}

pub(super) fn brand_header(ui: &mut egui::Ui, icon: &egui::TextureHandle) {
    ui.horizontal(|ui| {
        ui.add(egui::Image::new(icon).fit_to_exact_size(Vec2::splat(28.0)));
        ui.add_space(3.0);
        ui.label(RichText::new("ClipSync").color(Color32::WHITE).size(20.0));
    });
}

pub(super) fn unavailable_banner(
    ui: &mut egui::Ui,
    socket: &Path,
    error: &str,
    retry: impl FnOnce(),
) {
    ui.horizontal_wrapped(|ui| {
        ui.colored_label(ERROR, "Daemon unavailable");
        ui.label(error);
        ui.label(RichText::new(socket.display().to_string()).color(MUTED));
        if ui.small_button("Retry").clicked() {
            retry();
        }
    });
    ui.separator();
}

pub(super) fn message_panel(ui: &mut egui::Ui, message: &str, color: Color32) {
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_min_height(90.0);
            ui.vertical_centered(|ui| {
                ui.add_space(22.0);
                ui.label(RichText::new(message).color(color).size(14.0));
            });
        });
}

pub(super) fn setting_row(ui: &mut egui::Ui, name: &str, value: String) {
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong(name);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(value).color(MUTED).monospace());
                });
            });
        });
}

pub(super) struct Notice {
    message: String,
    error: bool,
}

impl Notice {
    pub(super) fn success(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error: false,
        }
    }

    pub(super) fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error: true,
        }
    }

    pub(super) fn show(&self, ui: &mut egui::Ui) {
        ui.colored_label(if self.error { ERROR } else { SUCCESS }, &self.message);
    }
}

pub(super) fn config_pointer(config: &serde_json::Value, pointer: &str) -> String {
    config.pointer(pointer).map_or_else(
        || "unavailable".to_owned(),
        |value| match value {
            serde_json::Value::String(value) => value.clone(),
            other => other.to_string(),
        },
    )
}

pub(super) fn config_pointer_u64(config: &serde_json::Value, pointer: &str) -> Option<u64> {
    config.pointer(pointer).and_then(serde_json::Value::as_u64)
}

pub(super) fn config_seconds(config: &serde_json::Value, pointer: &str) -> String {
    config_pointer_u64(config, pointer).map_or_else(
        || "unavailable".to_owned(),
        |value| format!("{value} seconds"),
    )
}

pub(super) fn management_grid_columns(width: f32) -> usize {
    if width >= NARROW_NAVIGATION_THRESHOLD {
        2
    } else {
        1
    }
}

pub(super) fn parse_byte_size(input: &str) -> Option<u64> {
    let input = input.trim();
    let split_at = input
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(input.len());
    let number = &input[..split_at];
    let suffix = input[split_at..].trim().to_ascii_lowercase();
    let multiplier = match suffix.as_str() {
        "" | "b" => 1_u64,
        "kb" => 1_000,
        "kib" => 1_024,
        "mb" => 1_000_000,
        "mib" => 1_048_576,
        "gb" => 1_000_000_000,
        "gib" => 1_073_741_824,
        _ => return None,
    };
    let decimal = number.split_once('.');
    let (whole, fraction) = decimal.map_or((number, ""), |parts| parts);
    if whole.is_empty()
        || decimal.is_some_and(|(_, fraction)| fraction.is_empty())
        || (multiplier == 1 && decimal.is_some())
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 6
    {
        return None;
    }
    let whole = whole.parse::<u64>().ok()?.checked_mul(multiplier)?;
    if fraction.is_empty() {
        return Some(whole);
    }
    let scale = 10_u64.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    let fractional_bytes = fraction.parse::<u64>().ok()?.checked_mul(multiplier)?;
    if !fractional_bytes.is_multiple_of(scale) {
        return None;
    }
    whole.checked_add(fractional_bytes / scale)
}

pub(super) fn format_bytes_input(bytes: u64) -> String {
    const UNITS: [(u64, &str); 3] = [
        (1024 * 1024 * 1024, "GiB"),
        (1024 * 1024, "MiB"),
        (1024, "KiB"),
    ];
    for (unit, suffix) in UNITS {
        if bytes >= unit && bytes.is_multiple_of(unit) {
            return format!("{} {suffix}", bytes / unit);
        }
    }
    format!("{bytes} B")
}

pub(super) fn format_bytes_exact(bytes: u64) -> String {
    let readable = format_bytes(bytes);
    if parse_byte_size(&readable) == Some(bytes) {
        readable
    } else {
        format!("{readable} ({bytes} B exact)")
    }
}

pub(super) fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format_unit(bytes, GIB, "GiB")
    } else if bytes >= MIB {
        format_unit(bytes, MIB, "MiB")
    } else if bytes >= KIB {
        format_unit(bytes, KIB, "KiB")
    } else {
        format!("{bytes} B")
    }
}

pub(super) fn format_unit(bytes: u64, unit: u64, suffix: &str) -> String {
    let whole = bytes / unit;
    let decimal = (bytes % unit) * 10 / unit;
    format!("{whole}.{decimal} {suffix}")
}
