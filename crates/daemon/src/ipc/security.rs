#[cfg(target_os = "linux")]
use std::os::fd::AsFd;
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

use fs2::FileExt;
use tokio::net::UnixStream;

use super::IpcError;

pub struct DaemonInstance {
    _lock: File,
}

impl DaemonInstance {
    /// Acquires the per-user daemon startup lock.
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::AlreadyRunning`] when another daemon startup or
    /// process owns the lock, and an I/O error when the lock cannot be secured.
    pub fn acquire(runtime_dir: &Path) -> Result<Self, IpcError> {
        std::fs::create_dir_all(runtime_dir)?;
        make_socket_parent_private(runtime_dir)?;
        let lock_path = runtime_dir.join("daemon.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        set_lock_permissions(&lock_path)?;
        match lock.try_lock_exclusive() {
            Ok(()) => Ok(Self { _lock: lock }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Err(IpcError::AlreadyRunning)
            }
            Err(error) => Err(error.into()),
        }
    }
}

pub(super) async fn remove_socket(socket: &Path) -> Result<(), IpcError> {
    match tokio::fs::symlink_metadata(socket).await {
        Ok(metadata) if is_socket(&metadata) => {}
        Ok(_) => return Err(IpcError::SocketPathNotSocket(socket.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    match tokio::fs::remove_file(socket).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) async fn prepare_socket(socket: &Path) -> Result<(), IpcError> {
    let parent = socket.parent().ok_or(IpcError::MissingSocketParent)?;
    tokio::fs::create_dir_all(parent).await?;
    make_socket_parent_private(parent)?;
    match std::fs::symlink_metadata(socket) {
        Ok(_) if !is_socket_path(socket)? => {
            return Err(IpcError::SocketPathNotSocket(socket.to_owned()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    match UnixStream::connect(socket).await {
        Ok(_) => Err(IpcError::AlreadyRunning),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            remove_stale_socket(socket).await
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) async fn remove_stale_socket(socket: &Path) -> Result<(), IpcError> {
    match tokio::fs::symlink_metadata(socket).await {
        Ok(metadata) if is_socket(&metadata) => remove_socket(socket).await,
        Ok(_) => Err(IpcError::SocketPathNotSocket(socket.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
pub(super) fn is_socket(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;

    metadata.file_type().is_socket()
}

#[cfg(unix)]
pub(super) fn is_socket_path(path: &Path) -> Result<bool, IpcError> {
    use std::os::unix::fs::FileTypeExt;

    Ok(std::fs::symlink_metadata(path)?.file_type().is_socket())
}

#[cfg(not(unix))]
pub(super) fn is_socket_path(_path: &Path) -> Result<bool, IpcError> {
    Ok(true)
}

#[cfg(unix)]
pub(super) fn make_socket_parent_private(parent: &Path) -> Result<(), IpcError> {
    use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, open};

    let fd = open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let stat = fstat(&fd).map_err(std::io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_dir()
        || stat.st_uid != rustix::process::getuid().as_raw()
    {
        return Err(IpcError::UnsafeSocketParent);
    }
    fchmod(&fd, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(std::io::Error::from)?;
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn make_socket_parent_private(_parent: &Path) -> Result<(), IpcError> {
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn peer_is_current_user(stream: &UnixStream) -> Result<bool, IpcError> {
    let credentials =
        rustix::net::sockopt::socket_peercred(stream.as_fd()).map_err(std::io::Error::from)?;
    Ok(credentials.uid == rustix::process::getuid())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn peer_is_current_user(_stream: &UnixStream) -> Result<bool, IpcError> {
    Ok(true)
}

#[cfg(unix)]
pub(super) fn set_socket_permissions(socket: &Path) -> Result<(), IpcError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(unix)]
pub(super) fn set_lock_permissions(lock: &Path) -> Result<(), IpcError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(lock, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
