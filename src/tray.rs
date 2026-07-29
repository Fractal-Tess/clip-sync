use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
    thread,
};

use fs2::FileExt as _;
use ksni::TrayMethods as _;
use tokio::sync::mpsc;

use crate::config::AppPaths;

struct TrayInstance {
    _lock: File,
}

impl TrayInstance {
    fn acquire(runtime_dir: &Path) -> Result<Option<Self>, String> {
        std::fs::create_dir_all(runtime_dir).map_err(|error| {
            format!(
                "could not create tray runtime directory {}: {error}",
                runtime_dir.display()
            )
        })?;
        std::fs::set_permissions(runtime_dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not secure {}: {error}", runtime_dir.display()))?;
        let lock_path = runtime_dir.join("tray.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|error| format!("could not open {}: {error}", lock_path.display()))?;
        lock.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("could not secure {}: {error}", lock_path.display()))?;
        match lock.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { _lock: lock })),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(format!(
                "could not lock tray instance {}: {error}",
                lock_path.display()
            )),
        }
    }
}

#[derive(Clone)]
struct UiLauncher {
    executable: PathBuf,
    config: PathBuf,
}

impl UiLauncher {
    fn launch(&self, target: &'static str) {
        let child = Command::new(&self.executable)
            .arg("--config")
            .arg(&self.config)
            .args(["ui", target])
            .spawn();
        match child {
            Ok(mut child) => {
                thread::spawn(move || {
                    if let Err(error) = child.wait() {
                        tracing::warn!(%error, target, "tray-launched UI process could not be reaped");
                    }
                });
            }
            Err(error) => {
                tracing::warn!(%error, target, "tray could not launch UI process");
            }
        }
    }
}

struct ClipSyncTray {
    launcher: UiLauncher,
    quit: mpsc::UnboundedSender<()>,
}

impl ksni::Tray for ClipSyncTray {
    fn id(&self) -> String {
        "clip-sync".to_owned()
    }

    fn title(&self) -> String {
        "clip-sync clipboard mesh".to_owned()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        static ICONS: LazyLock<Vec<ksni::Icon>> = LazyLock::new(|| {
            [
                include_bytes!("../assets/icon-16.png").as_slice(),
                include_bytes!("../assets/icon-22.png").as_slice(),
                include_bytes!("../assets/icon-32.png").as_slice(),
                include_bytes!("../assets/icon-64.png").as_slice(),
            ]
            .into_iter()
            .map(tray_icon_from_png)
            .collect()
        });
        ICONS.clone()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.launcher.launch("switcher");
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{MenuItem, StandardItem};

        vec![
            StandardItem {
                label: "History Switcher".to_owned(),
                activate: Box::new(|tray: &mut Self| tray.launcher.launch("switcher")),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Control Center".to_owned(),
                activate: Box::new(|tray: &mut Self| tray.launcher.launch("control")),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit Tray".to_owned(),
                icon_name: "application-exit".to_owned(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.quit.send(());
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn tray_icon_from_png(bytes: &[u8]) -> ksni::Icon {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .expect("embedded tray icon must be valid PNG")
        .into_rgba8();
    let width = i32::try_from(image.width()).expect("embedded tray icon width fits i32");
    let height = i32::try_from(image.height()).expect("embedded tray icon height fits i32");
    let mut data = image.into_raw();
    for pixel in data.chunks_exact_mut(4) {
        pixel.rotate_right(1);
    }
    ksni::Icon {
        width,
        height,
        data,
    }
}

/// Runs the singleton `StatusNotifier` tray process until it receives a quit or termination signal.
///
/// # Errors
///
/// Returns an error when the singleton lock, executable discovery, D-Bus registration, or signal
/// handling fails.
pub async fn run(paths: AppPaths) -> Result<(), String> {
    let Some(_instance) = TrayInstance::acquire(&paths.runtime_dir)? else {
        return Ok(());
    };
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not resolve the clip-sync executable: {error}"))?;
    let (quit_tx, mut quit_rx) = mpsc::unbounded_channel();
    let tray = ClipSyncTray {
        launcher: UiLauncher {
            executable,
            config: paths.config,
        },
        quit: quit_tx,
    };
    let _handle = tray
        .spawn()
        .await
        .map_err(|error| format!("could not register clip-sync tray item: {error}"))?;

    tokio::select! {
        _ = quit_rx.recv() => {}
        signal = tokio::signal::ctrl_c() => {
            signal.map_err(|error| format!("could not wait for tray shutdown signal: {error}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_icons_are_bounded_argb_pixmaps() {
        let icon = tray_icon_from_png(include_bytes!("../assets/icon-22.png"));
        assert_eq!((icon.width, icon.height), (22, 22));
        assert_eq!(icon.data.len(), 22 * 22 * 4);
    }

    #[test]
    fn tray_instance_is_singleton_and_private() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let first = TrayInstance::acquire(temporary.path())
            .expect("first acquisition")
            .expect("first process owns tray");
        let second = TrayInstance::acquire(temporary.path()).expect("second acquisition");

        assert!(second.is_none());
        assert_eq!(
            std::fs::metadata(temporary.path().join("tray.lock"))
                .expect("tray lock metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(first);
    }
}
