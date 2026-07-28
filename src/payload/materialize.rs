use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use thiserror::Error;
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use super::{
    ChunkStore, ChunkStoreError, FileSnapshot, FileSnapshotError, ManifestId, SnapshotEntryKind,
    StoredManifest,
};

const DEFAULT_FREE_SPACE_RESERVE: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaterializerConfig {
    /// Bytes that must remain free after materialization completes.
    pub free_space_reserve_bytes: u64,
}

impl Default for MaterializerConfig {
    fn default() -> Self {
        Self {
            free_space_reserve_bytes: DEFAULT_FREE_SPACE_RESERVE,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Materialization {
    manifest_id: ManifestId,
    directory: PathBuf,
    uri_list: Vec<u8>,
}

impl Materialization {
    #[must_use]
    pub const fn manifest_id(&self) -> ManifestId {
        self.manifest_id
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Exact `text/uri-list` bytes for the materialized snapshot roots.
    #[must_use]
    pub fn uri_list(&self) -> &[u8] {
        &self.uri_list
    }
}

/// Reconstructs authenticated snapshots only beneath a private runtime root.
pub struct Materializer {
    root: PathBuf,
    config: MaterializerConfig,
}

impl Materializer {
    /// Creates a private runtime materialization root.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created or secured.
    pub fn new(
        root: impl AsRef<Path>,
        config: MaterializerConfig,
    ) -> Result<Self, MaterializationError> {
        let root = root.as_ref().to_path_buf();
        create_private_dir(&root)?;
        Ok(Self { root, config })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Authenticates and atomically materializes a stored file snapshot.
    ///
    /// A preflight free-space check accounts for the logical file bytes and a
    /// configurable reserve. No payload-sized allocation is performed.
    ///
    /// # Errors
    ///
    /// Returns a clear free-space error, or an error for wrong manifest kind,
    /// cancellation, corruption, unsafe paths, or I/O.
    pub fn materialize(
        &self,
        store: &ChunkStore,
        manifest_id: ManifestId,
        cancellation: &CancellationToken,
    ) -> Result<Materialization, MaterializationError> {
        ensure_not_cancelled(cancellation)?;
        let StoredManifest::Files(snapshot) = store.manifest(manifest_id)? else {
            return Err(MaterializationError::NotFileSnapshot);
        };
        snapshot.validate(u64::MAX)?;

        let required = snapshot
            .logical_size()
            .checked_add(self.config.free_space_reserve_bytes)
            .ok_or(MaterializationError::SizeOverflow)?;
        let available = fs2::available_space(&self.root)?;
        if available < required {
            return Err(MaterializationError::InsufficientSpace {
                required,
                available,
            });
        }

        let destination = self.root.join(manifest_id.to_string());
        match fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                validate_existing_snapshot(&destination, &snapshot)?;
                return build_materialization(manifest_id, destination, &snapshot);
            }
            Ok(_) => return Err(MaterializationError::UnsafeRuntimeEntry),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let staging = self
            .root
            .join(format!(".{}.{}.staging", manifest_id, Uuid::new_v4()));
        create_private_dir(&staging)?;
        let result = Self::write_snapshot(store, &snapshot, &staging, cancellation);
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        match fs::rename(&staging, &destination) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                fs::remove_dir_all(&staging)?;
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error.into());
            }
        }
        build_materialization(manifest_id, destination, &snapshot)
    }

    /// Removes one materialization. A symlink at the target is unlinked rather
    /// than traversed.
    ///
    /// # Errors
    ///
    /// Returns an error for unexpected file types or filesystem failures.
    pub fn cleanup(&self, manifest_id: ManifestId) -> Result<bool, MaterializationError> {
        let path = self.root.join(manifest_id.to_string());
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || metadata.is_file() {
            fs::remove_file(path)?;
        } else if metadata.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            return Err(MaterializationError::UnsafeRuntimeEntry);
        }
        Ok(true)
    }

    /// Waits for a clipboard-ownership grace period and then removes an item.
    ///
    /// # Errors
    ///
    /// Returns an error if cleanup fails. Cancellation skips the cleanup so a
    /// newly reactivated item is not removed by an obsolete timer.
    pub async fn cleanup_after_grace(
        &self,
        manifest_id: ManifestId,
        grace: Duration,
        cancellation: CancellationToken,
    ) -> Result<bool, MaterializationError> {
        tokio::select! {
            () = tokio::time::sleep(grace) => self.cleanup(manifest_id),
            () = cancellation.cancelled() => Ok(false),
        }
    }

