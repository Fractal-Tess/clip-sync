use std::{
    collections::HashSet,
    fs::{self, File, Metadata},
    io,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{BlobManifest, ChunkStore, ChunkStoreError};

const DEFAULT_MAX_ENTRIES: usize = 100_000;
const DEFAULT_MAX_DEPTH: usize = 64;
const DEFAULT_MAX_URI_LIST_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_LOGICAL_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const MAX_RELATIVE_PATH_BYTES: usize = 4096;
const MAX_PATH_COMPONENT_BYTES: usize = 255;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileSnapshotLimits {
    pub max_entries: usize,
    pub max_depth: usize,
    pub max_uri_list_bytes: usize,
    pub max_logical_bytes: u64,
}

impl Default for FileSnapshotLimits {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            max_depth: DEFAULT_MAX_DEPTH,
            max_uri_list_bytes: DEFAULT_MAX_URI_LIST_BYTES,
            max_logical_bytes: DEFAULT_MAX_LOGICAL_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotEntryKind {
    File,
    Directory,
}

/// A safe relative snapshot entry with intentionally conservative metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSnapshotEntry {
    relative_path: String,
    kind: SnapshotEntryKind,
    executable: bool,
    logical_size: u64,
    blob: Option<BlobManifest>,
}

impl FileSnapshotEntry {
    #[must_use]
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }

    #[must_use]
    pub const fn kind(&self) -> SnapshotEntryKind {
        self.kind
    }

    #[must_use]
    pub const fn executable(&self) -> bool {
        self.executable
    }

    #[must_use]
    pub const fn logical_size(&self) -> u64 {
        self.logical_size
    }

    #[must_use]
    pub const fn blob(&self) -> Option<&BlobManifest> {
        self.blob.as_ref()
    }
}

/// Immutable file/directory snapshot manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSnapshot {
    logical_size: u64,
    entries: Vec<FileSnapshotEntry>,
}

impl FileSnapshot {
    #[must_use]
    pub const fn logical_size(&self) -> u64 {
        self.logical_size
    }

    #[must_use]
    pub fn entries(&self) -> &[FileSnapshotEntry] {
        &self.entries
    }

    pub(crate) fn validate(&self, maximum_bytes: u64) -> Result<(), FileSnapshotError> {
        if self.entries.is_empty() {
            return Err(FileSnapshotError::Empty);
        }
        let mut prior = None;
        let mut total = 0_u64;
        let mut directories = HashSet::new();
        for entry in &self.entries {
            validate_relative_path(&entry.relative_path)?;
            if prior.is_some_and(|prior: &str| prior >= entry.relative_path.as_str()) {
                return Err(FileSnapshotError::NonCanonicalManifest);
            }
            prior = Some(&entry.relative_path);
            if let Some((parent, _)) = entry.relative_path.rsplit_once('/')
                && !directories.contains(parent)
            {
                return Err(FileSnapshotError::NonCanonicalManifest);
            }
            match (entry.kind, &entry.blob) {
                (SnapshotEntryKind::Directory, None) if entry.logical_size == 0 => {
                    directories.insert(entry.relative_path.as_str());
                }
                (SnapshotEntryKind::File, Some(blob))
                    if blob.logical_size() == entry.logical_size =>
                {
                    total = total
                        .checked_add(entry.logical_size)
                        .ok_or(FileSnapshotError::SizeOverflow)?;
                }
                _ => return Err(FileSnapshotError::NonCanonicalManifest),
            }
        }
        if total != self.logical_size {
            return Err(FileSnapshotError::NonCanonicalManifest);
        }
        if total > maximum_bytes {
            return Err(FileSnapshotError::TooLarge {
                observed: total,
                maximum: maximum_bytes,
            });
        }
        Ok(())
    }
}

