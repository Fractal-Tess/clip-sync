use std::path::PathBuf;

#[cfg(debug_assertions)]
use specta_typescript::{BigIntExportBehavior, Typescript};
#[cfg(debug_assertions)]
use std::path::Path;
use tauri::{Manager, WindowEvent};
use tauri_plugin_window_state::{AppHandleExt, StateFlags};
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
pub fn run(config_override: Option<PathBuf>, start_hidden: bool) {
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
        .plugin(tauri_plugin_single_instance::init(
            |app, _arguments, _directory| {
                show_main_window(app);
            },
        ))
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(window_state_flags())
                .build(),
        )
        .manage(AppState::discover(config_override))
        .invoke_handler(specta.invoke_handler())
        .setup(move |app| {
            if !start_hidden {
                show_main_window(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "main"
                && let WindowEvent::CloseRequested { api, .. } = event
            {
                api.prevent_close();
                let _ = window.app_handle().save_window_state(window_state_flags());
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running ClipSync");
}

fn window_state_flags() -> StateFlags {
    StateFlags::POSITION | StateFlags::SIZE
}

fn show_main_window(app: &impl Manager<tauri::Wry>) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}
