//! Large items whose bytes stay on the device that copied them.
//!
//! A reference replicates like any other history entry, but carries only a
//! description. Another device fetches the bytes from the origin when the
//! item is activated, so large copies never fan out to every host.

use std::collections::BTreeSet;

use thiserror::Error;

use super::RepresentationDescriptor;

pub const MAX_REFERENCE_ENTRIES: usize = 100_000;
const MAX_RELATIVE_PATH_BYTES: usize = 4096;
const MAX_PATH_COMPONENT_BYTES: usize = 255;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reference {
    /// Files and directories as they sit on the origin, listed by path
    /// relative to the copied roots, parents before children.
    Files(Vec<FileEntry>),
    /// Non-file clipboard content too large to replicate inline, kept by the
    /// origin in its local store.
    Data(Vec<RepresentationDescriptor>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub directory: bool,
    pub executable: bool,
    pub size: u64,
}

impl Reference {
    #[must_use]
    pub fn logical_size(&self) -> u64 {
        match self {
            Self::Files(entries) => entries
                .iter()
                .fold(0_u64, |total, entry| total.saturating_add(entry.size)),
            Self::Data(representations) => {
                representations.iter().fold(0_u64, |total, representation| {
                    total.saturating_add(representation.byte_len())
                })
            }
        }
    }

    /// MIME types the item offers once it is on the clipboard.
    #[must_use]
    pub fn mime_types(&self) -> Vec<String> {
        match self {
            Self::Files(_) => vec!["text/uri-list".to_owned()],
            Self::Data(representations) => representations
                .iter()
                .map(|representation| representation.mime().to_owned())
                .collect(),
        }
    }

    /// Checks invariants every receiver relies on before touching its disk:
    /// safe relative paths in canonical order, parents listed before their
    /// children, and empty directories.
    ///
    /// # Errors
    ///
    /// Returns the first violated invariant.
    pub fn validate(&self) -> Result<(), ReferenceError> {
        match self {
            Self::Files(entries) => validate_entries(entries),
            Self::Data(representations) => {
                if representations.is_empty() {
                    return Err(ReferenceError::Empty);
                }
                if representations
                    .windows(2)
                    .any(|pair| pair[0].mime().as_bytes() >= pair[1].mime().as_bytes())
                {
                    return Err(ReferenceError::NonCanonical);
                }
                Ok(())
            }
        }
    }
}

fn validate_entries(entries: &[FileEntry]) -> Result<(), ReferenceError> {
    if entries.is_empty() {
        return Err(ReferenceError::Empty);
    }
    if entries.len() > MAX_REFERENCE_ENTRIES {
        return Err(ReferenceError::TooManyEntries);
    }
    let mut directories = BTreeSet::new();
    let mut prior: Option<&str> = None;
    for entry in entries {
        validate_relative_path(&entry.path)?;
        if prior.is_some_and(|prior| prior >= entry.path.as_str()) {
            return Err(ReferenceError::NonCanonical);
        }
        prior = Some(&entry.path);
        if let Some((parent, _)) = entry.path.rsplit_once('/')
            && !directories.contains(parent)
        {
            return Err(ReferenceError::NonCanonical);
        }
        if entry.directory {
            if entry.size != 0 || entry.executable {
                return Err(ReferenceError::NonCanonical);
            }
            directories.insert(entry.path.as_str());
        }
    }
    Ok(())
}

/// Accepts only '/'-separated relative paths without empty, `.`, or `..`
/// components, so a receiver can join them under its own directory safely.
///
/// # Errors
///
/// Returns [`ReferenceError::UnsafePath`] for anything else.
pub fn validate_relative_path(path: &str) -> Result<(), ReferenceError> {
    if path.is_empty()
        || path.len() > MAX_RELATIVE_PATH_BYTES
        || path.starts_with('/')
        || path.contains('\0')
    {
        return Err(ReferenceError::UnsafePath);
    }
    for component in path.split('/') {
        if component.is_empty()
            || component.len() > MAX_PATH_COMPONENT_BYTES
            || matches!(component, "." | "..")
        {
            return Err(ReferenceError::UnsafePath);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ReferenceError {
    #[error("a reference must describe at least one entry")]
    Empty,
    #[error("a reference lists more than {MAX_REFERENCE_ENTRIES} entries")]
    TooManyEntries,
    #[error("a reference path is not a safe relative path")]
    UnsafePath,
    #[error("reference entries are not in canonical order")]
    NonCanonical,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64) -> FileEntry {
        FileEntry {
            path: path.to_owned(),
            directory: false,
            executable: false,
            size,
        }
    }

    fn directory(path: &str) -> FileEntry {
        FileEntry {
            path: path.to_owned(),
            directory: true,
            executable: false,
            size: 0,
        }
    }

    #[test]
    fn nested_files_after_their_directory_are_valid() {
        let reference = Reference::Files(vec![
            directory("photos"),
            file("photos/a.jpg", 3),
            file("photos/b.jpg", 4),
            file("readme.txt", 5),
        ]);
        assert_eq!(reference.validate(), Ok(()));
        assert_eq!(reference.logical_size(), 12);
    }

    #[test]
    fn traversal_and_absolute_paths_are_rejected() {
        for path in ["../etc/passwd", "/etc/passwd", "a//b", "a/./b", ""] {
            assert_eq!(
                Reference::Files(vec![file(path, 1)]).validate(),
                Err(ReferenceError::UnsafePath),
                "{path:?}"
            );
        }
    }

    #[test]
    fn a_child_without_its_directory_is_rejected() {
        assert_eq!(
            Reference::Files(vec![file("photos/a.jpg", 1)]).validate(),
            Err(ReferenceError::NonCanonical)
        );
    }
}
