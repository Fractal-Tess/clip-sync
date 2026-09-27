//! Copied files: turning the clipboard's file list into a [`Reference`],
//! reopening the originals safely to serve them, and writing received files
//! safely on the other side.
//!
//! The origin never copies the files. It remembers where each one is and
//! what it looked like when copied (device, inode, size, modification time),
//! and refuses to serve a file that has since moved or changed.

use std::{
    collections::HashSet,
    fs::{self, File, Metadata, OpenOptions},
    io,
    path::{Component, Path, PathBuf},
};

use thiserror::Error;
use url::Url;

use crate::model::{FileEntry, MAX_REFERENCE_ENTRIES, Reference, validate_relative_path};

const MAX_DEPTH: usize = 64;
pub const MAX_URI_LIST_BYTES: usize = 1024 * 1024;
const MAX_PATH_COMPONENT_BYTES: usize = 255;

/// Where one entry of a reference authored here lives, and what it looked
/// like when it was copied. Directories carry no identity: only their
/// listed contents are served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFile {
    pub path: PathBuf,
    pub identity: Option<FileIdentity>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub modified_nanos: i128,
}

/// Parses `text/uri-list` bytes into absolute local paths.
///
/// # Errors
///
/// Rejects remote or non-file URIs, malformed UTF-8, relative paths,
/// traversal, duplicates, oversized input, or an empty list.
pub fn parse_file_uri_list(bytes: &[u8]) -> Result<Vec<PathBuf>, FilesError> {
    if bytes.len() > MAX_URI_LIST_BYTES {
        return Err(FilesError::UriListTooLarge);
    }
    let source = std::str::from_utf8(bytes).map_err(|_| FilesError::InvalidUri)?;
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
        if paths.len() == MAX_REFERENCE_ENTRIES {
            return Err(FilesError::TooManyEntries);
        }
        let url = Url::parse(line).map_err(|_| FilesError::InvalidUri)?;
        if url.scheme() != "file"
            || url
                .host_str()
                .is_some_and(|host| !host.is_empty() && host != "localhost")
        {
            return Err(FilesError::NotLocal);
        }
        let path = url.to_file_path().map_err(|()| FilesError::InvalidUri)?;
        validate_absolute_path(&path)?;
        if !unique.insert(path.clone()) {
            return Err(FilesError::Duplicate);
        }
        paths.push(path);
    }
    if paths.is_empty() {
        return Err(FilesError::Empty);
    }
    Ok(paths)
}

/// Walks the copied roots without reading any file contents.
///
/// Returns the reference peers will see and, in the same order, where each
/// entry lives here. Symlinks and special files are rejected rather than
/// followed or guessed at.
///
/// # Errors
///
/// Returns an error for unsafe paths, symlinks, unsupported file types,
/// name collisions between roots, excessive entries or depth, or I/O.
pub fn describe_files(paths: &[PathBuf]) -> Result<(Reference, Vec<SourceFile>), FilesError> {
    if paths.is_empty() {
        return Err(FilesError::Empty);
    }
    let mut planned = Vec::new();
    let mut root_names = HashSet::new();
    for path in paths {
        validate_absolute_path(path)?;
        let root_name = safe_component(
            path.file_name()
                .ok_or_else(|| FilesError::UnsafePath(path.clone()))?,
        )?;
        if !root_names.insert(root_name.clone()) {
            return Err(FilesError::DuplicateRootName(root_name));
        }
        let canonical_root = fs::canonicalize(path)?;
        plan_entry(path, &root_name, 0, &canonical_root, &mut planned)?;
    }
    planned.sort_unstable_by(|left, right| left.0.path.cmp(&right.0.path));
    let (entries, sources) = planned.into_iter().unzip();
    let reference = Reference::Files(entries);
    reference
        .validate()
        .map_err(|_| FilesError::UnsafePath(PathBuf::new()))?;
    Ok((reference, sources))
}

