use std::{
    fs::File,
    io::{Read as _, Write as _},
    os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

use eframe::egui;
use fs2::FileExt as _;

use crate::{
    config::AppPaths,
    ui::{style::MAX_UI_SIGNAL_BYTES, window::prepare_private_directory},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UiSignal {
    OpenQuick,
    OpenManagement,
    CloseQuick,
}

impl UiSignal {
    pub(super) const fn encode(self) -> &'static [u8] {
        match self {
            Self::OpenQuick => b"open-quick\n",
            Self::OpenManagement => b"open-management\n",
            Self::CloseQuick => b"close-quick\n",
        }
    }

    pub(super) fn parse(bytes: &[u8]) -> Option<Self> {
        match bytes {
            b"open-quick\n" => Some(Self::OpenQuick),
            b"open-management\n" => Some(Self::OpenManagement),
            b"close-quick\n" => Some(Self::CloseQuick),
            _ => None,
        }
    }

    pub(super) const fn requests_focus(self) -> bool {
        !matches!(self, Self::CloseQuick)
    }
}

pub(super) struct UiInstance {
    _lock: File,
    signal_socket: PathBuf,
    listener: Option<StdUnixListener>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl UiInstance {
    pub(super) fn acquire(runtime_dir: &Path, signal: UiSignal) -> Result<Option<Self>, String> {
        prepare_private_directory(runtime_dir, "UI runtime")?;

        let lock_path = runtime_dir.join("switcher.lock");
        let signal_socket = runtime_dir.join("switcher.sock");
        let lock_fd = rustix::fs::open(
            &lock_path,
            rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(|error| format!("could not open {}: {error}", lock_path.display()))?;
        let lock = File::from(lock_fd);
        set_private_mode(&lock_path, 0o600)?;

        match lock.try_lock_exclusive() {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                request_existing_signal(&signal_socket, signal, false)?;
                return Ok(None);
            }
            Err(error) => {
                return Err(format!(
                    "could not lock UI instance file {}: {error}",
                    lock_path.display()
                ));
            }
        }

        remove_stale_ui_socket(&signal_socket)?;
        let listener = StdUnixListener::bind(&signal_socket).map_err(|error| {
            format!(
                "could not bind UI signal socket {}: {error}",
                signal_socket.display()
            )
        })?;
        set_private_mode(&signal_socket, 0o600)?;
        Ok(Some(Self {
            _lock: lock,
            signal_socket,
            listener: Some(listener),
            shutdown: Arc::new(AtomicBool::new(false)),
            thread: None,
        }))
    }

    pub(super) fn start_signal_listener(
        &mut self,
        context: egui::Context,
        signal_tx: std_mpsc::Sender<UiSignal>,
    ) -> Result<(), String> {
        let Some(listener) = self.listener.take() else {
            return Ok(());
        };
        let shutdown = Arc::clone(&self.shutdown);
        let thread = std::thread::Builder::new()
            .name("clip-sync-ui-signals".to_owned())
            .spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(_) if shutdown.load(Ordering::Acquire) => break,
                        Ok(stream) => {
                            if !signal_peer_is_current_user(&stream) {
                                continue;
                            }
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                            let mut bytes = Vec::new();
                            if stream
                                .take(MAX_UI_SIGNAL_BYTES + 1)
                                .read_to_end(&mut bytes)
                                .is_ok()
                                && bytes.len() <= usize::try_from(MAX_UI_SIGNAL_BYTES).unwrap_or(32)
                                && let Some(signal) = UiSignal::parse(&bytes)
                            {
                                if signal.requests_focus() {
                                    context.send_viewport_cmd(egui::ViewportCommand::Focus);
                                }
                                if signal_tx.send(signal).is_err() {
                                    break;
                                }
                                context.request_repaint();
                            }
                        }
                        Err(error) => {
                            tracing::debug!(%error, "UI signal listener stopped");
                            break;
                        }
                    }
                }
            })
            .map_err(|error| format!("could not start UI signal listener: {error}"))?;
        self.thread = Some(thread);
        Ok(())
    }
}

impl Drop for UiInstance {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = StdUnixStream::connect(&self.signal_socket);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::debug!("UI signal listener panicked during shutdown");
        }
        match std::fs::remove_file(&self.signal_socket) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::debug!(%error, "could not remove UI signal socket"),
        }
    }
}

pub(super) fn signal_peer_is_current_user(stream: &StdUnixStream) -> bool {
    use std::os::fd::AsFd as _;

    rustix::net::sockopt::socket_peercred(stream.as_fd())
        .is_ok_and(|credentials| credentials.uid == rustix::process::getuid())
}

pub(super) fn request_existing_signal(
    socket: &Path,
    signal: UiSignal,
    absent_is_ok: bool,
) -> Result<(), String> {
    let mut last_error = None;
    for _ in 0..10 {
        match validate_ui_signal_socket(socket) {
            Ok(true) => {}
            Ok(false) if absent_is_ok => return Ok(()),
            Ok(false) => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(error) => return Err(error),
        }
        match StdUnixStream::connect(socket) {
            Ok(mut stream) => {
                stream
                    .write_all(signal.encode())
                    .map_err(|error| format!("could not signal {}: {error}", socket.display()))?;
                return Ok(());
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                if absent_is_ok {
                    return Ok(());
                }
                last_error = Some(error);
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                last_error = Some(error);
                break;
            }
        }
    }
    Err(format!(
        "another clip-sync UI is running, but its signal socket {} is unavailable: {}",
        socket.display(),
        last_error.map_or_else(
            || "signal socket did not appear".to_owned(),
            |error| error.to_string(),
        )
    ))
}

pub(super) fn validate_ui_signal_socket(socket: &Path) -> Result<bool, String> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

    let metadata = match std::fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("could not inspect {}: {error}", socket.display())),
    };
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_socket()
        || metadata.uid() != rustix::process::getuid().as_raw()
    {
        return Err(format!(
            "refusing unsafe clip-sync UI signal path {}",
            socket.display()
        ));
    }
    Ok(true)
}

/// Requests that an existing Quick History window close without starting a UI process.
///
/// The same-user signal is ignored by a management presentation and is a no-op when no UI exists.
///
/// # Errors
///
/// Returns an error for an unsafe runtime path or when a running UI cannot be signalled.
pub fn close_quick(paths: &AppPaths) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;

    if !paths.runtime_dir.exists() {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(&paths.runtime_dir)
        .map_err(|error| format!("could not inspect {}: {error}", paths.runtime_dir.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != rustix::process::getuid().as_raw()
    {
        return Err(format!(
            "refusing unsafe UI runtime directory {}",
            paths.runtime_dir.display()
        ));
    }
    request_existing_signal(
        &paths.runtime_dir.join("switcher.sock"),
        UiSignal::CloseQuick,
        true,
    )
}

pub(super) fn remove_stale_ui_socket(socket: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(socket) {
        Ok(metadata) => {
            use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

            if !metadata.file_type().is_socket()
                || metadata.uid() != rustix::process::getuid().as_raw()
            {
                return Err(format!(
                    "refusing to replace non-socket UI signal path {}",
                    socket.display()
                ));
            }
            std::fs::remove_file(socket).map_err(|error| {
                format!(
                    "could not remove stale UI signal socket {}: {error}",
                    socket.display()
                )
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not inspect UI signal socket {}: {error}",
            socket.display()
        )),
    }
}

#[cfg(unix)]
pub(super) fn set_private_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("could not secure {}: {error}", path.display()))
}
