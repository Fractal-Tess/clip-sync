use std::sync::Arc;

use eframe::egui::{self, Vec2};

use crate::config::AppPaths;

mod app;
mod global_shortcut;
mod history;
mod ipc_types;
mod ipc_worker;
mod singleton;
mod style;
mod window;

use app::ClipSyncApp;
pub use singleton::close_quick;
use singleton::{UiInstance, UiSignal};
use style::{APP_ID, WINDOW_TITLE, configure_style, decode_brand_icon};
use window::{
    geometry_coordinate_to_f32, geometry_position_to_f32, load_or_migrate_window_geometry,
    prepare_private_directory, restore_hyprland_geometry, window_state_path,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    Switcher,
    Control,
}

impl UiMode {
    const fn signal(self) -> UiSignal {
        match self {
            Self::Switcher => UiSignal::OpenQuick,
            Self::Control => UiSignal::OpenManagement,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presentation {
    Quick,
    Management,
}

impl Presentation {
    const fn from_mode(mode: UiMode) -> Self {
        match mode {
            UiMode::Switcher => Self::Quick,
            UiMode::Control => Self::Management,
        }
    }

    const fn activation_closes(self) -> bool {
        matches!(self, Self::Quick)
    }
}

const fn signal_closes_presentation(presentation: Presentation, signal: UiSignal) -> bool {
    matches!(
        (presentation, signal),
        (Presentation::Quick, UiSignal::CloseQuick)
    )
}
/// Starts the optional native egui process using the caller's resolved XDG paths.
///
/// # Errors
///
/// Returns an error when the native event loop or graphics context cannot start.
pub fn run(mode: UiMode, paths: AppPaths) -> Result<(), String> {
    let Some(instance) = UiInstance::acquire(&paths.runtime_dir, mode.signal())? else {
        return Ok(());
    };
    prepare_private_directory(&paths.state_dir, "UI state")?;
    let window_state_path = window_state_path(&paths.state_dir);
    let saved_geometry = load_or_migrate_window_geometry(&paths.state_dir)?;
    let restored_size = saved_geometry.map_or(Vec2::new(720.0, 480.0), |geometry| {
        Vec2::new(
            geometry_coordinate_to_f32(geometry.width),
            geometry_coordinate_to_f32(geometry.height),
        )
    });
    let native_icon = decode_brand_icon()?;
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(WINDOW_TITLE)
        .with_app_id(APP_ID)
        .with_icon(Arc::new(native_icon))
        .with_inner_size(restored_size)
        .with_min_inner_size(Vec2::new(480.0, 300.0))
        .with_decorations(false);
    if let Some(geometry) = saved_geometry
        && let (Some(x), Some(y)) = (geometry.x, geometry.y)
    {
        viewport =
            viewport.with_position([geometry_position_to_f32(x), geometry_position_to_f32(y)]);
        restore_hyprland_geometry(APP_ID, geometry);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(move |context| {
            configure_style(&context.egui_ctx);
            let app = ClipSyncApp::new(
                mode,
                paths,
                context.egui_ctx.clone(),
                instance,
                APP_ID,
                window_state_path,
                saved_geometry,
            )
            .map_err(std::io::Error::other)?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests;