/// Parses bounded `text/uri-list` or GNOME copied-files bytes into local paths.
///
/// # Errors
///
/// Rejects remote/non-file URIs, malformed UTF-8, relative paths, traversal,
/// NULs, duplicates, excessive input, or an empty list.
pub fn parse_file_uri_list(
    bytes: &[u8],
    limits: FileSnapshotLimits,
) -> Result<Vec<PathBuf>, FileSnapshotError> {
    validate_limits(limits)?;
    if bytes.len() > limits.max_uri_list_bytes {
        return Err(FileSnapshotError::UriListTooLarge {
            observed: bytes.len(),
            maximum: limits.max_uri_list_bytes,
        });
    }
    let source = std::str::from_utf8(bytes).map_err(|_| FileSnapshotError::InvalidUriListUtf8)?;
    let mut paths = Vec::new();
    let mut unique = HashSet::new();
    for raw_line in source.lines() {
        let line = raw_line.trim_end_matches('\r').trim();
        if line.is_empty()
            || line.starts_with('#')
            || matches!(line.to_ascii_lowercase().as_str(), "copy" | "cut")
        {
            continue;
        }
        if paths.len() == limits.max_entries {
            return Err(FileSnapshotError::TooManyEntries {
                maximum: limits.max_entries,
            });
        }
        let url = Url::parse(line).map_err(|_| FileSnapshotError::InvalidFileUri)?;
        if url.scheme() != "file" {
            return Err(FileSnapshotError::UnsupportedUriScheme);
        }
        if url
            .host_str()
            .is_some_and(|host| !host.is_empty() && host != "localhost")
        {
            return Err(FileSnapshotError::RemoteFileUri);
        }
        let path = url
            .to_file_path()
            .map_err(|()| FileSnapshotError::InvalidFileUri)?;
        validate_absolute_source_path(&path)?;
        if !unique.insert(path.clone()) {
            return Err(FileSnapshotError::DuplicateSource);
        }
        paths.push(path);
    }
    if paths.is_empty() {
        return Err(FileSnapshotError::Empty);
    }
    Ok(paths)
}

/// Preflights and streams local regular files/directories into the chunk store.
///
/// Symlinks are rejected rather than preserved or followed. The preflight walk
/// is deterministic and bounded, and regular-file identity/size are checked
/// again after opening to catch concurrent replacement.
///
/// # Errors
///
/// Returns an error for unsafe paths, unsupported file types, mutation during
/// capture, limits, cancellation, chunk storage, or I/O.
pub fn snapshot_file_uris(
    paths: &[PathBuf],
    store: &mut ChunkStore,
    limits: FileSnapshotLimits,
    cancellation: &CancellationToken,
) -> Result<FileSnapshot, FileSnapshotError> {
    validate_limits(limits)?;
    if paths.is_empty() {
        return Err(FileSnapshotError::Empty);
    }
    let mut planned = Vec::new();
    let mut root_names = HashSet::new();
    let mut logical_size = 0_u64;

    for path in paths {
        ensure_not_cancelled(cancellation)?;
        validate_absolute_source_path(path)?;
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            return Err(FileSnapshotError::Symlink(path.clone()));
        }
        let root_name = safe_component(
            path.file_name()
                .ok_or_else(|| FileSnapshotError::UnsafePath(path.clone()))?,
        )?;
        if !root_names.insert(root_name.clone()) {
            return Err(FileSnapshotError::DuplicateRootName(root_name));
        }
        let canonical_root = fs::canonicalize(path)?;
        SnapshotPlanner {
            canonical_root: &canonical_root,
            limits,
            cancellation,
            planned: &mut planned,
            total: &mut logical_size,
        }
        .plan_entry(path, &root_name, 0)?;
    }
    planned.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));

    let mut entries = Vec::with_capacity(planned.len());
    for entry in planned {
        ensure_not_cancelled(cancellation)?;
        let blob = if entry.kind == SnapshotEntryKind::File {
            let mut file = open_revalidated_file(&entry)?;
            Some(store.stage_reader(&mut file, entry.logical_size, cancellation)?)
        } else {
            None
        };
        entries.push(FileSnapshotEntry {
            relative_path: entry.relative_path,
            kind: entry.kind,
            executable: entry.executable,
            logical_size: entry.logical_size,
            blob,
        });
    }

    let snapshot = FileSnapshot {
        logical_size,
        entries,
    };
    snapshot.validate(limits.max_logical_bytes)?;
    Ok(snapshot)
}