fn plan_entry(
    source: &Path,
    relative_path: &str,
    depth: usize,
    canonical_root: &Path,
    planned: &mut Vec<(FileEntry, SourceFile)>,
) -> Result<(), FilesError> {
    if depth > MAX_DEPTH {
        return Err(FilesError::TooDeep);
    }
    if planned.len() == MAX_REFERENCE_ENTRIES {
        return Err(FilesError::TooManyEntries);
    }
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return Err(FilesError::Symlink(source.to_path_buf()));
    }
    if !fs::canonicalize(source)?.starts_with(canonical_root) {
        return Err(FilesError::UnsafePath(source.to_path_buf()));
    }

    if metadata.is_file() {
        planned.push((
            FileEntry {
                path: relative_path.to_owned(),
                directory: false,
                executable: is_executable(&metadata),
                size: metadata.len(),
            },
            SourceFile {
                path: source.to_path_buf(),
                identity: Some(identity(&metadata)),
            },
        ));
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(FilesError::UnsupportedFileType(source.to_path_buf()));
    }
    planned.push((
        FileEntry {
            path: relative_path.to_owned(),
            directory: true,
            executable: false,
            size: 0,
        },
        SourceFile {
            path: source.to_path_buf(),
            identity: None,
        },
    ));
    let mut children = Vec::new();
    for child in fs::read_dir(source)? {
        let child = child?;
        children.push((safe_component(&child.file_name())?, child.path()));
        if planned.len().saturating_add(children.len()) > MAX_REFERENCE_ENTRIES {
            return Err(FilesError::TooManyEntries);
        }
    }
    children.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    for (name, child) in children {
        plan_entry(
            &child,
            &format!("{relative_path}/{name}"),
            depth + 1,
            canonical_root,
            planned,
        )?;
    }
    Ok(())
}

/// Reopens a copied file to serve it, refusing if it moved or changed since
/// it was copied. The identity is checked on the path before opening and on
/// the open handle, so a swap between the two is caught.
///
/// # Errors
///
/// Returns [`FilesError::SourceChanged`] when the file no longer matches.
pub fn open_source(source: &SourceFile) -> Result<File, FilesError> {
    let changed = || FilesError::SourceChanged(source.path.clone());
    let expected = source.identity.ok_or_else(changed)?;
    let before = fs::symlink_metadata(&source.path).map_err(|_| changed())?;
    if before.file_type().is_symlink() || !before.is_file() || identity(&before) != expected {
        return Err(changed());
    }
    let file = File::open(&source.path).map_err(|_| changed())?;
    let after = file.metadata()?;
    if !after.is_file() || identity(&after) != expected {
        return Err(changed());
    }
    Ok(file)
}

/// Joins a received relative path under `root`, rejecting anything that
/// could escape it.
///
/// # Errors
///
/// Returns [`FilesError::UnsafeRelativePath`] for an unsafe path.
pub fn destination(root: &Path, relative: &str) -> Result<PathBuf, FilesError> {
    validate_relative_path(relative).map_err(|_| FilesError::UnsafeRelativePath)?;
    let mut path = root.to_path_buf();
    path.extend(relative.split('/'));
    Ok(path)
}

/// Creates a received file that must not already exist, owner-only.
///
/// # Errors
///
/// Returns the I/O error, including when the path already exists.
pub fn create_received_file(path: &Path, executable: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    #[cfg(not(unix))]
    let _ = executable;
    options.open(path)
}

