const COMMANDS: &[&str] = &[
    "get_status",
    "get_history",
    "get_image_preview",
    "update_history",
    "get_peers",
    "get_settings",
    "get_diagnostics",
    "get_transfers",
    "cancel_transfer",
    "forget_device",
    "update_shared_setting",
    "activate_history",
];

fn main() {
    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS));
    tauri_build::try_build(attributes).expect("failed to prepare ClipSync Tauri permissions");
}