#[derive(Debug)]
struct PlannedEntry {
    source: PathBuf,
    relative_path: String,
    kind: SnapshotEntryKind,
    executable: bool,
    logical_size: u64,
    identity: FileIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

struct SnapshotPlanner<'a> {
    canonical_root: &'a Path,
    limits: FileSnapshotLimits,
    cancellation: &'a CancellationToken,
    planned: &'a mut Vec<PlannedEntry>,
    total: &'a mut u64,
}

impl SnapshotPlanner<'_> {
    fn plan_entry(
        &mut self,
        source: &Path,
        relative_path: &str,
        depth: usize,
    ) -> Result<(), FileSnapshotError> {
        ensure_not_cancelled(self.cancellation)?;
        if depth > self.limits.max_depth {
            return Err(FileSnapshotError::TooDeep {
                maximum: self.limits.max_depth,
            });
        }
        if self.planned.len() == self.limits.max_entries {
            return Err(FileSnapshotError::TooManyEntries {
                maximum: self.limits.max_entries,
            });
        }
        let metadata = fs::symlink_metadata(source)?;
        if metadata.file_type().is_symlink() {
            return Err(FileSnapshotError::Symlink(source.to_path_buf()));
        }
        let canonical = fs::canonicalize(source)?;
        if !canonical.starts_with(self.canonical_root) {
            return Err(FileSnapshotError::Traversal(source.to_path_buf()));
        }

        let (kind, logical_size) = if metadata.is_file() {
            *self.total = self
                .total
                .checked_add(metadata.len())
                .ok_or(FileSnapshotError::SizeOverflow)?;
            if *self.total > self.limits.max_logical_bytes {
                return Err(FileSnapshotError::TooLarge {
                    observed: *self.total,
                    maximum: self.limits.max_logical_bytes,
                });
            }
            (SnapshotEntryKind::File, metadata.len())
        } else if metadata.is_dir() {
            (SnapshotEntryKind::Directory, 0)
        } else {
            return Err(FileSnapshotError::UnsupportedFileType(source.to_path_buf()));
        };
        self.planned.push(PlannedEntry {
            source: source.to_path_buf(),
            relative_path: relative_path.to_owned(),
            kind,
            executable: is_executable(&metadata),
            logical_size,
            identity: file_identity(&metadata),
        });

        if kind == SnapshotEntryKind::Directory {
            let mut children = Vec::new();
            for child in fs::read_dir(source)? {
                ensure_not_cancelled(self.cancellation)?;
                let child = child?;
                let name = safe_component(&child.file_name())?;
                children.push((name, child.path()));
                if self.planned.len().saturating_add(children.len()) > self.limits.max_entries {
                    return Err(FileSnapshotError::TooManyEntries {
                        maximum: self.limits.max_entries,
                    });
                }
            }
            children.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            for (name, child) in children {
                let child_relative = format!("{relative_path}/{name}");
                self.plan_entry(&child, &child_relative, depth + 1)?;
            }
        }
        Ok(())
    }
}

fn open_revalidated_file(entry: &PlannedEntry) -> Result<File, FileSnapshotError> {
    let before = fs::symlink_metadata(&entry.source)?;
    if before.file_type().is_symlink() || !before.is_file() {
        return Err(FileSnapshotError::SourceChanged(entry.source.clone()));
    }
    let file = File::open(&entry.source)?;
    let after = file.metadata()?;
    if !after.is_file()
        || after.len() != entry.logical_size
        || file_identity(&before) != entry.identity
        || file_identity(&after) != entry.identity
    {
        return Err(FileSnapshotError::SourceChanged(entry.source.clone()));
    }
    Ok(file)
}

fn validate_limits(limits: FileSnapshotLimits) -> Result<(), FileSnapshotError> {
    if limits.max_entries == 0
        || limits.max_depth == 0
        || limits.max_uri_list_bytes == 0
        || limits.max_logical_bytes == 0
    {
        return Err(FileSnapshotError::InvalidLimits);
    }
    Ok(())
}

