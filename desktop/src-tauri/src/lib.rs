use std::path::PathBuf;

#[cfg(debug_assertions)]
use specta_typescript::{BigIntExportBehavior, Typescript};
#[cfg(debug_assertions)]
use std::path::Path;
use tauri_specta::{Builder as SpectaBuilder, collect_commands};

#[macro_use]
mod commands;
mod image_preview;
mod state;
mod views;

use commands::{
    activate_history, cancel_transfer, forget_device, get_diagnostics, get_history,
    get_image_preview, get_peers, get_settings, get_status, get_transfers, update_history,
    update_peer_interfaces, update_shared_setting,
};
use state::AppState;

/// Launches the Tauri desktop control window.
///
/// # Panics
///
/// Panics when generated bindings cannot be written in a debug build or when
/// Tauri cannot initialize or run the application.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run(config_override: Option<PathBuf>) {
    let specta = SpectaBuilder::<tauri::Wry>::new().commands(collect_commands![
        get_status,
        get_history,
        get_image_preview,
        update_history,
        get_peers,
        get_settings,
        get_diagnostics,
        get_transfers,
        cancel_transfer,
        forget_device,
        update_shared_setting,
        update_peer_interfaces,
        activate_history
    ]);

    #[cfg(debug_assertions)]
    specta
        .export(
            Typescript::default().bigint(BigIntExportBehavior::Number),
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/lib/bindings.ts"),
        )
        .expect("failed to export Tauri Specta bindings");

    tauri::Builder::default()
        .manage(AppState::discover(config_override))
        .invoke_handler(specta.invoke_handler())
        .run(tauri::generate_context!())
        .expect("error while running ClipSync");
}
