use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};

use fs2::FileExt;

use super::{EnvelopeError, Result, STORE_LOCK_FILENAME};

/// Process-lifetime exclusive owner of the local daemon/store state.
pub struct StoreLock {
    file: File,
    state_dir: PathBuf,
}

impl StoreLock {
    /// Creates and exclusively locks the owner-only state lock without waiting.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError::StoreBusy`] when another process owns the lock,
    /// or an I/O/security error for an unsafe state path.
    pub fn acquire(state_dir: impl AsRef<Path>) -> Result<Self> {
        let state_dir = state_dir.as_ref();
        create_private_directory(state_dir)?;
        let path = state_dir.join(STORE_LOCK_FILENAME);
        let file = open_lock_file(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self {
                file,
                state_dir: state_dir.to_path_buf(),
            }),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                Err(EnvelopeError::StoreBusy)
            }
            Err(error) => Err(error.into()),
        }
    }

    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(unix)]
pub(super) fn open_private_keyslot(path: &Path) -> Result<File> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, open};

    let fd = open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let stat = fstat(&fd).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_uid != rustix::process::getuid().as_raw()
        || stat.st_mode & 0o777 != 0o600
    {
        return Err(EnvelopeError::UnsafeKeyslot);
    }
    Ok(File::from(fd))
}

#[cfg(not(unix))]
pub(super) fn open_private_keyslot(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(EnvelopeError::UnsafeKeyslot);
    }
    Ok(File::open(path)?)
}

#[cfg(unix)]
fn open_lock_file(path: &Path) -> Result<File> {
    use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, open};

    let fd = open(
        path,
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(io::Error::from)?;
    let stat = fstat(&fd).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_uid != rustix::process::getuid().as_raw()
    {
        return Err(EnvelopeError::UnsafeLock);
    }
    fchmod(&fd, Mode::RUSR | Mode::WUSR).map_err(io::Error::from)?;
    Ok(File::from(fd))
}

#[cfg(not(unix))]
fn open_lock_file(path: &Path) -> Result<File> {
    Ok(fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)?)
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> Result<()> {
    use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, open};

    fs::create_dir_all(path)?;
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let stat = fstat(&fd).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_dir()
        || stat.st_uid != rustix::process::getuid().as_raw()
    {
        return Err(EnvelopeError::UnsafeLock);
    }
    fchmod(&fd, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(io::Error::from)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

pub(super) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

pub(super) fn path_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) fn cleanup_orphan_keyslot_temps(state_dir: &Path) -> Result<usize> {
    let mut removed = 0_usize;
    for entry in fs::read_dir(state_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(".history.keyslot.")
            || !Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"))
        {
            continue;
        }
        let file_type = entry.file_type()?;
        if !file_type.is_file() && !file_type.is_symlink() {
            return Err(EnvelopeError::InvalidKeyslot);
        }
        fs::remove_file(entry.path())?;
        removed = removed
            .checked_add(1)
            .ok_or(EnvelopeError::InvalidKeyslot)?;
    }
    Ok(removed)
}