fn validate_absolute_source_path(path: &Path) -> Result<(), FileSnapshotError> {
    if !path.is_absolute() {
        return Err(FileSnapshotError::UnsafePath(path.to_path_buf()));
    }
    for component in path.components() {
        if matches!(component, Component::ParentDir | Component::CurDir) {
            return Err(FileSnapshotError::Traversal(path.to_path_buf()));
        }
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<(), FileSnapshotError> {
    if path.is_empty()
        || path.len() > MAX_RELATIVE_PATH_BYTES
        || path.starts_with('/')
        || path.contains('\0')
    {
        return Err(FileSnapshotError::UnsafeManifestPath);
    }
    for component in path.split('/') {
        if component.is_empty()
            || component.len() > MAX_PATH_COMPONENT_BYTES
            || matches!(component, "." | "..")
        {
            return Err(FileSnapshotError::UnsafeManifestPath);
        }
    }
    Ok(())
}

fn safe_component(component: &std::ffi::OsStr) -> Result<String, FileSnapshotError> {
    let component = component
        .to_str()
        .ok_or(FileSnapshotError::NonUtf8FileName)?;
    if component.is_empty()
        || component.len() > MAX_PATH_COMPONENT_BYTES
        || matches!(component, "." | "..")
        || component.contains(['/', '\0'])
    {
        return Err(FileSnapshotError::UnsafeManifestPath);
    }
    Ok(component.to_owned())
}

#[cfg(unix)]
fn is_executable(metadata: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.is_file() && metadata.permissions().mode() & 0o100 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &Metadata) -> bool {
    false
}

#[cfg(unix)]
fn file_identity(metadata: &Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(not(unix))]
fn file_identity(_metadata: &Metadata) -> FileIdentity {
    FileIdentity {}
}

fn ensure_not_cancelled(cancellation: &CancellationToken) -> Result<(), FileSnapshotError> {
    if cancellation.is_cancelled() {
        Err(FileSnapshotError::Cancelled)
    } else {
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum FileSnapshotError {
    #[error("file snapshot is empty")]
    Empty,
    #[error("file snapshot limits must be nonzero")]
    InvalidLimits,
    #[error("URI list is {observed} bytes, exceeding its {maximum}-byte limit")]
    UriListTooLarge { observed: usize, maximum: usize },
    #[error("file URI list is not valid UTF-8")]
    InvalidUriListUtf8,
    #[error("invalid file URI")]
    InvalidFileUri,
    #[error("only file:// URIs can be snapshotted")]
    UnsupportedUriScheme,
    #[error("remote file URI authorities are not accepted")]
    RemoteFileUri,
    #[error("unsafe source path: {0:?}")]
    UnsafePath(PathBuf),
    #[error("path traversal is not allowed: {0:?}")]
    Traversal(PathBuf),
    #[error("duplicate source path")]
    DuplicateSource,
    #[error("two snapshot roots use the same safe name {0:?}")]
    DuplicateRootName(String),
    #[error("symlinks are not captured: {0:?}")]
    Symlink(PathBuf),
    #[error("unsupported non-file/non-directory entry: {0:?}")]
    UnsupportedFileType(PathBuf),
    #[error("non-UTF-8 filenames cannot be represented safely")]
    NonUtf8FileName,
    #[error("unsafe relative path in snapshot manifest")]
    UnsafeManifestPath,
    #[error("snapshot manifest is not canonical")]
    NonCanonicalManifest,
    #[error("snapshot exceeds the {maximum}-entry limit")]
    TooManyEntries { maximum: usize },
    #[error("snapshot exceeds the {maximum}-level depth limit")]
    TooDeep { maximum: usize },
    #[error("snapshot reached {observed} bytes, exceeding its {maximum}-byte limit")]
    TooLarge { observed: u64, maximum: u64 },
    #[error("snapshot size overflow")]
    SizeOverflow,
    #[error("source changed while it was being snapshotted: {0:?}")]
    SourceChanged(PathBuf),
    #[error("snapshot was cancelled")]
    Cancelled,
    #[error(transparent)]
    ChunkStore(#[from] ChunkStoreError),
    #[error(transparent)]
    Io(#[from] io::Error),
}
