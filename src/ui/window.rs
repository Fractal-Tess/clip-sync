use std::{
    fs::OpenOptions,
    io::Write as _,
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::ui::singleton::set_private_mode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct WindowGeometry {
    pub(super) x: Option<i32>,
    pub(super) y: Option<i32>,
    pub(super) width: u32,
    pub(super) height: u32,
}

impl WindowGeometry {
    pub(super) fn is_valid(self) -> bool {
        let valid_position = match (self.x, self.y) {
            (Some(x), Some(y)) => x.abs() <= 100_000 && y.abs() <= 100_000,
            (None, None) => true,
            _ => false,
        };
        valid_position
            && (480..=16_384).contains(&self.width)
            && (300..=16_384).contains(&self.height)
    }
}

#[derive(Deserialize)]
pub(super) struct HyprlandClient {
    pub(super) address: String,
    pub(super) class: String,
    pub(super) at: [i32; 2],
    pub(super) size: [i32; 2],
}
pub(super) fn window_state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("window.json")
}

pub(super) fn legacy_window_state_paths(state_dir: &Path) -> [PathBuf; 2] {
    [
        state_dir.join("switcher-window.json"),
        state_dir.join("control-window.json"),
    ]
}

pub(super) fn load_or_migrate_window_geometry(
    state_dir: &Path,
) -> Result<Option<WindowGeometry>, String> {
    let canonical = window_state_path(state_dir);
    let legacy = legacy_window_state_paths(state_dir);
    if let Some(geometry) = load_window_geometry(&canonical) {
        for path in &legacy {
            remove_legacy_geometry_file(path)?;
        }
        return Ok(Some(geometry));
    }

    let migrated = legacy.iter().find_map(|path| load_window_geometry(path));
    if let Some(geometry) = migrated {
        save_window_geometry(&canonical, geometry)?;
    }
    for path in &legacy {
        remove_legacy_geometry_file(path)?;
    }
    Ok(migrated)
}

pub(super) fn remove_legacy_geometry_file(path: &Path) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("could not inspect {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "refusing unsafe legacy UI geometry path {}",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        if metadata.uid() != rustix::process::getuid().as_raw() {
            return Err(format!(
                "refusing legacy UI geometry not owned by this user: {}",
                path.display()
            ));
        }
    }
    std::fs::remove_file(path)
        .map_err(|error| format!("could not remove {}: {error}", path.display()))
}

pub(super) fn prepare_private_directory(path: &Path, label: &str) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|error| {
        format!(
            "could not create {label} directory {}: {error}",
            path.display()
        )
    })?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "refusing unsafe {label} directory {}",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        if metadata.uid() != rustix::process::getuid().as_raw() {
            return Err(format!(
                "refusing {label} directory not owned by this user: {}",
                path.display()
            ));
        }
    }
    set_private_mode(path, 0o700)
}

pub(super) fn load_window_geometry(path: &Path) -> Option<WindowGeometry> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    let geometry = serde_json::from_slice::<WindowGeometry>(&std::fs::read(path).ok()?).ok()?;
    geometry.is_valid().then_some(geometry)
}

pub(super) fn save_window_geometry(path: &Path, geometry: WindowGeometry) -> Result<(), String> {
    if !geometry.is_valid() {
        return Err("refusing to persist invalid window geometry".to_owned());
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("window state path has no parent: {}", path.display()))?;
    prepare_private_directory(parent, "UI state")?;
    let temporary = parent.join(format!(
        ".window-state-{}-{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| format!("could not create {}: {error}", temporary.display()))?;
        let encoded = serde_json::to_vec(&geometry)
            .map_err(|error| format!("could not encode window geometry: {error}"))?;
        file.write_all(&encoded)
            .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| format!("could not sync {}: {error}", temporary.display()))?;
        std::fs::rename(&temporary, path).map_err(|error| {
            format!("could not replace window state {}: {error}", path.display())
        })?;
        set_private_mode(path, 0o600)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub(super) fn query_hyprland_geometry(app_id: &str) -> Option<WindowGeometry> {
    let output = Command::new("hyprctl")
        .args(["-j", "clients"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let clients = serde_json::from_slice::<Vec<HyprlandClient>>(&output.stdout).ok()?;
    let client = clients.into_iter().find(|client| client.class == app_id)?;
    let width = u32::try_from(client.size[0]).ok()?;
    let height = u32::try_from(client.size[1]).ok()?;
    let geometry = WindowGeometry {
        x: Some(client.at[0]),
        y: Some(client.at[1]),
        width,
        height,
    };
    geometry.is_valid().then_some(geometry)
}

pub(super) fn restore_hyprland_geometry(app_id: &'static str, geometry: WindowGeometry) {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return;
    }
    thread::spawn(move || {
        for _ in 0..20 {
            let Some(client) = query_hyprland_client(app_id) else {
                thread::sleep(Duration::from_millis(50));
                continue;
            };
            let selector = format!("address:{}", client.address);
            let resize = format!("exact {} {},{selector}", geometry.width, geometry.height);
            let _ = Command::new("hyprctl")
                .args(["dispatch", "resizewindowpixel", &resize])
                .output();
            if let (Some(x), Some(y)) = (geometry.x, geometry.y) {
                let movement = format!("exact {x} {y},{selector}");
                let _ = Command::new("hyprctl")
                    .args(["dispatch", "movewindowpixel", &movement])
                    .output();
            }
            return;
        }
    });
}

pub(super) fn query_hyprland_client(app_id: &str) -> Option<HyprlandClient> {
    let output = Command::new("hyprctl")
        .args(["-j", "clients"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice::<Vec<HyprlandClient>>(&output.stdout)
        .ok()?
        .into_iter()
        .find(|client| client.class == app_id)
}

pub(super) fn context_window_geometry(
    context: &egui::Context,
    previous: Option<WindowGeometry>,
) -> Option<WindowGeometry> {
    if let Some(rect) = context.input(|input| input.viewport().outer_rect)
        && rect.is_finite()
        && let (Some(x), Some(y), Some(width), Some(height)) = (
            rounded_geometry_position(rect.min.x),
            rounded_geometry_position(rect.min.y),
            rounded_geometry_coordinate(rect.width()),
            rounded_geometry_coordinate(rect.height()),
        )
    {
        let geometry = WindowGeometry {
            x: Some(x),
            y: Some(y),
            width,
            height,
        };
        if geometry.is_valid() {
            return Some(geometry);
        }
    }

    let size = context.content_rect().size();
    let geometry = WindowGeometry {
        x: previous.and_then(|geometry| geometry.x),
        y: previous.and_then(|geometry| geometry.y),
        width: rounded_geometry_coordinate(size.x)?,
        height: rounded_geometry_coordinate(size.y)?,
    };
    geometry.is_valid().then_some(geometry)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "validated geometry coordinates are far below f32's exact integer limit"
)]
pub(super) fn geometry_coordinate_to_f32(value: u32) -> f32 {
    value as f32
}

#[allow(
    clippy::cast_precision_loss,
    reason = "validated window positions are far below f32's exact integer limit"
)]
pub(super) fn geometry_position_to_f32(value: i32) -> f32 {
    value as f32
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "the finite value is range-checked before rounding"
)]
pub(super) fn rounded_geometry_position(value: f32) -> Option<i32> {
    (value.is_finite() && value.abs() <= 100_000.0).then(|| value.round() as i32)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the finite positive value is range-checked before rounding"
)]
pub(super) fn rounded_geometry_coordinate(value: f32) -> Option<u32> {
    (value.is_finite() && (0.0..=16_384.0).contains(&value)).then(|| value.round() as u32)
}