/// Creates a received directory, owner-only.
///
/// # Errors
///
/// Returns the I/O error, including when the path already exists.
pub fn create_received_directory(path: &Path) -> io::Result<()> {
    fs::create_dir(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// `text/uri-list` bytes naming the top-level entries of a reference that
/// sit under `root`, as file managers expect on the clipboard.
///
/// # Errors
///
/// Returns an error when a path cannot be expressed as a `file://` URI.
pub fn uri_list(root: &Path, entries: &[FileEntry]) -> Result<Vec<u8>, FilesError> {
    let paths = entries
        .iter()
        .filter(|entry| !entry.path.contains('/'))
        .map(|entry| Ok((destination(root, &entry.path)?, entry.directory)))
        .collect::<Result<Vec<_>, FilesError>>()?;
    uri_list_for_paths(
        paths
            .iter()
            .map(|(path, directory)| (path.as_path(), *directory)),
    )
}

/// `text/uri-list` bytes for absolute paths.
///
/// # Errors
///
/// Returns an error when a path cannot be expressed as a `file://` URI.
pub fn uri_list_for_paths<'a>(
    paths: impl IntoIterator<Item = (&'a Path, bool)>,
) -> Result<Vec<u8>, FilesError> {
    let mut list = Vec::new();
    for (path, directory) in paths {
        let url = if directory {
            Url::from_directory_path(path)
        } else {
            Url::from_file_path(path)
        }
        .map_err(|()| FilesError::UnsafePath(path.to_path_buf()))?;
        list.extend_from_slice(url.as_str().as_bytes());
        list.extend_from_slice(b"\r\n");
    }
    if list.is_empty() {
        return Err(FilesError::Empty);
    }
    Ok(list)
}

fn validate_absolute_path(path: &Path) -> Result<(), FilesError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(FilesError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

fn safe_component(component: &std::ffi::OsStr) -> Result<String, FilesError> {
    let component = component.to_str().ok_or(FilesError::NonUtf8FileName)?;
    if component.is_empty()
        || component.len() > MAX_PATH_COMPONENT_BYTES
        || matches!(component, "." | "..")
        || component.contains(['/', '\0'])
    {
        return Err(FilesError::UnsafeRelativePath);
    }
    Ok(component.to_owned())
}

#[cfg(unix)]
fn is_executable(metadata: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o100 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &Metadata) -> bool {
    false
}

#[cfg(unix)]
fn identity(metadata: &Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified_nanos: i128::from(metadata.mtime()) * 1_000_000_000
            + i128::from(metadata.mtime_nsec()),
    }
}

#[cfg(not(unix))]
fn identity(metadata: &Metadata) -> FileIdentity {
    FileIdentity {
        device: 0,
        inode: 0,
        size: metadata.len(),
        modified_nanos: 0,
    }
}

#[derive(Debug, Error)]
pub enum FilesError {
    #[error("no files were copied")]
    Empty,
    #[error("the copied file list is too large")]
    UriListTooLarge,
    #[error("the copied file list contains an invalid URI")]
    InvalidUri,
    #[error("only local file:// URIs can be copied")]
    NotLocal,
    #[error("the same file was listed twice")]
    Duplicate,
    #[error("two copied roots share the name {0:?}")]
    DuplicateRootName(String),
    #[error("unsafe path {0:?}")]
    UnsafePath(PathBuf),
    #[error("unsafe relative path")]
    UnsafeRelativePath,
    #[error("symlinks are not copied: {0:?}")]
    Symlink(PathBuf),
    #[error("only regular files and directories are copied: {0:?}")]
    UnsupportedFileType(PathBuf),
    #[error("file names must be UTF-8")]
    NonUtf8FileName,
    #[error("more than {MAX_REFERENCE_ENTRIES} files were copied")]
    TooManyEntries,
    #[error("the copied directories are nested more than {MAX_DEPTH} levels deep")]
    TooDeep,
    #[error("{0:?} moved or changed since it was copied")]
    SourceChanged(PathBuf),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let album = directory.path().join("album");
        fs::create_dir(&album).unwrap();
        fs::write(album.join("b.jpg"), b"bbbb").unwrap();
        fs::write(album.join("a.jpg"), b"aa").unwrap();
        fs::write(directory.path().join("notes.txt"), b"n").unwrap();
        directory
    }

    #[test]
    fn describes_directories_before_their_files_in_order() {
        let directory = tree();
        let paths = vec![
            directory.path().join("notes.txt"),
            directory.path().join("album"),
        ];
        let (reference, sources) = describe_files(&paths).unwrap();
        let Reference::Files(entries) = &reference else {
            panic!("expected files");
        };
        let names = entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["album", "album/a.jpg", "album/b.jpg", "notes.txt"]);
        assert_eq!(reference.logical_size(), 7);
        assert_eq!(sources[1].path, directory.path().join("album/a.jpg"));
        assert!(sources[0].identity.is_none());
    }

    #[test]
    fn a_changed_file_is_not_served() {
        let directory = tree();
        let path = directory.path().join("notes.txt");
        let (_, sources) = describe_files(std::slice::from_ref(&path)).unwrap();
        assert!(open_source(&sources[0]).is_ok());

        fs::write(&path, b"now longer").unwrap();
        assert!(matches!(
            open_source(&sources[0]),
            Err(FilesError::SourceChanged(_))
        ));
    }

    #[test]
    fn symlinks_are_rejected() {
        let directory = tree();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(directory.path().join("notes.txt"), &link).unwrap();
        assert!(matches!(
            describe_files(&[link]),
            Err(FilesError::Symlink(_))
        ));
    }

    #[test]
    fn uri_lists_round_trip_and_reject_remote_hosts() {
        let directory = tree();
        let paths = vec![directory.path().join("notes.txt")];
        let list = uri_list_for_paths(paths.iter().map(|path| (path.as_path(), false))).unwrap();
        assert_eq!(parse_file_uri_list(&list).unwrap(), paths);
        assert!(matches!(
            parse_file_uri_list(b"file://elsewhere/etc/passwd"),
            Err(FilesError::NotLocal)
        ));
    }

    #[test]
    fn destinations_cannot_escape_their_root() {
        let root = Path::new("/cache/item");
        assert_eq!(
            destination(root, "album/a.jpg").unwrap(),
            Path::new("/cache/item/album/a.jpg")
        );
        for unsafe_path in ["../x", "/etc/passwd", "a/../../x", ""] {
            assert!(destination(root, unsafe_path).is_err(), "{unsafe_path:?}");
        }
    }
}