    /// Removes abandoned `.staging` directories after an interrupted process.
    ///
    /// # Errors
    ///
    /// Returns an error for filesystem failures or an unexpected non-directory
    /// staging entry.
    pub fn cleanup_staging(&self) -> Result<usize, MaterializationError> {
        let mut removed = 0;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with('.') || !name.ends_with(".staging") {
                continue;
            }
            let metadata = entry.file_type()?;
            if metadata.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else if metadata.is_file() || metadata.is_symlink() {
                fs::remove_file(entry.path())?;
            } else {
                return Err(MaterializationError::UnsafeRuntimeEntry);
            }
            removed += 1;
        }
        Ok(removed)
    }

    fn write_snapshot(
        store: &ChunkStore,
        snapshot: &FileSnapshot,
        staging: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), MaterializationError> {
        for entry in snapshot.entries() {
            ensure_not_cancelled(cancellation)?;
            let path = safe_join(staging, entry.relative_path())?;
            match entry.kind() {
                SnapshotEntryKind::Directory => create_private_dir(&path)?,
                SnapshotEntryKind::File => {
                    let parent = path
                        .parent()
                        .ok_or(MaterializationError::UnsafeManifestPath)?;
                    if !parent.is_dir() {
                        return Err(MaterializationError::UnsafeManifestPath);
                    }
                    let mut file = create_private_file(&path)?;
                    let blob = entry
                        .blob()
                        .ok_or(MaterializationError::UnsafeManifestPath)?;
                    store.read_blob(blob, &mut file, cancellation)?;
                    file.flush()?;
                    file.sync_all()?;
                    set_file_permissions(&path, entry.executable())?;
                }
            }
        }
        Ok(())
    }
}

fn build_materialization(
    manifest_id: ManifestId,
    directory: PathBuf,
    snapshot: &FileSnapshot,
) -> Result<Materialization, MaterializationError> {
    let mut uri_list = Vec::new();
    for entry in snapshot
        .entries()
        .iter()
        .filter(|entry| !entry.relative_path().contains('/'))
    {
        let path = safe_join(&directory, entry.relative_path())?;
        let url = if entry.kind() == SnapshotEntryKind::Directory {
            Url::from_directory_path(path)
        } else {
            Url::from_file_path(path)
        }
        .map_err(|()| MaterializationError::InvalidRuntimePath)?;
        uri_list.extend_from_slice(url.as_str().as_bytes());
        uri_list.extend_from_slice(b"\r\n");
    }
    if uri_list.is_empty() {
        return Err(MaterializationError::UnsafeManifestPath);
    }
    Ok(Materialization {
        manifest_id,
        directory,
        uri_list,
    })
}

fn validate_existing_snapshot(
    directory: &Path,
    snapshot: &FileSnapshot,
) -> Result<(), MaterializationError> {
    let mut pending = vec![directory.to_path_buf()];
    let mut observed = 0_usize;
    while let Some(parent) = pending.pop() {
        for child in fs::read_dir(parent)? {
            let child = child?;
            let file_type = child.file_type()?;
            if file_type.is_symlink() || !file_type.is_file() && !file_type.is_dir() {
                return Err(MaterializationError::UnsafeRuntimeEntry);
            }
            observed = observed
                .checked_add(1)
                .ok_or(MaterializationError::SizeOverflow)?;
            if observed > snapshot.entries().len() {
                return Err(MaterializationError::UnsafeRuntimeEntry);
            }
            if file_type.is_dir() {
                pending.push(child.path());
            }
        }
    }
    if observed != snapshot.entries().len() {
        return Err(MaterializationError::UnsafeRuntimeEntry);
    }
    for entry in snapshot.entries() {
        let metadata = fs::symlink_metadata(safe_join(directory, entry.relative_path())?)?;
        if metadata.file_type().is_symlink()
            || entry.kind() == SnapshotEntryKind::Directory && !metadata.is_dir()
            || entry.kind() == SnapshotEntryKind::File
                && (!metadata.is_file() || metadata.len() != entry.logical_size())
        {
            return Err(MaterializationError::UnsafeRuntimeEntry);
        }
    }
    Ok(())
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, MaterializationError> {
    if relative.is_empty() || relative.starts_with('/') || relative.contains('\0') {
        return Err(MaterializationError::UnsafeManifestPath);
    }
    let mut result = root.to_path_buf();
    for component in relative.split('/') {
        if component.is_empty() || matches!(component, "." | "..") {
            return Err(MaterializationError::UnsafeManifestPath);
        }
        result.push(component);
    }
    Ok(result)
}

fn ensure_not_cancelled(cancellation: &CancellationToken) -> Result<(), MaterializationError> {
    if cancellation.is_cancelled() {
        Err(MaterializationError::Cancelled)
    } else {
        Ok(())
    }
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(unix)]
fn set_file_permissions(path: &Path, executable: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path, _executable: bool) -> io::Result<()> {
    Ok(())
}

#[derive(Debug, Error)]
pub enum MaterializationError {
    #[error("manifest does not describe a file snapshot")]
    NotFileSnapshot,
    #[error(
        "runtime storage has {available} bytes free but materialization requires {required} bytes"
    )]
    InsufficientSpace { required: u64, available: u64 },
    #[error("materialization size overflow")]
    SizeOverflow,
    #[error("unsafe relative path in snapshot manifest")]
    UnsafeManifestPath,
    #[error("runtime path cannot be represented as a file URI")]
    InvalidRuntimePath,
    #[error("unexpected runtime entry type")]
    UnsafeRuntimeEntry,
    #[error("materialization was cancelled")]
    Cancelled,
    #[error(transparent)]
    ChunkStore(#[from] ChunkStoreError),
    #[error(transparent)]
    Snapshot(#[from] FileSnapshotError),
    #[error(transparent)]
    Io(#[from] io::Error),
}
